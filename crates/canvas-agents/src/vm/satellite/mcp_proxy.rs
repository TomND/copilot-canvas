//! The guest side of the MCP gateway (plan VM-37; docs/VM-SESSIONS.md §7):
//! an MCP server (Streamable HTTP) on the guest's loopback,
//! `/mcp/<term>/<server>`, whose sessions go to the hub as streams on the
//! link (`SatUp::Mcp`); the hub's answers come back through
//! [`McpProxy::down`]. Its port is in `HelloInfo::mcp_port`.
//!
//! - `POST`: one JSON-RPC message (a batch only where the session's protocol
//!   version has them, 2025-03-26: it is split into single messages, the
//!   link carries one at a time). Notifications and responses: `202`.
//!   Requests: the answer as JSON, or as `text/event-stream` when the hub
//!   sends the session something else first (progress, a server's request)
//!   or the answer takes long (then keep-alive comments).
//! - `GET`: the session's server-to-client stream (SSE); what comes with no
//!   request waiting and no `GET` open is kept (a little) for the next one.
//! - `DELETE`: ends the session (`MCP_CLOSE`).
//! - The first `POST` (`initialize`, without `Mcp-Session-Id`) opens a
//!   session: a new stream (`MCP_OPEN`), its id in `Mcp-Session-Id`. A
//!   session the hub refuses answers `403` with the hub's reason.
//! - Only `127.0.0.1`, `localhost` or `[::1]` with this port in `Host` and
//!   `Origin` (DNS rebinding: `403`); heads, bodies, connections, sessions
//!   and waits are bounded.
//! - The link down: `503`; a session it, the hub or a `DELETE` ended: `404`
//!   (MCP clients then start a new session).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use canvas_common::sat::{MCP_CLOSE, MCP_ERROR, MCP_MESSAGE, MCP_OPEN, McpOpen, SAT_FILE_CHUNK, SatUp};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::Value;

use crate::vm::mcp::jsonrpc::{self, Kind};

/// A request's head (request line and headers) is at most this long.
const MAX_HEAD: usize = 16 << 10;
const MAX_CONNECTIONS: usize = 64;
/// Sessions open at once (the hub's link allows 32 streams); past it the least used ends.
const MAX_SESSIONS: usize = 32;
/// A request's head and body must arrive within this long; a response must be taken within it.
const READ_WAIT: Duration = Duration::from_secs(30);
/// An answer waited for longer than this goes out as a stream (with keep-alives).
const JSON_WAIT: Duration = Duration::from_secs(20);
const KEEPALIVE: Duration = Duration::from_secs(15);
/// The longest a request waits for its answer.
const ANSWER_WAIT: Duration = Duration::from_secs(30 * 60);
/// Messages of the server's kept for the next `GET` stream.
const BACKLOG: usize = 64;
/// Messages queued for one response stream; past it the next stream takes them.
const OUTLET_QUEUE: usize = 256;
/// Ended sessions remembered (their requests answer `404`).
const ENDED: usize = 1024;

