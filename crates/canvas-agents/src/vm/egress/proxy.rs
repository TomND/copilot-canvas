//! The stand-in egress point ([`serve`](super::serve)): an HTTP/1.1 forward
//! proxy that applies a sandbox's policy to every request, as the platform's
//! egress does for `vm::aca` sandboxes.
//!
//! - Absolute-form requests (`GET http://host/path HTTP/1.1`, `Upgrade:
//!   websocket` among them) are decided on host, path and method. An allowed
//!   one goes out on a connection of its own with its head written anew:
//!   `Host` from the URL, hop-by-hop headers dropped, and each `Transform`
//!   header set from the egress point's secrets, replacing any the guest sent.
//!   A body passes as it came (`Content-Length` or chunked; a request with
//!   both, or with another coding, is refused: none can be read two ways).
//! - `CONNECT host:port` is decided on host only, then bytes pass both ways.
//! - A refusal answers `403` naming the rule, or "denied by default"; every
//!   decision, allowed or not, goes to the [`Recorder`].
//! - A name that resolves to this machine or its link is not connected to
//!   (only an address the policy writes out, or `localhost`, is), nor is the
//!   proxy itself.
//! - Bounded: heads, connections, connect and idle times. Nothing is logged,
//!   and no answer of the proxy carries a header's value.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::render::token_byte;
use super::{Decision, Recorder, Request, Secrets, decide};
use crate::paths::now_ms;
use crate::vm::provider::{EgressDecision, EgressPolicy, Secret};

/// The policy of every request (`None`: no policy yet, everything is refused).
pub(super) type PolicyFn = Arc<dyn Fn() -> Option<EgressPolicy> + Send + Sync>;

/// A request's head.
const MAX_HEAD: usize = 32 << 10;
/// An answer's head.
const MAX_ANSWER_HEAD: usize = 64 << 10;
const MAX_HEADERS: usize = 100;
/// A chunk-size or trailer line.
const MAX_LINE: usize = 4 << 10;
const MAX_CONNECTIONS: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A request's head arrives within this once it has begun (and a new
/// connection's first byte within this too).
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// A kept-alive connection waits this long for its next request.
const KEEP_ALIVE: Duration = Duration::from_secs(60);
/// No byte of a body or an answer for this long ends the exchange.
const IDLE: Duration = Duration::from_secs(300);
/// A tunnel with no byte either way for this long is closed.
const TUNNEL_IDLE: Duration = Duration::from_secs(1800);
/// How often a tunnel looks at its idle time.
const TICK: Duration = Duration::from_secs(5);
/// Headers of one hop: never passed on (the framing ones are written anew).
const HOP: [&str; 9] = ["connection", "keep-alive", "proxy-connection", "proxy-authorization", "proxy-authenticate", "te", "trailer", "upgrade", "transfer-encoding"];

struct Ctx {
    policy: PolicyFn,
    secrets: Arc<dyn Secrets>,
    recorder: Arc<dyn Recorder>,
    /// The proxy's own address (never a request's destination).
    own: SocketAddr,
    live: AtomicUsize,
}

/// Counts a connection while its thread runs (also when it panics).
struct Live(Arc<Ctx>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, SeqCst);
    }
}

pub(super) fn serve(listener: TcpListener, policy: PolicyFn, secrets: Arc<dyn Secrets>, recorder: Arc<dyn Recorder>) -> Result<()> {
    let own = listener.local_addr()?;
    let ctx = Arc::new(Ctx { policy, secrets, recorder, own, live: AtomicUsize::new(0) });
    let mut failing: Option<Instant> = None;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                failing = None;
                stream
            }
            Err(error) => {
                // A full file table passes; a listener that fails for a minute ends.
                if failing.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(60) {
                    return Err(error).context("the egress stand-in cannot accept connections");
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if ctx.live.fetch_add(1, SeqCst) >= MAX_CONNECTIONS {
            ctx.live.fetch_sub(1, SeqCst);
            let mut stream = stream;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = answer(&mut stream, 503, "the egress stand-in has too many connections");
            continue;
        }
        let live = Live(ctx.clone());
        // A thread that cannot start drops its closure, and so the count.
        if std::thread::Builder::new().name("egress".into()).spawn(move || connection(&live.0, stream)).is_err() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// What an exchange leaves of its connection.
enum After {
    KeepAlive,
    Close,
    /// Switched protocols: the upstream connection and what it already sent.
    Upgraded(TcpStream, Vec<u8>),
}

fn connection(ctx: &Ctx, client: TcpStream) {
    let _ = client.set_nodelay(true);
    let (Ok(()), Ok(clone)) = (client.set_write_timeout(Some(IDLE)), client.try_clone()) else { return };
    let mut reader = BufReader::with_capacity(16 << 10, clone);
    let mut writer = client;
    let mut wait = HEAD_TIMEOUT;
    let after = loop {
        let head = match read_head(&mut reader, MAX_HEAD, wait) {
            Ok(Some(head)) => head,
            Ok(None) | Err(HeadError::Broken) => return,
            Err(HeadError::TooLarge) => break answer(&mut writer, 431, "the request's head is too large").map(|_| After::Close),
        };
        wait = KEEP_ALIVE;
        let request = match parse_request(&head) {
            Ok(request) => request,
            Err(text) => break answer(&mut writer, 400, text).map(|_| After::Close),
        };
        if request.method == "CONNECT" {
            tunnel(ctx, &request, reader, writer);
            return;
        }
        match forward(ctx, &request, &mut reader, &mut writer) {
            Ok(After::KeepAlive) => continue,
            other => break other,
        }
    };
    match after {
        Ok(After::Upgraded(upstream, early)) => {
            let from_client = reader.buffer().to_vec();
            relay(writer, from_client, upstream, early);
        }
        Ok(_) => {
            // The guest may still be sending what was refused: read it a moment
            // so that closing does not reset the connection before the answer.
            let _ = writer.shutdown(Shutdown::Write);
            let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
            let _ = io::copy(&mut reader.take(1 << 20), &mut io::sink());
        }
        Err(_) => {}
    }
}

enum HeadError {
    TooLarge,
    /// Closed midway, too slow, or a read failed.
    Broken,
}

impl From<io::Error> for HeadError {
    fn from(_: io::Error) -> Self {
        HeadError::Broken
    }
}

/// Reads a head up to its empty line: `None` when the peer closed before its
/// first byte, which must come within `wait` (the rest within [`HEAD_TIMEOUT`]).
fn read_head(reader: &mut BufReader<TcpStream>, max: usize, wait: Duration) -> Result<Option<Vec<u8>>, HeadError> {
    let mut head = Vec::new();
    let mut deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut).into());
        }
        reader.get_ref().set_read_timeout(Some(left))?;
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if buf.is_empty() {
            return if head.is_empty() { Ok(None) } else { Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()) };
        }
        if head.is_empty() {
            deadline = Instant::now() + HEAD_TIMEOUT;
        }
        let (mut used, mut done) = (0, false);
        for &byte in buf {
            used += 1;
            head.push(byte);
            if head.ends_with(b"\n\r\n") || head.ends_with(b"\n\n") {
                done = true;
                break;
            }
            if head.len() > max {
                break;
            }
        }
        reader.consume(used);
        if done {
            return Ok(Some(head));
        }
        if head.len() > max {
            return Err(HeadError::TooLarge);
        }
    }
}