/// The proxy's link to the hub may take a message within this long.
const UP_WAIT: Duration = Duration::from_secs(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct McpProxy {
    port: u16,
    up: Mutex<Option<Sender<SatUp>>>,
    state: Mutex<State>,
    streams: AtomicU64,
    outlets: AtomicU64,
    connections: AtomicUsize,
}

#[derive(Default)]
struct State {
    /// Sessions by their id.
    sessions: HashMap<String, Arc<Session>>,
    /// Stream → session id.
    streams: HashMap<u64, String>,
    ended: VecDeque<String>,
}

/// One guest MCP session: a stream on the link.
struct Session {
    id: String,
    stream: u64,
    term: String,
    server: String,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The protocol version the session speaks (from its initialize answer).
    version: Option<String>,
    /// Requests waiting for their answer: id (as a key) → the response that waits.
    waiting: HashMap<String, Sender<Event>>,
    /// Response streams that take the server's own messages, newest last.
    outlets: Vec<(u64, Sender<Event>)>,
    get: Option<(u64, Sender<Event>)>,
    backlog: VecDeque<Value>,
    /// The hub's reason, when it refused or ended the stream.
    error: Option<String>,
    used: Instant,
}

enum Event {
    /// A message of the server's own (a request or a notification).
    Message(Value),
    /// The answer to one of the response's requests.
    Answer(Value),
    /// The guest cancelled one of the response's requests (its id as a key).
    Cancelled(String),
    /// The session ended (`link`: because the link dropped).
    Ended { why: String, link: bool },
}

impl McpProxy {
    /// Starts serving on the guest's loopback (`port` 0: any free port).
    pub fn start(port: u16) -> Result<Arc<McpProxy>> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        // Stream ids go on from the time: a restarted satellite never reuses one the hub may still hold.
        let first = canvas_common::paths::now_ms().saturating_mul(1024);
        let proxy = Arc::new(McpProxy { port, up: Mutex::default(), state: Mutex::default(), streams: AtomicU64::new(first), outlets: AtomicU64::new(0), connections: AtomicUsize::new(0) });
        let weak = Arc::downgrade(&proxy);
        std::thread::Builder::new().name("mcp-proxy".into()).spawn(move || {
            for connection in listener.incoming() {
                let Some(proxy) = weak.upgrade() else { return };
                let Ok(mut connection) = connection else { continue };
                if proxy.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    proxy.connections.fetch_sub(1, Ordering::SeqCst);
                    let _ = connection.set_write_timeout(Some(Duration::from_secs(2)));
                    refuse(&mut connection, 503, "too many connections to the MCP proxy");
                    continue;
                }
                let serving = proxy.clone();
                let spawned = std::thread::Builder::new().name("mcp-proxy-conn".into()).spawn(move || {
                    serving.serve(connection);
                    serving.connections.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    proxy.connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })?;
        Ok(proxy)
    }

    /// The port it serves on (0: none).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The link its streams go up on now (`None`: no link; every stream has ended).
    pub fn attach(&self, up: Option<Sender<SatUp>>) {
        *lock(&self.up) = up;
    }

    fn linked(&self) -> bool {
        lock(&self.up).is_some()
    }

    /// Sends one stream message to the hub (`false`: no link, or it does not take it).
    fn send_up(&self, stream: u64, kind: u8, bytes: Vec<u8>) -> bool {
        let up = lock(&self.up);
        up.as_ref().is_some_and(|tx| tx.send_timeout(SatUp::Mcp { stream, kind, bytes }, UP_WAIT).is_ok())
    }

    /// One message from the hub for stream `stream`.
    pub fn down(&self, stream: u64, kind: u8, bytes: Vec<u8>) {
        let session = {
            let state = lock(&self.state);
            state.streams.get(&stream).and_then(|id| state.sessions.get(id)).cloned()
        };
        let Some(session) = session else { return };
        match kind {
            MCP_MESSAGE => {
                if let Ok(message) = serde_json::from_slice::<Value>(&bytes) {
                    route(&session, message);
                }
            }
            MCP_ERROR => {
                let why = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| "refused by the hub".into());
                lock(&session.inner).error = Some(why);
            }
            MCP_CLOSE => {
                let why = lock(&session.inner).error.clone().unwrap_or_else(|| "the hub ended the MCP session".into());
                self.end(&session, &why, false);
            }
            _ => {}
        }
    }

    /// The link dropped: every stream ends (the guest's MCP clients start new sessions).
    pub fn link_lost(&self) {
        let sessions: Vec<Arc<Session>> = lock(&self.state).sessions.values().cloned().collect();
        for session in sessions {
            self.end(&session, "the VM's link to the hub dropped", true);
        }
    }

    /// Ends `session` here: its waiting requests are answered with `why`;
    /// its id answers `404` from now on.
    fn end(&self, session: &Arc<Session>, why: &str, link: bool) {
        {
            let mut state = lock(&self.state);
            if state.sessions.remove(&session.id).is_some() {
                state.streams.remove(&session.stream);
                state.ended.push_back(session.id.clone());
                if state.ended.len() > ENDED {
                    state.ended.pop_front();
                }
            }
        }
        let mut inner = lock(&session.inner);
        let ended = || Event::Ended { why: why.to_owned(), link };
        for (_, tx) in inner.waiting.drain() {
            let _ = tx.send(ended());
        }
        for (_, tx) in inner.outlets.drain(..) {
            let _ = tx.send(ended());
        }
        if let Some((_, tx)) = inner.get.take() {
            let _ = tx.send(ended());
        }
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.state).sessions.get(id).cloned()
    }

    /// A new session of `term` with `server`; the least used one ends when there are too many.
    fn new_session(&self, term: &str, server: &str) -> Arc<Session> {
        let session = Arc::new(Session {
            id: uuid::Uuid::new_v4().simple().to_string(),
            stream: self.streams.fetch_add(1, Ordering::Relaxed),
            term: term.to_owned(),
            server: server.to_owned(),
            inner: Mutex::new(Inner { version: None, waiting: HashMap::new(), outlets: Vec::new(), get: None, backlog: VecDeque::new(), error: None, used: Instant::now() }),
        });
        let evicted = {
            let mut state = lock(&self.state);
            let evicted = if state.sessions.len() >= MAX_SESSIONS { state.sessions.values().min_by_key(|s| lock(&s.inner).used).cloned() } else { None };
            state.sessions.insert(session.id.clone(), session.clone());
            state.streams.insert(session.stream, session.id.clone());
            evicted
        };
        if let Some(old) = evicted {
            self.end(&old, "too many MCP sessions: the least used one ended", false);
            self.send_up(old.stream, MCP_CLOSE, Vec::new());
        }
        session
    }
}

/// A message of the hub's for `session`: an answer goes to the response
/// waiting for it; anything else to the newest response stream that takes
/// it, else the `GET` stream, else the backlog.
fn route(session: &Session, message: Value) {
    let mut inner = lock(&session.inner);
    inner.used = Instant::now();
    match jsonrpc::classify(&message) {
        Ok(Kind::Response { id }) => {
            if let Some(tx) = inner.waiting.remove(&jsonrpc::key(&id)) {
                stop_taking(&mut inner, &tx);
                let _ = tx.send(Event::Answer(message));
            }
        }
        Ok(_) => {
            let mut message = message;
            while let Some((_, tx)) = inner.outlets.last() {
                if tx.len() < OUTLET_QUEUE {
                    match tx.send(Event::Message(message)) {
                        Ok(()) => return,
                        Err(crossbeam_channel::SendError(event)) => message = event.into_message(),
                    }
                }
                inner.outlets.pop();
            }
            if let Some((_, tx)) = &inner.get {
                if tx.len() < OUTLET_QUEUE {
                    match tx.send(Event::Message(message)) {
                        Ok(()) => return,
                        Err(crossbeam_channel::SendError(event)) => message = event.into_message(),
                    }
                }
                inner.get = None;
            }
            inner.backlog.push_back(message);
            if inner.backlog.len() > BACKLOG {
                inner.backlog.pop_front();
            }
        }
        Err(_) => {}
    }
}

/// A response with nothing more to wait for takes no more of the server's
/// messages (they go to the next stream rather than to one that ends).
fn stop_taking(inner: &mut Inner, tx: &Sender<Event>) {
    if !inner.waiting.values().any(|t| t.same_channel(tx)) {
        inner.outlets.retain(|(_, t)| !t.same_channel(tx));
    }
}

impl Event {
    fn into_message(self) -> Value {
        match self {
            Event::Message(message) | Event::Answer(message) => message,
            _ => Value::Null,
        }
    }
}