/// A head: its first line and its headers (names as sent).
struct Head {
    line: String,
    headers: Vec<(String, String)>,
}

/// Parses a head strictly: no folded lines, names that are tokens, no
/// control characters in values (a head is always written anew from this).
fn parse_head(bytes: &[u8]) -> Result<Head, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the head is not UTF-8")?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).skip_while(|l| l.is_empty());
    let line = lines.next().ok_or("an empty head")?.to_owned();
    let mut headers = Vec::new();
    for l in lines {
        if l.is_empty() {
            break;
        }
        if l.starts_with([' ', '\t']) {
            return Err("folded header lines are refused");
        }
        let (name, value) = l.split_once(':').ok_or("a header line without a colon")?;
        if name.is_empty() || !name.bytes().all(token_byte) {
            return Err("a header name that is not a token");
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err("a header value with control characters");
        }
        headers.push((name.to_owned(), value.to_owned()));
        if headers.len() > MAX_HEADERS {
            return Err("too many headers");
        }
    }
    Ok(Head { line, headers })
}

/// A guest's request.
struct ClientRequest {
    method: String,
    target: String,
    http11: bool,
    headers: Vec<(String, String)>,
}

fn parse_request(bytes: &[u8]) -> Result<ClientRequest, &'static str> {
    let head = parse_head(bytes)?;
    let mut parts = head.line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("the request line is not METHOD TARGET HTTP/1.1");
    };
    if method.is_empty() || !method.bytes().all(token_byte) {
        return Err("the method is not a token");
    }
    if target.is_empty() || target.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("the request's target is empty or has control characters");
    }
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err("only HTTP/1.1 and HTTP/1.0 are relayed"),
    };
    Ok(ClientRequest { method: method.to_owned(), target: target.to_owned(), http11, headers: head.headers })
}

/// The values of every `name` header.
fn values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// The comma-separated tokens of every `name` header, lower case.
fn tokens(headers: &[(String, String)], name: &str) -> Vec<String> {
    values(headers, name).flat_map(|v| v.split(',')).map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).collect()
}

/// How a body is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Body {
    None,
    Length(u64),
    Chunked,
    UntilClose,
}

fn length(text: &str) -> Option<u64> {
    let text = text.trim();
    (!text.is_empty() && text.len() <= 18 && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

/// A request's body: one way to read it or a refusal (status, text).
fn request_body(headers: &[(String, String)]) -> Result<Body, (u16, &'static str)> {
    let lengths: Vec<&str> = values(headers, "content-length").collect();
    if values(headers, "transfer-encoding").next().is_some() {
        if !lengths.is_empty() {
            return Err((400, "a request with both Transfer-Encoding and Content-Length"));
        }
        return if tokens(headers, "transfer-encoding") == ["chunked"] { Ok(Body::Chunked) } else { Err((501, "only the chunked transfer coding is relayed")) };
    }
    match lengths.as_slice() {
        [] => Ok(Body::None),
        [one] => length(one).map(Body::Length).ok_or((400, "Content-Length is not a number")),
        _ => Err((400, "more than one Content-Length")),
    }
}

/// An answer's body (RFC 9112 §6.3).
fn answer_body(method: &str, status: u16, headers: &[(String, String)]) -> Body {
    if method == "HEAD" || status < 200 || status == 204 || status == 304 {
        return Body::None;
    }
    if values(headers, "transfer-encoding").next().is_some() {
        return if tokens(headers, "transfer-encoding").last().map(String::as_str) == Some("chunked") { Body::Chunked } else { Body::UntilClose };
    }
    let lengths: Vec<Option<u64>> = values(headers, "content-length").map(length).collect();
    match lengths.first() {
        Some(Some(n)) if lengths.iter().all(|l| *l == Some(*n)) => Body::Length(*n),
        _ => Body::UntilClose,
    }
}

fn invalid(text: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text)
}

/// A line of at most [`MAX_LINE`] bytes, without its line end.
fn read_line(from: &mut BufReader<TcpStream>) -> io::Result<String> {
    let mut line = Vec::new();
    from.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(if line.len() > MAX_LINE { invalid("a line is too long") } else { io::Error::from(io::ErrorKind::UnexpectedEof) });
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| invalid("a line that is not text"))
}

/// Copies a body as it is delimited; `dechunk` passes a chunked body's bytes
/// alone (to an HTTP/1.0 client, which reads to the close).
fn copy_body(from: &mut BufReader<TcpStream>, to: &mut impl Write, body: Body, dechunk: bool) -> io::Result<()> {
    from.get_ref().set_read_timeout(Some(IDLE))?;
    let exactly = |from: &mut BufReader<TcpStream>, to: &mut dyn Write, n: u64| -> io::Result<()> {
        if io::copy(&mut from.by_ref().take(n), to)? < n { Err(io::Error::from(io::ErrorKind::UnexpectedEof)) } else { Ok(()) }
    };
    match body {
        Body::None => Ok(()),
        Body::Length(n) => exactly(from, to, n),
        Body::UntilClose => io::copy(from, to).map(drop),
        Body::Chunked => loop {
            let line = read_line(from)?;
            let size = line.split(';').next().unwrap_or_default().trim();
            if size.is_empty() || size.len() > 15 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid("a chunk size that does not parse"));
            }
            let size = u64::from_str_radix(size, 16).map_err(|_| invalid("a chunk size that does not parse"))?;
            if !dechunk {
                write!(to, "{size:x}\r\n")?;
            }
            if size == 0 {
                // Trailers are dropped: nothing after a body carries what its head could not.
                for _ in 0..=MAX_HEADERS {
                    if read_line(from)?.is_empty() {
                        if !dechunk {
                            to.write_all(b"\r\n")?;
                        }
                        return Ok(());
                    }
                }
                return Err(invalid("too many trailers"));
            }
            exactly(from, to, size)?;
            if !read_line(from)?.is_empty() {
                return Err(invalid("a chunk without its line end"));
            }
            if !dechunk {
                to.write_all(b"\r\n")?;
            }
        },
    }
}

/// Text of the pack (a rule's or a secret's name) as an answer shows it.
fn printable(text: &str) -> String {
    text.chars().take(120).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// `host:port` or `[v6]:port` (`default` when the port may be left out):
/// the host in lower case.
fn authority(text: &str, default: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let (v6, after) = rest.split_once(']')?;
            v6.parse::<Ipv6Addr>().ok()?;
            let port = if after.is_empty() { None } else { Some(after.strip_prefix(':')?) };
            (v6, port)
        }
        None => {
            let (host, port) = text.rsplit_once(':').map_or((text, None), |(h, p)| (h, Some(p)));
            let label = |l: &str| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if host.len() > 253 || !host.strip_suffix('.').unwrap_or(host).split('.').all(label) {
                return None;
            }
            (host, port)
        }
    };
    let port = match port {
        Some(p) => Some(p).filter(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0)?,
        None => default?,
    };
    Some((host.to_ascii_lowercase(), port))
}

/// An absolute-form target.
struct Target {
    host: String,
    port: u16,
    /// Origin-form: path and query.
    path: String,
}

impl Target {
    /// The `Host` header it goes out with.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == 80 { host } else { format!("{host}:{}", self.port) }
    }
}

fn absolute(target: &str) -> Result<Target, (u16, &'static str)> {
    let Some((scheme, rest)) = target.split_once("://") else {
        return Err((400, "the egress stand-in is a proxy: send http://host/path, or CONNECT host:port"));
    };
    if scheme.eq_ignore_ascii_case("https") {
        return Err((501, "https:// goes through CONNECT"));
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return Err((400, "only http:// is relayed (https:// through CONNECT)"));
    }
    let (authority_text, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let (host, port) = authority(authority_text, Some(80)).ok_or((400, "the URL's host does not parse (or carries credentials)"))?;
    let path = path.split('#').next().unwrap_or_default();
    let path = if path.is_empty() { "/".to_owned() } else if path.starts_with('?') { format!("/{path}") } else { path.to_owned() };
    Ok(Target { host, port, path })
}

/// An address a name may not lead to: this machine's, its link's, none.
fn internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80 || v6.to_ipv4_mapped().is_some_and(|v4| internal(IpAddr::V4(v4))),
    }
}

impl Ctx {
    /// The request's decision when it is allowed, else the refusal's text.
    fn verdict(&self, asked: &Request) -> Result<Decision, String> {
        let Some(policy) = (self.policy)() else { return Err("denied: this sandbox has no egress policy".into()) };
        let decision = decide(&policy, asked);
        if decision.allowed {
            return Ok(decision);
        }
        Err(match &decision.rule {
            Some(rule) => format!("denied by egress rule {}", printable(rule)),
            None => "denied by default".into(),
        })
    }

    /// Records a decision: host, method and path (no query), never a header.
    fn record(&self, asked: &Request, allowed: bool) {
        let path = asked.path.as_deref().and_then(|p| p.split(['?', '#']).next()).unwrap_or_default();
        let path: String = path.chars().take(1024).collect();
        self.recorder.record(EgressDecision { ms: now_ms(), host: asked.host.clone(), method: asked.method.clone(), path, allowed });
    }

    fn dial(&self, host: &str, port: u16) -> Result<TcpStream, String> {
        let literal = host.parse::<IpAddr>().ok();
        let addrs: Vec<SocketAddr> = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => (host, port).to_socket_addrs().map_err(|_| format!("{host} does not resolve"))?.collect(),
        };
        if addrs.iter().any(|a| a.ip().is_unspecified() || (a.port() == self.own.port() && (a.ip().is_loopback() || a.ip() == self.own.ip()))) {
            return Err(format!("{host}:{port} is the egress stand-in itself"));
        }
        // DNS could point a name the policy allows at the hub's own services
        // or a cloud's metadata address: only an address written out may.
        if literal.is_none() && host != "localhost" && addrs.iter().any(|a| internal(a.ip())) {
            return Err(format!("{host} resolves to an address of this machine or its link"));
        }
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(stream) => return Ok(stream),
                Err(error) => last = Some(error.kind()),
            }
        }
        Err(format!("cannot reach {host}:{port}{}", last.map(|kind| format!(" ({kind})")).unwrap_or_default()))
    }
}

/// `CONNECT host:port`: decided on host only, then a tunnel.
fn tunnel(ctx: &Ctx, request: &ClientRequest, reader: BufReader<TcpStream>, mut writer: TcpStream) {
    let Some((host, port)) = authority(&request.target, None) else {
        let _ = answer(&mut writer, 400, "CONNECT takes host:port");
        return;
    };
    let asked = Request { host: host.clone(), port, method: "CONNECT".into(), path: None };
    if let Err(text) = ctx.verdict(&asked) {
        ctx.record(&asked, false);
        let _ = answer(&mut writer, 403, &text);
        return;
    }
    ctx.record(&asked, true);
    let upstream = match ctx.dial(&host, port) {
        Ok(upstream) => upstream,
        Err(text) => {
            let _ = answer(&mut writer, 502, &text);
            return;
        }
    };
    // Nothing can be injected into a tunnel: what passes is the guest's own
    // TLS (or whatever it speaks), so a Transform rule's headers are not set
    // here. The platform's egress, which terminates TLS, sets them; here a
    // guest needing them sends plain http:// through the proxy.
    if writer.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
        return;
    }
    let early = reader.buffer().to_vec();
    drop(reader);
    relay(writer, early, upstream, Vec::new());
}