/// A request as read off a connection.
struct Request {
    method: String,
    path: String,
    /// Names in lower case.
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Writes a whole response and ends it (`Connection: close`).
fn respond(out: &mut TcpStream, status: u16, headers: &[(&str, &str)], content_type: Option<&str>, body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n", reason(status), body.len());
    if let Some(content_type) = content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = out.write_all(head.as_bytes()).and_then(|_| out.write_all(body)).and_then(|_| out.flush());
}

fn respond_json(out: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &Value) {
    respond(out, status, headers, Some("application/json"), body.to_string().as_bytes());
}

/// An HTTP refusal with a JSON-RPC error (without an id) saying why.
fn refuse(out: &mut TcpStream, status: u16, why: &str) {
    let code = if status == 400 { jsonrpc::INVALID_REQUEST } else { jsonrpc::SESSION_ENDED };
    respond_json(out, status, &[], &jsonrpc::error(Value::Null, code, why, None));
}

/// Starts an event stream; `false`: the client is gone.
fn start_events(out: &mut TcpStream, session: Option<&str>) -> bool {
    let mut head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n".to_owned();
    if let Some(session) = session {
        head.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes()).and_then(|_| out.flush()).is_ok()
}

/// One message as an event; `false`: the client is gone.
fn event(out: &mut TcpStream, message: &Value) -> bool {
    out.write_all(format!("event: message\ndata: {message}\n\n").as_bytes()).and_then(|_| out.flush()).is_ok()
}

fn keepalive(out: &mut TcpStream) -> bool {
    out.write_all(b": keep-alive\n\n").and_then(|_| out.flush()).is_ok()
}

/// One line of the head, within what is left of `budget` (`None`: too long, cut or not text).
fn head_line(reader: &mut BufReader<TcpStream>, budget: &mut usize) -> Option<String> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(*budget as u64 + 1).read_until(b'\n', &mut line).ok()?;
    if read == 0 || read > *budget || !line.ends_with(b"\n") {
        return None;
    }
    *budget -= read;
    String::from_utf8(line).ok().map(|l| l.trim_end_matches(['\r', '\n']).to_owned())
}

/// The request line and headers (`Err`: the status to answer).
fn read_head(reader: &mut BufReader<TcpStream>) -> Result<Request, u16> {
    let mut budget = MAX_HEAD;
    let line = head_line(reader, &mut budget).ok_or(431u16)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { return Err(400) };
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(400);
    }
    let mut headers = Vec::new();
    loop {
        let line = head_line(reader, &mut budget).ok_or(431u16)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        if headers.len() > 100 {
            return Err(431);
        }
    }
    let path = target.split(['?', '#']).next().unwrap_or_default().to_owned();
    Ok(Request { method: method.to_owned(), path, headers })
}

/// The body (`Content-Length` or chunked), at most [`SAT_FILE_CHUNK`] bytes.
fn read_body(reader: &mut BufReader<TcpStream>, out: &mut TcpStream, request: &Request) -> Result<Vec<u8>, (u16, &'static str)> {
    let too_large = (413, "the body is larger than the VM link carries (1 MiB)");
    let chunked = request.header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let length = match request.header("content-length") {
        Some(length) => Some(length.parse::<usize>().map_err(|_| (400, "a bad Content-Length"))?),
        None if chunked => None,
        None => return Err((411, "a body needs a Content-Length")),
    };
    if length.is_some_and(|l| l > SAT_FILE_CHUNK) {
        return Err(too_large);
    }
    if request.header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
        let _ = out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let cut = (400, "the body was cut short");
    match length {
        Some(length) => {
            let mut body = vec![0; length];
            reader.read_exact(&mut body).map_err(|_| cut)?;
            Ok(body)
        }
        None => {
            let mut body = Vec::new();
            let mut budget = MAX_HEAD;
            loop {
                let line = head_line(reader, &mut budget).ok_or(cut)?;
                let size = usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| (400, "a bad chunk size"))?;
                if size == 0 {
                    while !head_line(reader, &mut budget).ok_or(cut)?.is_empty() {}
                    return Ok(body);
                }
                if body.len() + size > SAT_FILE_CHUNK {
                    return Err(too_large);
                }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..]).map_err(|_| cut)?;
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).map_err(|_| cut)?;
            }
        }
    }
}

/// `/mcp/<term>/<server>` → (term, server).
fn route_of(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/mcp/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (term, server) = rest.split_once('/')?;
    let term_ok = crate::vm::launch::valid_box(term);
    let server_ok = !server.is_empty() && server.len() <= 100 && server != "." && server != ".." && server.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (term_ok && server_ok).then(|| (term.to_owned(), server.to_owned()))
}

/// Whether `accept` takes `kind` (`application/json`, `text/event-stream`).
fn accepts(accept: Option<&str>, kind: &str) -> bool {
    let Some(accept) = accept else { return true };
    let family = format!("{}/*", kind.split('/').next().unwrap_or_default());
    accept.split(',').map(|part| part.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()).any(|t| t == kind || t == "*/*" || t == family)
}

impl McpProxy {
    /// `127.0.0.1`, `localhost` or `[::1]`, with this proxy's port.
    fn host_ok(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            Some((name, port)) if !name.ends_with(':') => (name, port),
            _ => return false,
        };
        matches!(name.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "[::1]") && port.parse::<u16>() == Ok(self.port)
    }

    fn origin_ok(&self, origin: &str) -> bool {
        origin.strip_prefix("http://").is_some_and(|host| self.host_ok(host.trim_end_matches('/')))
    }

    /// One connection: one request (`Connection: close`). Its end is
    /// graceful: what the client still sends (a body that was refused) is
    /// read and dropped for a moment, so the response is not lost to a reset.
    fn serve(&self, connection: TcpStream) {
        let _ = connection.set_read_timeout(Some(READ_WAIT));
        let _ = connection.set_write_timeout(Some(READ_WAIT));
        let _ = connection.set_nodelay(true);
        let Ok(mut out) = connection.try_clone() else { return };
        let mut reader = BufReader::new(connection);
        self.handle(&mut reader, &mut out);
        let _ = out.shutdown(std::net::Shutdown::Write);
        let _ = out.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = std::io::copy(&mut reader.take(4 * SAT_FILE_CHUNK as u64), &mut std::io::sink());
    }

    fn handle(&self, reader: &mut BufReader<TcpStream>, out: &mut TcpStream) {
        let request = match read_head(reader) {
            Ok(request) => request,
            Err(status) => return refuse(out, status, "a request the MCP proxy cannot read"),
        };
        match request.header("host") {
            None => return refuse(out, 400, "a request without Host"),
            Some(host) if !self.host_ok(host) => return refuse(out, 403, "the MCP proxy serves this machine's loopback only"),
            Some(_) => {}
        }
        if request.header("origin").is_some_and(|origin| !self.origin_ok(origin)) {
            return refuse(out, 403, "the MCP proxy serves this machine's loopback only");
        }
        let Some((term, server)) = route_of(&request.path) else {
            return refuse(out, 404, "the MCP proxy serves /mcp/<session>/<server>");
        };
        match request.method.as_str() {
            "POST" => self.post(reader, out, &request, &term, &server),
            "GET" => self.get(out, &request, &term, &server),
            "DELETE" => self.delete(out, &request, &term, &server),
            _ => respond_json(out, 405, &[("Allow", "GET, POST, DELETE")], &jsonrpc::error(Value::Null, jsonrpc::INVALID_REQUEST, "GET, POST or DELETE", None)),
        }
    }

    /// The session a request names (`Err`: its status and why).
    fn named(&self, request: &Request, term: &str, server: &str) -> Result<Arc<Session>, (u16, &'static str)> {
        let id = request.header("mcp-session-id").ok_or((400, "no Mcp-Session-Id: initialize first"))?;
        let session = self.session(id).filter(|s| s.term == term && s.server == server).ok_or((404, "no such MCP session (it ended): initialize again"))?;
        if !self.linked() {
            return Err((503, "the VM's link to the hub is down"));
        }
        Ok(session)
    }

    fn post(&self, reader: &mut BufReader<TcpStream>, out: &mut TcpStream, request: &Request, term: &str, server: &str) {
        if !request.header("content-type").is_some_and(|t| t.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("application/json")) {
            return refuse(out, 415, "the body is application/json");
        }
        let accept = request.header("accept");
        let (json_ok, sse_ok) = (accepts(accept, "application/json"), accepts(accept, "text/event-stream"));
        if !json_ok && !sse_ok {
            return refuse(out, 406, "the answer is application/json or text/event-stream");
        }
        let body = match read_body(reader, out, request) {
            Ok(body) => body,
            Err((status, why)) => return refuse(out, status, why),
        };
        let Ok(body) = serde_json::from_slice::<Value>(&body) else {
            return respond_json(out, 400, &[], &jsonrpc::error(Value::Null, jsonrpc::PARSE_ERROR, "the body is not JSON", None));
        };
        let opening = request.header("mcp-session-id").is_none();
        let session = if opening {
            if !matches!(jsonrpc::classify(&body), Ok(Kind::Request { ref method, .. }) if method == "initialize") {
                return refuse(out, 400, "no Mcp-Session-Id: initialize first");
            }
            if !self.linked() {
                return refuse(out, 503, "the VM's link to the hub is down");
            }
            self.new_session(term, server)
        } else {
            match self.named(request, term, server) {
                Ok(session) => session,
                Err((status, why)) => return refuse(out, status, why),
            }
        };
        let version = lock(&session.inner).version.clone();
        if let Some(asked) = request.header("mcp-protocol-version").filter(|_| !opening)
            && (!jsonrpc::known(asked) || version.as_deref().is_some_and(|v| v != asked))
        {
            return refuse(out, 400, "an MCP-Protocol-Version this session does not speak");
        }
        let batch = body.is_array();
        let messages = match jsonrpc::split(body, version.as_deref()) {
            Ok(messages) => messages,
            Err(error) => {
                if opening {
                    self.end(&session, "not opened", false);
                }
                return respond_json(out, 400, &[], &error);
            }
        };
        // The requests' ids, each waited for once.
        let mut ids = Vec::new();
        for message in &messages {
            if let Ok(Kind::Request { id, .. }) = jsonrpc::classify(message) {
                ids.push(jsonrpc::key(&id));
            }
        }
        let (tx, rx) = crossbeam_channel::unbounded::<Event>();
        let outlet = self.outlets.fetch_add(1, Ordering::Relaxed);
        {
            let mut inner = lock(&session.inner);
            inner.used = Instant::now();
            let unique: HashSet<&String> = ids.iter().collect();
            if unique.len() != ids.len() || ids.iter().any(|id| inner.waiting.contains_key(id)) {
                drop(inner);
                return refuse(out, 400, "a request id that is already waiting for its answer");
            }
            for id in &ids {
                inner.waiting.insert(id.clone(), tx.clone());
            }
            if sse_ok && !ids.is_empty() {
                inner.outlets.push((outlet, tx.clone()));
            }
            // A cancellation ends the wait for that request here too.
            for message in &messages {
                if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
                    && let Some(id) = message.pointer("/params/requestId")
                    && let Some(waiting) = inner.waiting.remove(&jsonrpc::key(id))
                {
                    stop_taking(&mut inner, &waiting);
                    let _ = waiting.send(Event::Cancelled(jsonrpc::key(id)));
                }
            }
        }
        drop(tx);
        let mut sent = !opening || self.send_up(session.stream, MCP_OPEN, serde_json::to_vec(&McpOpen { term: term.to_owned(), server: server.to_owned() }).unwrap_or_default());
        for message in &messages {
            sent = sent && self.send_up(session.stream, MCP_MESSAGE, serde_json::to_vec(message).unwrap_or_default());
        }
        if !sent {
            self.end(&session, "the VM's link to the hub is down", true);
            return refuse(out, 503, "the VM's link to the hub is down");
        }
        if ids.is_empty() {
            return respond(out, 202, &[], None, b"");
        }
        self.answer(out, &session, rx, ids, batch, sse_ok, opening, outlet);
    }

    /// Waits for a request's answers and writes them: JSON when only the
    /// answers come, an event stream when something else comes first or
    /// they take long.
    #[allow(clippy::too_many_arguments)]
    fn answer(&self, out: &mut TcpStream, session: &Session, rx: Receiver<Event>, ids: Vec<String>, batch: bool, sse_ok: bool, opening: bool, outlet: u64) {
        let started = Instant::now();
        let header = opening.then_some(session.id.as_str());
        let mut remaining: HashSet<String> = ids.into_iter().collect();
        let mut answers: Vec<Value> = Vec::new();
        let mut streaming = false;
        let mut ended: Option<(String, bool)> = None;
        let mut cancelled: Vec<String> = Vec::new();
        let mut gone = false;
        while !remaining.is_empty() && !gone {
            let wait = if streaming || !sse_ok { KEEPALIVE } else { JSON_WAIT.saturating_sub(started.elapsed()).max(Duration::from_millis(1)) };
            match rx.recv_timeout(wait) {
                Ok(Event::Answer(message)) => {
                    remaining.remove(&jsonrpc::key(&message["id"]));
                    if opening && let Some(version) = message.pointer("/result/protocolVersion").and_then(Value::as_str) {
                        lock(&session.inner).version = Some(version.to_owned());
                    }
                    if streaming {
                        gone = !event(out, &message);
                    } else {
                        answers.push(message);
                    }
                }
                Ok(Event::Message(message)) => {
                    if !streaming {
                        streaming = true;
                        gone = !start_events(out, header) || !answers.drain(..).all(|a| event(out, &a));
                    }
                    gone = gone || !event(out, &message);
                }
                Ok(Event::Cancelled(id)) => {
                    if remaining.remove(&id) {
                        cancelled.push(id);
                    }
                }
                Ok(Event::Ended { why, link }) => {
                    ended = Some((why, link));
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if started.elapsed() > ANSWER_WAIT {
                        ended = Some(("no answer from the hub's MCP server within 30 minutes".into(), false));
                        break;
                    }
                    if sse_ok && !streaming && started.elapsed() >= JSON_WAIT {
                        streaming = true;
                        gone = !start_events(out, header) || !answers.drain(..).all(|a| event(out, &a));
                    }
                    if streaming {
                        gone = gone || !keepalive(out);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        {
            let mut inner = lock(&session.inner);
            for id in &remaining {
                inner.waiting.remove(id);
            }
            inner.outlets.retain(|(o, _)| *o != outlet);
        }
        if gone {
            if opening {
                // The client never learnt the session's id: nobody can use it.
                if let Some(session) = self.session(&session.id) {
                    self.end(&session, "the client went away", false);
                    self.send_up(session.stream, MCP_CLOSE, Vec::new());
                }
            }
            return;
        }
        // What was not answered: cancelled, or the reason the session ended.
        let id = |key: &String| serde_json::from_str::<Value>(key).unwrap_or(Value::Null);
        let mut unanswered: Vec<Value> = cancelled.iter().map(|key| jsonrpc::error(id(key), -32800, "the request was cancelled", None)).collect();
        let why = ended.as_ref().map_or("no answer from the hub", |(why, _)| why.as_str());
        unanswered.extend(remaining.iter().map(|key| jsonrpc::error(id(key), jsonrpc::SESSION_ENDED, why, None)));
        if streaming {
            for error in &unanswered {
                if !event(out, error) {
                    return;
                }
            }
            return;
        }
        answers.extend(unanswered);
        let link = ended.as_ref().is_some_and(|(_, link)| *link);
        let body = if batch { Value::Array(answers) } else { answers.pop().unwrap_or(Value::Null) };
        let refused = opening && ended.as_ref().is_some_and(|(_, link)| !link) && lock(&session.inner).error.is_some();
        let status = if link {
            503
        } else if refused {
            403
        } else {
            200
        };
        let headers: Vec<(&str, &str)> = header.filter(|_| status == 200 && self.session(&session.id).is_some()).map(|id| ("Mcp-Session-Id", id)).into_iter().collect();
        respond_json(out, status, &headers, &body);
    }

    fn get(&self, out: &mut TcpStream, request: &Request, term: &str, server: &str) {
        if !request.header("accept").is_some_and(|a| accepts(Some(a), "text/event-stream")) {
            return refuse(out, 406, "the server-to-client stream is text/event-stream");
        }
        let session = match self.named(request, term, server) {
            Ok(session) => session,
            Err((status, why)) => return refuse(out, status, why),
        };
        let (tx, rx) = crossbeam_channel::unbounded::<Event>();
        let outlet = self.outlets.fetch_add(1, Ordering::Relaxed);
        let backlog: Vec<Value> = {
            let mut inner = lock(&session.inner);
            if let Some((_, old)) = inner.get.replace((outlet, tx)) {
                let _ = old.send(Event::Ended { why: "another GET stream took over".into(), link: false });
            }
            inner.backlog.drain(..).collect()
        };
        let mut open = start_events(out, None) && backlog.iter().all(|m| event(out, m));
        while open {
            match rx.recv_timeout(KEEPALIVE) {
                Ok(Event::Message(message)) => open = event(out, &message),
                Ok(Event::Ended { .. }) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => open = keepalive(out),
            }
        }
        let mut inner = lock(&session.inner);
        if inner.get.as_ref().is_some_and(|(o, _)| *o == outlet) {
            inner.get = None;
        }
    }

    fn delete(&self, out: &mut TcpStream, request: &Request, term: &str, server: &str) {
        let session = match self.named(request, term, server) {
            Ok(session) => session,
            Err((status, why)) => return refuse(out, status, why),
        };
        self.end(&session, "the client ended the MCP session", false);
        self.send_up(session.stream, MCP_CLOSE, Vec::new());
        respond(out, 200, &[], None, b"");
    }
}

/// A small HTTP/1.1 client for tests (the proxy closes every connection after its response).
#[cfg(test)]
pub(crate) mod client {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use serde_json::Value;

    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Reply {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }

        pub fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }

        /// The messages of an event stream.
        pub fn events(&self) -> Vec<Value> {
            String::from_utf8_lossy(&self.body).lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect()
        }

        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    fn head(reader: &mut BufReader<TcpStream>) -> (u16, Vec<(String, String)>) {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }
        (status, headers)
    }

    fn connect(port: u16, text: &[u8]) -> BufReader<TcpStream> {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        // The proxy may answer (and close) before it reads a refused body.
        let _ = socket.write_all(text);
        BufReader::new(socket)
    }

    /// Sends `text` as it is and reads the whole response.
    pub fn raw(port: u16, text: &[u8]) -> Reply {
        let mut reader = connect(port, text);
        let (status, headers) = head(&mut reader);
        let mut body = Vec::new();
        let _ = reader.read_to_end(&mut body);
        Reply { status, headers, body }
    }

    fn compose(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut text = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
            text.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
        }
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        if method == "POST" {
            text.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        text.push_str("\r\n");
        let mut text = text.into_bytes();
        text.extend_from_slice(body);
        text
    }

    pub fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        raw(port, &compose(port, method, path, headers, body))
    }

    /// A JSON-RPC message (or batch) posted as an MCP client does.
    pub fn post(port: u16, path: &str, session: Option<&str>, message: &Value) -> Reply {
        let mut headers = vec![("Content-Type", "application/json"), ("Accept", "application/json, text/event-stream")];
        if let Some(session) = session {
            headers.push(("Mcp-Session-Id", session));
        }
        request(port, "POST", path, &headers, message.to_string().as_bytes())
    }

    /// A response read event by event (a `GET` stream).
    pub struct Stream {
        pub status: u16,
        reader: BufReader<TcpStream>,
    }

    impl Stream {
        pub fn open(port: u16, path: &str, session: &str) -> Stream {
            let text = compose(port, "GET", path, &[("Accept", "text/event-stream"), ("Mcp-Session-Id", session)], b"");
            let mut reader = connect(port, &text);
            let (status, _) = head(&mut reader);
            Stream { status, reader }
        }

        /// The next message (`None`: the stream ended or nothing came within 20 s).
        pub fn next(&mut self) -> Option<Value> {
            loop {
                let mut line = String::new();
                if self.reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return None;
                }
                if let Some(data) = line.trim_end().strip_prefix("data: ") {
                    return serde_json::from_str(data).ok();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::client::{self, Reply};
    use super::*;
    use crossbeam_channel::bounded;
    use serde_json::json;

    const TERM: &str = "0b1e5ca1-0000-4000-8000-0000000000aa";

    fn path(server: &str) -> String {
        format!("/mcp/{TERM}/{server}")
    }

    type Got = Arc<Mutex<Vec<(u64, u8, Value)>>>;

    /// A stand-in for the hub on the other end of a link (a channel): it
    /// records what comes up and answers as each method says.
    fn hub(proxy: &Arc<McpProxy>) -> Got {
        let (tx, rx) = bounded::<SatUp>(1024);
        proxy.attach(Some(tx));
        let got: Got = Arc::default();
        let (proxy, seen) = (proxy.clone(), got.clone());
        std::thread::spawn(move || {
            for up in rx {
                let SatUp::Mcp { stream, kind, bytes } = up else { continue };
                let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                seen.lock().unwrap().push((stream, kind, value.clone()));
                let say = |message: Value| proxy.down(stream, MCP_MESSAGE, message.to_string().into_bytes());
                if kind == MCP_OPEN && value["server"] == "refused" {
                    proxy.down(stream, MCP_ERROR, json!({ "message": "refused by the VM's MCP policy: nope" }).to_string().into_bytes());
                    proxy.down(stream, MCP_CLOSE, Vec::new());
                    continue;
                }
                if kind != MCP_MESSAGE || value.get("method").is_none() || value.get("id").is_none() {
                    continue;
                }
                let id = value["id"].clone();
                match value["method"].as_str().unwrap_or_default() {
                    "initialize" => say(jsonrpc::result(id, json!({ "protocolVersion": value["params"]["protocolVersion"], "capabilities": { "tools": {} } }))),
                    "tools/list" => say(jsonrpc::result(id, json!({ "tools": [] }))),
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(50));
                        say(jsonrpc::notification("notifications/progress", json!({ "progressToken": "t", "progress": 1 })));
                        say(jsonrpc::result(id, json!({ "done": true })));
                    }
                    "ask" => {
                        say(jsonrpc::request(json!("s1"), "roots/list", json!({})));
                        say(jsonrpc::result(id, json!({ "asked": true })));
                    }
                    "push" => {
                        say(jsonrpc::result(id, json!({})));
                        std::thread::sleep(Duration::from_millis(20));
                        say(jsonrpc::notification("notifications/tools/list_changed", json!({})));
                    }
                    "echo" => say(jsonrpc::result(id, value["params"].clone())),
                    "end" => proxy.down(stream, MCP_CLOSE, Vec::new()),
                    "never" => {}
                    _ => say(jsonrpc::error(id, jsonrpc::METHOD_NOT_FOUND, "no such method", None)),
                }
            }
        });
        got
    }

    fn initialize(proxy: &McpProxy, version: &str) -> (Reply, String) {
        let reply = client::post(proxy.port(), &path("forge"), None, &jsonrpc::request(json!(0), "initialize", json!({ "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } })));
        let session = reply.header("Mcp-Session-Id").unwrap_or_default().to_owned();
        (reply, session)
    }

    fn call(proxy: &McpProxy, session: &str, id: Value, method: &str) -> Reply {
        client::post(proxy.port(), &path("forge"), Some(session), &jsonrpc::request(id, method, json!({})))
    }

    fn wait_until(what: &str, ok: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_session_opens_with_initialize_and_answers_come_as_json_or_a_stream() {
        let proxy = McpProxy::start(0).unwrap();
        assert_ne!(proxy.port(), 0);
        let got = hub(&proxy);
        let (reply, session) = initialize(&proxy, "2025-06-18");
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_eq!(reply.header("Content-Type"), Some("application/json"));
        assert_eq!(reply.json()["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply.json()["id"], 0, "ids are kept");
        assert_eq!(session.len(), 32);
        {
            let got = got.lock().unwrap();
            assert_eq!(got[0].1, MCP_OPEN);
            assert_eq!((got[0].2["term"].as_str(), got[0].2["server"].as_str()), (Some(TERM), Some("forge")), "the session's box and server come from the path");
            assert_eq!((got[1].1, got[1].2["method"].as_str()), (MCP_MESSAGE, Some("initialize")));
        }
        let note = client::post(proxy.port(), &path("forge"), Some(&session), &jsonrpc::notification("notifications/initialized", json!({})));
        assert_eq!((note.status, note.body.len()), (202, 0));
        let list = call(&proxy, &session, json!("a-1"), "tools/list");
        assert_eq!((list.status, list.json()["id"].clone()), (200, json!("a-1")));
        // Something else first: the answer streams.
        let slow = call(&proxy, &session, json!(7), "slow");
        assert_eq!(slow.header("Content-Type"), Some("text/event-stream"));
        let events = slow.events();
        assert_eq!(events.len(), 2, "{}", slow.text());
        assert_eq!((events[0]["method"].as_str(), events[1]["id"].clone()), (Some("notifications/progress"), json!(7)));
        let asked = call(&proxy, &session, json!(8), "ask");
        let events = asked.events();
        assert_eq!((events[0]["method"].as_str(), events[0]["id"].as_str()), (Some("roots/list"), Some("s1")));
        assert_eq!(events[1]["id"], 8);
        let answer = client::post(proxy.port(), &path("forge"), Some(&session), &jsonrpc::result(json!("s1"), json!({ "roots": [] })));
        assert_eq!(answer.status, 202, "a response to the server's request");
        wait_until("the answer went up", || got.lock().unwrap().iter().any(|(_, k, v)| *k == MCP_MESSAGE && v["id"] == "s1" && v.get("result").is_some()));
        // A large message crosses whole.
        let text = "x".repeat(900 << 10);
        let echo = client::post(proxy.port(), &path("forge"), Some(&session), &jsonrpc::request(json!(9), "echo", json!({ "text": text })));
        assert_eq!(echo.json()["result"]["text"].as_str().map(str::len), Some(900 << 10));
        let unknown = call(&proxy, &session, json!(10), "resources/list");
        assert_eq!(unknown.json()["error"]["code"], jsonrpc::METHOD_NOT_FOUND, "the hub's errors pass as they are");
    }

    #[test]
    fn sessions_need_their_id_and_end_with_delete_the_hub_or_the_link() {
        let proxy = McpProxy::start(0).unwrap();
        let got = hub(&proxy);
        let (_, session) = initialize(&proxy, "2025-06-18");
        assert_eq!(call(&proxy, "", json!(1), "tools/list").status, 404, "an empty id is no session");
        let no_id = client::post(proxy.port(), &path("forge"), None, &jsonrpc::request(json!(1), "tools/list", json!({})));
        assert_eq!(no_id.status, 400, "no session id: initialize first");
        assert_eq!(call(&proxy, "00000000000000000000000000000000", json!(1), "tools/list").status, 404);
        assert_eq!(client::post(proxy.port(), &path("github"), Some(&session), &jsonrpc::request(json!(1), "tools/list", json!({}))).status, 404, "a session belongs to its path");
        // The server-to-client stream: what comes with nothing waiting goes there.
        let mut stream = client::Stream::open(proxy.port(), &path("forge"), &session);
        assert_eq!(stream.status, 200);
        assert_eq!(call(&proxy, &session, json!(2), "push").status, 200);
        assert_eq!(stream.next().unwrap()["method"], "notifications/tools/list_changed");
        // DELETE ends it here and on the hub.
        let deleted = client::request(proxy.port(), "DELETE", &path("forge"), &[("Mcp-Session-Id", &session)], b"");
        assert_eq!(deleted.status, 200);
        assert!(stream.next().is_none(), "the GET stream ends with the session");
        let stream_id = got.lock().unwrap()[0].0;
        wait_until("the hub was told", || got.lock().unwrap().iter().any(|(s, k, _)| *s == stream_id && *k == MCP_CLOSE));
        assert_eq!(call(&proxy, &session, json!(3), "tools/list").status, 404);
        assert_eq!(client::request(proxy.port(), "DELETE", &path("forge"), &[("Mcp-Session-Id", &session)], b"").status, 404);
        // The hub ends a session: the request waiting gets an error, then 404.
        let (_, session) = initialize(&proxy, "2025-06-18");
        let ended = call(&proxy, &session, json!(4), "end");
        assert_eq!((ended.status, ended.json()["error"]["code"].clone(), ended.json()["id"].clone()), (200, json!(jsonrpc::SESSION_ENDED), json!(4)));
        assert_eq!(call(&proxy, &session, json!(5), "tools/list").status, 404);
        // The link drops: waiting requests get 503, the session 404, new sessions 503.
        let (_, session) = initialize(&proxy, "2025-06-18");
        let waiting = {
            let (port, session) = (proxy.port(), session.clone());
            std::thread::spawn(move || client::post(port, &path("forge"), Some(&session), &jsonrpc::request(json!(6), "never", json!({}))))
        };
        wait_until("the request reached the hub", || got.lock().unwrap().iter().any(|(_, _, v)| v["method"] == "never"));
        proxy.attach(None);
        proxy.link_lost();
        assert_eq!(waiting.join().unwrap().status, 503);
        assert_eq!(call(&proxy, &session, json!(7), "tools/list").status, 404);
        assert_eq!(initialize(&proxy, "2025-06-18").0.status, 503);
        // Linked again: new sessions.
        let _got = hub(&proxy);
        assert_eq!(initialize(&proxy, "2025-06-18").0.status, 200);
    }

    #[test]
    fn a_refused_session_answers_403_with_the_hubs_reason() {
        let proxy = McpProxy::start(0).unwrap();
        let _got = hub(&proxy);
        let reply = client::post(proxy.port(), &path("refused"), None, &jsonrpc::request(json!("i"), "initialize", json!({ "protocolVersion": "2025-06-18" })));
        assert_eq!(reply.status, 403, "{}", reply.text());
        assert_eq!(reply.json()["id"], "i");
        assert!(reply.json()["error"]["message"].as_str().unwrap().contains("nope"));
        assert!(reply.header("Mcp-Session-Id").is_none());
        assert!(lock(&proxy.state).sessions.is_empty());
    }

    #[test]
    fn only_loopback_hosts_and_origins_and_bounded_requests() {
        let proxy = McpProxy::start(0).unwrap();
        let _got = hub(&proxy);
        let port = proxy.port();
        let init = jsonrpc::request(json!(0), "initialize", json!({ "protocolVersion": "2025-06-18" })).to_string();
        let post = |headers: &[(&str, &str)], body: &[u8]| client::request(port, "POST", &path("forge"), headers, body);
        let json_type = ("Content-Type", "application/json");
        let foreign_host = format!("evil.example:{port}");
        assert_eq!(post(&[("Host", &foreign_host), json_type], init.as_bytes()).status, 403, "DNS rebinding: another host");
        assert_eq!(post(&[("Host", "127.0.0.1:1"), json_type], init.as_bytes()).status, 403, "another port");
        assert_eq!(post(&[json_type, ("Origin", "http://evil.example")], init.as_bytes()).status, 403);
        assert_eq!(post(&[json_type, ("Origin", "null")], init.as_bytes()).status, 403);
        let origin = format!("http://localhost:{port}");
        assert_eq!(post(&[json_type, ("Origin", &origin)], init.as_bytes()).status, 200, "this machine's own origin");
        let localhost = format!("localhost:{port}");
        assert_eq!(post(&[("Host", &localhost), json_type], init.as_bytes()).status, 200);
        assert_eq!(client::raw(port, format!("POST {} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}", path("forge")).as_bytes()).status, 400, "no Host");
        assert_eq!(client::request(port, "POST", "/other", &[json_type], init.as_bytes()).status, 404);
        assert_eq!(client::request(port, "POST", "/mcp/../forge", &[json_type], init.as_bytes()).status, 404);
        assert_eq!(client::request(port, "POST", &format!("/mcp/{TERM}/a b"), &[json_type], init.as_bytes()).status, 400, "not even a request line");
        assert_eq!(client::request(port, "PUT", &path("forge"), &[json_type], init.as_bytes()).status, 405);
        assert_eq!(post(&[("Content-Type", "text/plain")], init.as_bytes()).status, 415);
        assert_eq!(post(&[json_type, ("Accept", "text/html")], init.as_bytes()).status, 406);
        assert_eq!(post(&[json_type], b"{ not json").json()["error"]["code"], jsonrpc::PARSE_ERROR);
        let big = vec![b' '; SAT_FILE_CHUNK + 1];
        assert_eq!(post(&[json_type], &big).status, 413);
        let long = "x".repeat(MAX_HEAD);
        assert_eq!(post(&[json_type, ("X-Long", &long)], init.as_bytes()).status, 431);
        let chunked = format!("POST {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{init}\r\n0\r\n\r\n", path("forge"), init.len());
        assert_eq!(client::raw(port, chunked.as_bytes()).status, 200, "a chunked body");
        let no_length = format!("POST {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\r\n", path("forge"));
        assert_eq!(client::raw(port, no_length.as_bytes()).status, 411);
        assert_eq!(client::request(port, "GET", &path("forge"), &[("Accept", "text/event-stream")], b"").status, 400, "a GET names its session");
    }

    #[test]
    fn batches_follow_the_protocol_version_and_ids_stay_the_clients() {
        let proxy = McpProxy::start(0).unwrap();
        let got = hub(&proxy);
        let (_, old) = initialize(&proxy, "2025-03-26");
        let batch = json!([
            jsonrpc::request(json!(1), "tools/list", json!({})),
            jsonrpc::request(json!("two"), "echo", json!({ "x": 2 })),
            jsonrpc::notification("notifications/progress", json!({ "progressToken": "p", "progress": 1 })),
        ]);
        let reply = client::post(proxy.port(), &path("forge"), Some(&old), &batch);
        assert_eq!(reply.status, 200, "{}", reply.text());
        let mut ids: Vec<String> = reply.json().as_array().unwrap().iter().map(|a| a["id"].to_string()).collect();
        ids.sort();
        assert_eq!(ids, ["\"two\"", "1"]);
        wait_until("initialize and the batch's three went up, one message each", || got.lock().unwrap().iter().filter(|(_, k, v)| *k == MCP_MESSAGE && v.is_object()).count() == 4);
        assert!(got.lock().unwrap().iter().all(|(_, _, v)| !v.is_array()), "a batch never crosses the link");
        let (_, new) = initialize(&proxy, "2025-06-18");
        let refused = client::post(proxy.port(), &path("forge"), Some(&new), &batch);
        assert_eq!(refused.status, 400);
        assert!(refused.json()["error"]["message"].as_str().unwrap().contains("2025-06-18"));
        let opening = client::post(proxy.port(), &path("forge"), None, &json!([jsonrpc::request(json!(0), "initialize", json!({}))]));
        assert_eq!(opening.status, 400, "no batch opens a session");
        let duplicate = client::post(proxy.port(), &path("forge"), Some(&old), &json!([jsonrpc::request(json!(5), "echo", json!({})), jsonrpc::request(json!(5), "echo", json!({}))]));
        assert_eq!(duplicate.status, 400, "an id waited for twice");
        let version = |session: &str, v: &str| client::request(proxy.port(), "POST", &path("forge"), &[("Content-Type", "application/json"), ("Mcp-Session-Id", session), ("MCP-Protocol-Version", v)], jsonrpc::request(json!(1), "tools/list", json!({})).to_string().as_bytes()).status;
        assert_eq!(version(&new, "2025-06-18"), 200);
        assert_eq!(version(&new, "2025-03-26"), 400, "not this session's version");
        assert_eq!(version(&new, "2099-01-01"), 400, "a version the proxy does not speak");
    }

    #[test]
    fn a_cancellation_ends_the_wait_for_its_request() {
        let proxy = McpProxy::start(0).unwrap();
        let got = hub(&proxy);
        let (_, session) = initialize(&proxy, "2025-06-18");
        let waiting = {
            let (port, session) = (proxy.port(), session.clone());
            std::thread::spawn(move || client::post(port, &path("forge"), Some(&session), &jsonrpc::request(json!(42), "never", json!({}))))
        };
        wait_until("the request reached the hub", || got.lock().unwrap().iter().any(|(_, _, v)| v["method"] == "never"));
        let cancel = client::post(proxy.port(), &path("forge"), Some(&session), &jsonrpc::notification("notifications/cancelled", json!({ "requestId": 42, "reason": "user" })));
        assert_eq!(cancel.status, 202);
        let reply = waiting.join().unwrap();
        assert_eq!((reply.status, reply.json()["id"].clone()), (200, json!(42)));
        assert!(reply.json().get("error").is_some());
        wait_until("the cancellation went up", || got.lock().unwrap().iter().any(|(_, _, v)| v["method"] == "notifications/cancelled"));
    }
}