/// Passes bytes both ways until both sides end, or neither says anything
/// for [`TUNNEL_IDLE`]; `early_*`: bytes already read from one side.
fn relay(client: TcpStream, early_from_client: Vec<u8>, upstream: TcpStream, early_from_upstream: Vec<u8>) {
    let epoch = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    for stream in [&client, &upstream] {
        let _ = stream.set_write_timeout(Some(IDLE));
        let _ = stream.set_read_timeout(Some(TICK));
    }
    if (&upstream).write_all(&early_from_client).is_err() || (&client).write_all(&early_from_upstream).is_err() {
        return;
    }
    let (Ok(c), Ok(u)) = (client.try_clone(), upstream.try_clone()) else { return };
    let seen = last.clone();
    let up = std::thread::Builder::new().name("egress-tunnel".into()).spawn(move || pump(c, u, &seen, epoch));
    pump(upstream, client, &last, epoch);
    if let Ok(up) = up {
        let _ = up.join();
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, last: &AtomicU64, epoch: Instant) {
    let mut buf = vec![0u8; 16 << 10];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                // Half-closed: the other way goes on until it ends too.
                let _ = to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
                last.store(epoch.elapsed().as_millis() as u64, SeqCst);
            }
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if (epoch.elapsed().as_millis() as u64).saturating_sub(last.load(SeqCst)) > TUNNEL_IDLE.as_millis() as u64 {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}

/// The proxy's own short answer (it closes the connection).
fn answer(writer: &mut TcpStream, status: u16, text: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = format!("{text}\n");
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nX-Atelier-Egress: {}\r\n\r\n", body.len(), if status == 403 { "refused" } else { "error" });
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

/// A header's value from its format (`Bearer {}`) and a secret; `None` when
/// either could end the header (a control character) or the format has no
/// place for the value.
fn formatted(format: &str, value: &Secret) -> Option<Secret> {
    let format = if format.is_empty() { "{}" } else { format };
    let (before, after) = format.split_once("{}")?;
    if format.chars().any(char::is_control) || value.expose().iter().any(u8::is_ascii_control) {
        return None;
    }
    let mut out = Vec::with_capacity(before.len() + value.expose().len() + after.len());
    out.extend_from_slice(before.as_bytes());
    out.extend_from_slice(value.expose());
    out.extend_from_slice(after.as_bytes());
    Some(Secret::new(out))
}

/// The request's head as it goes out: origin-form, `Host` from the URL, the
/// guest's end-to-end headers except those a transform sets, the body's
/// framing, then the transform's headers. A [`Secret`] (it holds their
/// values), sized once so that no copy is left behind by growing.
fn request_head(request: &ClientRequest, target: &Target, body: Body, upgrade: Option<&str>, set: &[(&str, Secret)]) -> Secret {
    let named = tokens(&request.headers, "connection");
    let kept: Vec<&(String, String)> = request
        .headers
        .iter()
        .filter(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            !(HOP.contains(&lower.as_str())
                || matches!(lower.as_str(), "host" | "content-length")
                || named.contains(&lower)
                || set.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
                || (lower == "expect" && value.eq_ignore_ascii_case("100-continue")))
        })
        .collect();
    let line = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", request.method, target.path, target.host_header());
    let framing = match body {
        Body::Length(n) => format!("Content-Length: {n}\r\n"),
        Body::Chunked => "Transfer-Encoding: chunked\r\n".to_owned(),
        _ => String::new(),
    };
    let connection = match upgrade {
        Some(protocol) => format!("Connection: Upgrade\r\nUpgrade: {protocol}\r\n"),
        None => "Connection: close\r\n".to_owned(),
    };
    let size = line.len() + framing.len() + connection.len() + 2 + kept.iter().map(|(n, v)| n.len() + v.len() + 4).sum::<usize>() + set.iter().map(|(n, v)| n.len() + v.expose().len() + 4).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(line.as_bytes());
    for (name, value) in kept {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(framing.as_bytes());
    out.extend_from_slice(connection.as_bytes());
    for (name, value) in set {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.expose());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Secret::new(out)
}

/// An answer's head as it goes back: hop-by-hop headers dropped (a
/// chunked coding kept unless `dechunked`), the length written once, then
/// `last`: this connection's own `Connection` (and `Upgrade`) lines.
fn answer_head(status: u16, reason: &str, headers: &[(String, String)], body: Body, dechunked: bool, last: &str) -> Vec<u8> {
    let named = tokens(headers, "connection");
    let reason: String = reason.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(100).collect();
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    let mut lengths = 0;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        let coding = lower == "transfer-encoding" && matches!(body, Body::Chunked | Body::UntilClose) && !dechunked;
        if (HOP.contains(&lower.as_str()) && !coding) || named.contains(&lower) {
            continue;
        }
        if lower == "content-length" {
            // Kept as it is only where it describes no body here (HEAD, 304).
            lengths += 1;
            if body != Body::None || lengths > 1 {
                continue;
            }
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Body::Length(n) = body {
        out.push_str(&format!("Content-Length: {n}\r\n"));
    }
    out.push_str(last);
    out.push_str("\r\n");
    out.into_bytes()
}

/// An absolute-form request (an `Upgrade` among them): decided, its
/// transform's headers set, sent on a connection of its own, its answer relayed.
fn forward(ctx: &Ctx, request: &ClientRequest, reader: &mut BufReader<TcpStream>, writer: &mut TcpStream) -> io::Result<After> {
    let close = |writer: &mut TcpStream, status: u16, text: &str| answer(writer, status, text).map(|_| After::Close);
    let target = match absolute(&request.target) {
        Ok(target) => target,
        Err((status, text)) => return close(writer, status, text),
    };
    let body = match request_body(&request.headers) {
        Ok(body) => body,
        Err((status, text)) => return close(writer, status, text),
    };
    let asked = Request { host: target.host.clone(), port: target.port, method: request.method.clone(), path: Some(target.path.clone()) };
    let decision = match ctx.verdict(&asked) {
        Ok(decision) => decision,
        Err(text) => {
            ctx.record(&asked, false);
            return close(writer, 403, &text);
        }
    };
    // The transform's values, from the secrets this egress point holds.
    let rule = printable(decision.rule.as_deref().unwrap_or_default());
    let mut set = Vec::new();
    for header in &decision.headers {
        let value = ctx.secrets.value(&header.secret, &header.key);
        let Some(value) = value.as_ref().and_then(|v| formatted(&header.format, v)) else {
            ctx.record(&asked, false);
            let why = if value.is_none() { "has no value here yet" } else { "cannot be a header's value" };
            return close(writer, 503, &format!("egress rule {rule}: secret {} {why}", printable(&header.secret)));
        };
        set.push((header.name.as_str(), value));
    }
    ctx.record(&asked, true);
    let mut upstream = match ctx.dial(&target.host, target.port) {
        Ok(upstream) => upstream,
        Err(text) => return close(writer, 502, &text),
    };
    let upgrade = values(&request.headers, "upgrade").next().filter(|_| tokens(&request.headers, "connection").iter().any(|t| t == "upgrade"));
    let head = request_head(request, &target, body, upgrade, &set);
    drop(set);
    let _ = upstream.set_nodelay(true);
    upstream.set_write_timeout(Some(IDLE))?;
    // The head straight to the socket (a buffer would keep a copy of the values).
    let sent = upstream.write_all(head.expose());
    drop(head);
    if sent.is_err() {
        return close(writer, 502, &format!("cannot send to {}:{}", target.host, target.port));
    }
    if request.http11 && values(&request.headers, "expect").any(|v| v.eq_ignore_ascii_case("100-continue")) {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut to_upstream = BufWriter::new(upstream.try_clone()?);
    if copy_body(reader, &mut to_upstream, body, false).and_then(|_| to_upstream.flush()).is_err() {
        return close(writer, 502, "the request's body did not reach its host");
    }
    drop(to_upstream);
    let mut from_upstream = BufReader::with_capacity(16 << 10, upstream.try_clone()?);
    let (status, reason, headers) = loop {
        let parsed = match read_head(&mut from_upstream, MAX_ANSWER_HEAD, IDLE) {
            Ok(Some(head)) => parse_head(&head).ok(),
            _ => None,
        };
        let answered = parsed.and_then(|head| {
            let mut parts = head.line.splitn(3, ' ');
            let (version, status) = (parts.next()?, parts.next()?);
            let status = Some(status).filter(|s| s.len() == 3).and_then(|s| s.parse::<u16>().ok()).filter(|s| (100..600).contains(s))?;
            version.starts_with("HTTP/1.").then(|| (status, parts.next().unwrap_or_default().to_owned(), head.headers))
        });
        let Some((status, reason, headers)) = answered else {
            return close(writer, 502, &format!("no answer from {}:{}", target.host, target.port));
        };
        if (100..200).contains(&status) && status != 101 {
            if request.http11 {
                writer.write_all(&answer_head(status, &reason, &headers, Body::None, false, ""))?;
            }
            continue;
        }
        break (status, reason, headers);
    };
    if status == 101 {
        let Some(protocol) = upgrade else { return close(writer, 502, "the host switched protocols unasked") };
        let protocol = values(&headers, "upgrade").next().unwrap_or(protocol).to_owned();
        writer.write_all(&answer_head(101, &reason, &headers, Body::None, false, &format!("Connection: Upgrade\r\nUpgrade: {protocol}\r\n")))?;
        return Ok(After::Upgraded(upstream, from_upstream.buffer().to_vec()));
    }
    let body = answer_body(&request.method, status, &headers);
    let keep = request.http11 && upgrade.is_none() && !tokens(&request.headers, "connection").iter().any(|t| t == "close") && body != Body::UntilClose;
    let dechunk = !request.http11 && body == Body::Chunked;
    let last = if keep { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
    let mut to_client = BufWriter::new(writer.try_clone()?);
    to_client.write_all(&answer_head(status, &reason, &headers, body, dechunk, last))?;
    copy_body(&mut from_upstream, &mut to_client, body, dechunk)?;
    to_client.flush()?;
    Ok(if keep { After::KeepAlive } else { After::Close })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::provider::{EgressRule, HeaderFromSecret};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const SECRET: &str = "s3cret-forge-token-0f1e2d";

    struct Values(HashMap<(String, String), String>);

    impl Secrets for Values {
        fn value(&self, name: &str, key: &str) -> Option<Secret> {
            self.0.get(&(name.to_owned(), key.to_owned())).map(|v| Secret::new(v.as_bytes().to_vec()))
        }
    }

    #[derive(Default)]
    struct Kept(Mutex<Vec<EgressDecision>>);

    impl Recorder for Kept {
        fn record(&self, decision: EgressDecision) {
            self.0.lock().unwrap().push(decision);
        }
    }

    /// A proxy on loopback whose policy a test changes.
    struct Proxy {
        port: u16,
        policy: Arc<Mutex<Option<EgressPolicy>>>,
        kept: Arc<Kept>,
    }

    impl Proxy {
        fn start(policy: Option<EgressPolicy>) -> Proxy {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let shared = Arc::new(Mutex::new(policy));
            let read = shared.clone();
            let kept = Arc::new(Kept::default());
            let values = HashMap::from([(("forge-token".to_owned(), "value".to_owned()), SECRET.to_owned()), (("broken".to_owned(), "value".to_owned()), "x\r\nX-Smuggled: 1".to_owned())]);
            let (recorder, secrets): (Arc<dyn Recorder>, Arc<dyn Secrets>) = (kept.clone(), Arc::new(Values(values)));
            std::thread::spawn(move || super::super::serve(listener, Arc::new(move || read.lock().unwrap().clone()), secrets, recorder));
            Proxy { port, policy: shared, kept }
        }

        fn set(&self, policy: Option<EgressPolicy>) {
            *self.policy.lock().unwrap() = policy;
        }

        /// (host, method, path, allowed) of every decision so far.
        fn decisions(&self) -> Vec<(String, String, String, bool)> {
            self.kept.0.lock().unwrap().iter().map(|d| (d.host.clone(), d.method.clone(), d.path.clone(), d.allowed)).collect()
        }
    }

    fn decision(host: &str, method: &str, path: &str, allowed: bool) -> (String, String, String, bool) {
        (host.into(), method.into(), path.into(), allowed)
    }

    /// A local host that records every request it gets (head and body) and
    /// answers `saw <request line>`; `/chunked` answers chunked with a
    /// trailer; a WebSocket upgrade is switched and then echoed.
    struct Upstream {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Upstream {
        fn start() -> Upstream {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let log = log.clone();
                    std::thread::spawn(move || Upstream::exchange(stream, &log));
                }
            });
            Upstream { port, seen }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn exchange(stream: TcpStream, log: &Mutex<Vec<String>>) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let lower = head.to_ascii_lowercase();
            let mut body = Vec::new();
            if let Some(n) = lower.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()) {
                body.resize(n, 0);
                let _ = reader.read_exact(&mut body);
            } else if lower.contains("transfer-encoding: chunked") {
                let mut byte = [0u8; 1];
                while !body.ends_with(b"0\r\n\r\n") && reader.read(&mut byte).unwrap_or(0) == 1 {
                    body.push(byte[0]);
                }
            }
            log.lock().unwrap().push(format!("{head}{}", String::from_utf8_lossy(&body)));
            let first = head.lines().next().unwrap_or_default().to_owned();
            if lower.contains("upgrade: websocket") {
                let _ = writer.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
                let _ = io::copy(&mut reader, &mut writer);
            } else if first.contains(" /chunked") {
                let _ = writer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\nX-Trailer: t\r\n\r\n");
            } else {
                let text = format!("saw {first}");
                let _ = writer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nX-Upstream: yes\r\n\r\n{text}", text.len()).as_bytes());
            }
        }
    }

    fn connect(port: u16) -> (BufReader<TcpStream>, TcpStream) {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        (BufReader::new(stream.try_clone().unwrap()), stream)
    }

    /// One answer: its status, its head and its body (by its Content-Length,
    /// none for a `HEAD`).
    fn read_answer(reader: &mut BufReader<TcpStream>, head_only: bool) -> (u16, String, String) {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed after {head:?}");
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let length = head.to_ascii_lowercase().lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
        let mut body = vec![0; if head_only { 0 } else { length }];
        reader.read_exact(&mut body).unwrap();
        (status, head, String::from_utf8(body).unwrap())
    }

    /// A request on a connection of its own: (status, head, body).
    fn ask(port: u16, request: &str) -> (u16, String, String) {
        let (mut reader, mut writer) = connect(port);
        writer.write_all(request.as_bytes()).unwrap();
        read_answer(&mut reader, false)
    }

    /// Everything the proxy sends back to a request, to its close.
    fn ask_raw(port: u16, request: &str) -> String {
        let (mut reader, mut writer) = connect(port);
        writer.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        out
    }

    fn rule(name: &str, host: &str, path: Option<&str>, methods: &[&str], action: &str) -> EgressRule {
        EgressRule { name: name.into(), host: host.into(), path: path.map(str::to_owned), methods: methods.iter().map(|m| m.to_string()).collect(), action: action.into(), headers: Vec::new() }
    }

    fn bearer(name: &str, host: &str, path: Option<&str>, secret: &str) -> EgressRule {
        EgressRule { headers: vec![HeaderFromSecret { name: "Authorization".into(), secret: secret.into(), key: "value".into(), format: "Bearer {}".into() }], ..rule(name, host, path, &[], "Transform") }
    }

    fn deny(rules: Vec<EgressRule>) -> Option<EgressPolicy> {
        Some(EgressPolicy { default_action: "Deny".into(), rules })
    }

    #[test]
    fn allowed_requests_pass_and_refused_ones_are_recorded_and_never_sent() {
        let up = Upstream::start();
        let u = up.port;
        let proxy = Proxy::start(deny(vec![rule("no-admin", "127.0.0.1", Some("/ok/admin"), &[], "Deny"), rule("ok", "127.0.0.1", Some("/ok/*"), &["GET", "POST"], "Allow")]));
        let (status, head, body) = ask(proxy.port, &format!("GET http://127.0.0.1:{u}/ok/x?q=1 HTTP/1.1\r\nHost: elsewhere.example.com\r\nConnection: close\r\n\r\n"));
        assert_eq!((status, body.as_str()), (200, "saw GET /ok/x?q=1 HTTP/1.1"));
        assert!(head.contains("X-Upstream: yes") && head.contains("Connection: close"), "{head}");
        assert!(up.seen()[0].contains(&format!("Host: 127.0.0.1:{u}\r\n")) && !up.seen()[0].contains("elsewhere"), "Host comes from the URL: {:?}", up.seen());
        let (status, _, _) = ask(proxy.port, &format!("POST http://127.0.0.1:{u}/ok/y HTTP/1.1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"));
        assert_eq!(status, 200);
        assert!(up.seen()[1].ends_with("Content-Length: 5\r\nConnection: close\r\n\r\nhello"), "{:?}", up.seen()[1]);

        // A refused method, a refused host (never looked up), a deny naming itself.
        let (status, head, body) = ask(proxy.port, &format!("DELETE http://127.0.0.1:{u}/ok/x HTTP/1.1\r\n\r\n"));
        assert_eq!((status, body.as_str()), (403, "denied by default\n"));
        assert!(head.contains("X-Atelier-Egress: refused"));
        assert_eq!(ask(proxy.port, "GET http://denied.example.com/ok/x HTTP/1.1\r\n\r\n").2, "denied by default\n");
        let (status, _, body) = ask(proxy.port, &format!("POST http://127.0.0.1:{u}/ok/admin HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc"));
        assert_eq!((status, body.as_str()), (403, "denied by egress rule no-admin\n"));
        assert_eq!(up.seen().len(), 2, "a refused request never reaches a host");
        assert_eq!(
            proxy.decisions(),
            vec![
                decision("127.0.0.1", "GET", "/ok/x", true),
                decision("127.0.0.1", "POST", "/ok/y", true),
                decision("127.0.0.1", "DELETE", "/ok/x", false),
                decision("denied.example.com", "GET", "/ok/x", false),
                decision("127.0.0.1", "POST", "/ok/admin", false),
            ]
        );
    }

    #[test]
    fn a_transform_sets_its_headers_over_the_guests_and_the_value_shows_nowhere_else() {
        let up = Upstream::start();
        let u = up.port;
        let policy = deny(vec![bearer("forge", "127.0.0.1", Some("/api"), "forge-token"), bearer("missing", "127.0.0.1", Some("/missing"), "nope"), bearer("broken", "127.0.0.1", Some("/broken"), "broken")]);
        let proxy = Proxy::start(policy.clone());
        let request = format!("GET http://127.0.0.1:{u}/api/repos HTTP/1.1\r\nAuthorization: Bearer guest-value\r\nauthorization: Basic Z3Vlc3Q=\r\nProxy-Authorization: Basic cHJveHk=\r\nX-Keep: 1\r\nConnection: close\r\n\r\n");
        let (status, head, body) = ask(proxy.port, &request);
        assert_eq!(status, 200);
        let seen = up.seen()[0].clone();
        let auth: Vec<&str> = seen.lines().filter(|l| l.to_ascii_lowercase().starts_with("authorization:")).collect();
        assert_eq!(auth, [format!("Authorization: Bearer {SECRET}")], "set once, the guest's replaced: {seen}");
        assert!(!seen.contains("guest-value") && !seen.contains("Z3Vlc3Q") && !seen.contains("Proxy-Authorization") && seen.contains("X-Keep: 1\r\n"), "{seen}");

        // A secret the egress point does not hold, or one that would end its header: nothing goes out.
        let (status, _, missing) = ask(proxy.port, &format!("GET http://127.0.0.1:{u}/missing HTTP/1.1\r\n\r\n"));
        assert_eq!((status, missing.as_str()), (503, "egress rule missing: secret nope has no value here yet\n"));
        let (status, _, broken) = ask(proxy.port, &format!("GET http://127.0.0.1:{u}/broken HTTP/1.1\r\n\r\n"));
        assert_eq!((status, broken.as_str()), (503, "egress rule broken: secret broken cannot be a header's value\n"));
        assert_eq!(up.seen().len(), 1);
        assert!(!up.seen()[0].contains("X-Smuggled"));
        assert_eq!(proxy.decisions().iter().map(|d| d.3).collect::<Vec<_>>(), [true, false, false]);

        // The value is in no answer to the guest, no recorded decision, no Debug.
        for text in [head, body, missing, broken] {
            assert!(!text.contains(SECRET), "{text}");
        }
        let kept = proxy.kept.0.lock().unwrap().clone();
        assert!(!format!("{kept:?}").contains(SECRET) && !serde_json::to_string(&kept).unwrap().contains(SECRET));
        let decided = decide(policy.as_ref().unwrap(), &Request { host: "127.0.0.1".into(), port: u, method: "GET".into(), path: Some("/api".into()) });
        assert!(decided.allowed && decided.headers.len() == 1 && !format!("{decided:?}").contains(SECRET));
        let value = Secret::new(SECRET.as_bytes().to_vec());
        assert!(!format!("{:?}", formatted("Bearer {}", &value).unwrap()).contains(SECRET));
    }

    #[test]
    fn connect_is_decided_on_host_only_and_tunnels_without_injection() {
        let up = Upstream::start();
        let u = up.port;
        let proxy = Proxy::start(deny(vec![bearer("hub", "127.0.0.1", None, "forge-token"), rule("api", "api.example.com", Some("/x"), &[], "Allow")]));
        let (mut reader, mut writer) = connect(proxy.port);
        writer.write_all(format!("CONNECT 127.0.0.1:{u} HTTP/1.1\r\nHost: 127.0.0.1:{u}\r\n\r\n").as_bytes()).unwrap();
        let (status, head, _) = read_answer(&mut reader, false);
        assert_eq!(status, 200, "{head}");
        writer.write_all(b"GET /through HTTP/1.1\r\nHost: tunnel\r\nConnection: close\r\n\r\n").unwrap();
        assert_eq!(read_answer(&mut reader, false).2, "saw GET /through HTTP/1.1");
        assert!(!up.seen()[0].contains("Authorization"), "nothing is injected into a tunnel");
        // A host no rule names, and one whose only rule has a path, are refused.
        for target in ["denied.example.com:443", "api.example.com:443"] {
            let (status, _, body) = ask(proxy.port, &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"));
            assert_eq!((status, body.as_str()), (403, "denied by default\n"), "{target}");
        }
        assert_eq!(ask(proxy.port, "CONNECT api.example.com HTTP/1.1\r\n\r\n").0, 400, "CONNECT names a port");
        assert_eq!(proxy.decisions(), vec![decision("127.0.0.1", "CONNECT", "", true), decision("denied.example.com", "CONNECT", "", false), decision("api.example.com", "CONNECT", "", false)]);
    }

    #[test]
    fn an_upgrade_passes() {
        let up = Upstream::start();
        let u = up.port;
        let proxy = Proxy::start(deny(vec![rule("ws", "127.0.0.1", Some("/ws"), &["GET"], "Allow")]));
        let upgrade = |path: &str| format!("GET http://127.0.0.1:{u}{path} HTTP/1.1\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
        let (mut reader, mut writer) = connect(proxy.port);
        writer.write_all(upgrade("/ws").as_bytes()).unwrap();
        let (status, head, _) = read_answer(&mut reader, true);
        assert_eq!(status, 101);
        assert!(head.contains("Connection: Upgrade\r\nUpgrade: websocket\r\n"), "{head}");
        writer.write_all(b"ping").unwrap();
        let mut echo = [0u8; 4];
        reader.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ping");
        let seen = &up.seen()[0];
        assert!(seen.contains("Connection: Upgrade\r\nUpgrade: websocket\r\n") && seen.contains("Sec-WebSocket-Key:") && !seen.contains("keep-alive"), "{seen}");
        assert_eq!(ask(proxy.port, &upgrade("/other")).0, 403);
        assert_eq!(up.seen().len(), 1);
    }

    #[test]
    fn a_changed_policy_applies_to_the_next_request() {
        let up = Upstream::start();
        let u = up.port;
        let open = deny(vec![rule("local", "127.0.0.1", None, &[], "Allow")]);
        let proxy = Proxy::start(open.clone());
        let (mut reader, mut writer) = connect(proxy.port);
        let get = format!("GET http://127.0.0.1:{u}/a HTTP/1.1\r\n\r\n");
        writer.write_all(format!("HEAD http://127.0.0.1:{u}/a HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
        let (status, head, _) = read_answer(&mut reader, true);
        assert!(status == 200 && head.contains("Content-Length: ") && head.contains("Connection: keep-alive"), "a HEAD keeps its length and no body: {head}");
        writer.write_all(get.as_bytes()).unwrap();
        assert_eq!(read_answer(&mut reader, false).2, "saw GET /a HTTP/1.1", "the same connection, kept alive");
        proxy.set(deny(Vec::new()));
        writer.write_all(get.as_bytes()).unwrap();
        assert_eq!(read_answer(&mut reader, false).2, "denied by default\n", "the new policy, on the same connection");
        proxy.set(None);
        assert_eq!(ask(proxy.port, &get).2, "denied: this sandbox has no egress policy\n");
        proxy.set(open);
        assert_eq!(ask(proxy.port, &get).0, 200);
        assert_eq!(proxy.decisions().iter().map(|d| d.3).collect::<Vec<_>>(), [true, true, false, false, true]);
    }

    #[test]
    fn bodies_pass_as_they_came() {
        let up = Upstream::start();
        let u = up.port;
        let proxy = Proxy::start(deny(vec![rule("local", "127.0.0.1", None, &[], "Allow")]));
        let (mut reader, mut writer) = connect(proxy.port);
        writer.write_all(format!("POST http://127.0.0.1:{u}/c HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n4\r\nwiki\r\n0\r\nX-Sneaky: 1\r\n\r\n").as_bytes()).unwrap();
        assert_eq!(read_answer(&mut reader, false).0, 100, "Expect: 100-continue is answered by the proxy");
        assert_eq!(read_answer(&mut reader, false).2, "saw POST /c HTTP/1.1");
        let seen = &up.seen()[0];
        assert!(seen.ends_with("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nwiki\r\n0\r\n\r\n") && !seen.contains("Expect") && !seen.contains("Sneaky"), "{seen:?}");
        // A chunked answer passes as it is to HTTP/1.1 (its trailer dropped), its bytes alone to HTTP/1.0.
        let eleven = ask_raw(proxy.port, &format!("GET http://127.0.0.1:{u}/chunked HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(eleven.contains("Transfer-Encoding: chunked\r\n") && eleven.ends_with("\r\n\r\n5\r\nhello\r\n0\r\n\r\n") && !eleven.contains("X-Trailer"), "{eleven:?}");
        let ten = ask_raw(proxy.port, &format!("GET http://127.0.0.1:{u}/chunked HTTP/1.0\r\n\r\n"));
        assert!(!ten.contains("Transfer-Encoding") && ten.contains("Connection: close\r\n") && ten.ends_with("\r\n\r\nhello"), "{ten:?}");
    }

    #[test]
    fn bad_and_oversized_requests_are_refused() {
        let proxy = Proxy::start(Some(EgressPolicy { default_action: "Allow".into(), rules: Vec::new() }));
        let me = proxy.port;
        let cases = [
            ("GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_owned(), 400),
            ("POST http://127.0.0.1:1/ HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n".into(), 400),
            ("POST http://127.0.0.1:1/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".into(), 400),
            ("POST http://127.0.0.1:1/ HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n".into(), 501),
            ("GET https://example.com/ HTTP/1.1\r\n\r\n".into(), 501),
            ("GET ftp://example.com/ HTTP/1.1\r\n\r\n".into(), 400),
            ("GET http://user:pw@example.com/ HTTP/1.1\r\n\r\n".into(), 400),
            ("GET http://example.com/ HTTP/1.1\r\nX-A: 1\r\n folded\r\n\r\n".into(), 400),
            ("GET http://example.com/ HTTP/1.1\r\nX A: 1\r\n\r\n".into(), 400),
            ("GET http://example.com/ HTTP/2\r\n\r\n".into(), 400),
            ("GET  http://example.com/ HTTP/1.1\r\n\r\n".into(), 400),
            (format!("GET http://example.com/ HTTP/1.1\r\nX-Big: {}\r\n\r\n", "a".repeat(40 << 10)), 431),
            (format!("GET http://127.0.0.1:{me}/ HTTP/1.1\r\n\r\n"), 502),
            (format!("GET http://localhost:{me}/ HTTP/1.1\r\n\r\n"), 502),
            ("GET http://0.0.0.0:1/ HTTP/1.1\r\n\r\n".into(), 502),
            (format!("CONNECT 127.0.0.1:{me} HTTP/1.1\r\n\r\n"), 502),
        ];
        for (request, want) in cases {
            let (status, _, body) = ask(me, &request);
            assert_eq!(status, want, "{:?}: {body}", &request[..request.len().min(60)]);
        }
    }

    #[test]
    fn parts() {
        assert_eq!(authority("Example.COM:8080", None), Some(("example.com".into(), 8080)));
        assert_eq!(authority("[::1]", Some(80)), Some(("::1".into(), 80)));
        assert_eq!(authority("example.com.", Some(80)), Some(("example.com.".into(), 80)));
        for bad in ["", "example.com", "a@b:1", "a b:1", "x:0", "x:65536", "x:+1", "[::1]x", "[nope]:1", "a/b:1"] {
            assert_eq!(authority(bad, None), None, "{bad}");
        }
        let target = absolute("http://Example.com?q=1#frag").unwrap();
        assert_eq!((target.host.as_str(), target.port, target.path.as_str(), target.host_header()), ("example.com", 80, "/?q=1", "example.com".to_owned()));
        assert_eq!(absolute("http://[::1]:8080/x").unwrap().host_header(), "[::1]:8080");
        for ip in ["127.0.0.1", "169.254.169.254", "0.0.0.0", "255.255.255.255", "224.0.0.1", "::1", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["10.0.0.1", "192.168.1.1", "93.184.216.34", "2001:db8::1"] {
            assert!(!internal(ip.parse().unwrap()), "{ip}");
        }
        let value = Secret::new(b"t0k".to_vec());
        assert_eq!(formatted("Bearer {}", &value).unwrap().expose(), b"Bearer t0k");
        assert_eq!(formatted("", &value).unwrap().expose(), b"t0k");
        assert!(formatted("Bearer", &value).is_none() && formatted("a\r\n{}", &value).is_none() && formatted("{}", &Secret::new(b"a\nb".to_vec())).is_none());
        let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect::<Vec<_>>();
        assert_eq!(answer_body("GET", 200, &h(&[("Content-Length", "3"), ("content-length", "3")])), Body::Length(3));
        assert_eq!(answer_body("GET", 200, &h(&[("Content-Length", "3"), ("content-length", "4")])), Body::UntilClose);
        assert_eq!(answer_body("GET", 200, &h(&[("Transfer-Encoding", "gzip, chunked"), ("Content-Length", "3")])), Body::Chunked);
        assert_eq!(answer_body("HEAD", 200, &h(&[("Content-Length", "3")])), Body::None);
        assert_eq!((answer_body("GET", 204, &[]), answer_body("GET", 200, &[])), (Body::None, Body::UntilClose));
        assert_eq!(request_body(&h(&[("Content-Length", " 12 ")])), Ok(Body::Length(12)));
        assert_eq!(request_body(&h(&[("Content-Length", "-1")])), Err((400, "Content-Length is not a number")));
    }
}
