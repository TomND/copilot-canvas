//! Minimal HTTP/1.1 client for the server inside an OpenCode TUI (`--port`
//! with `OPENCODE_SERVER_PASSWORD` basic auth). It reports what OpenCode does
//! not store: busy sessions, pending permission prompts and questions.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Basic-auth user name OpenCode expects unless `OPENCODE_SERVER_USERNAME` says otherwise.
pub const USERNAME: &str = "opencode";
const TIMEOUT: Duration = Duration::from_millis(500);
const DEADLINE: Duration = Duration::from_secs(2);
const MAX_BODY: usize = 16 * 1024 * 1024;

/// A session as the server lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSession {
    pub id: String,
    pub parent: Option<String>,
    pub directory: String,
    pub created_ms: u64,
    pub title: String,
}

/// Live state of one OpenCode instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Live {
    /// Sessions that are not idle: id → "busy" or "retry" (idle ones are absent).
    pub status: HashMap<String, String>,
    /// Pending permission prompts: (session, request id).
    pub permissions: Vec<(String, String)>,
    /// Pending questions: (session, request id).
    pub questions: Vec<(String, String)>,
    /// Root sessions of the instance's directory, newest first.
    pub sessions: Vec<ServerSession>,
}

impl Live {
    /// Sessions waiting for the user (permission prompts and questions).
    pub fn waiting(&self) -> impl Iterator<Item = &str> {
        self.permissions.iter().chain(&self.questions).map(|(session, _)| session.as_str())
    }
}

/// Status, permissions, questions and recent root sessions of the server.
pub fn live(port: u16, password: &str) -> Result<Live> {
    let directory = directory(port, password)?;
    live_with(port, password, &directory, true)
}

/// The folder the TUI works in (`GET /path`).
pub fn directory(port: u16, password: &str) -> Result<String> {
    let path = get_json(port, password, "/path")?;
    path["directory"].as_str().filter(|d| !d.is_empty()).map(str::to_owned).context("GET /path: no directory")
}

/// `path` scoped to the TUI's own folder. OpenCode's server keeps one
/// instance per folder; a request without one starts a separate default
/// instance (project scan, MCP connections), and a TUI asked to quit while
/// that is still starting fails with an unhandled `AbortError` and hangs.
fn scoped(path: &str, directory: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}directory={}", percent_encoding::utf8_percent_encode(directory, percent_encoding::NON_ALPHANUMERIC))
}

/// Like [`live`] for a TUI in `directory`; `sessions` also lists the
/// instance's recent root sessions.
pub fn live_with(port: u16, password: &str, directory: &str, sessions: bool) -> Result<Live> {
    let status = get_json(port, password, &scoped("/session/status", directory))?;
    let mut live = Live::default();
    if let Some(map) = status.as_object() {
        for (id, value) in map {
            let kind = value["type"].as_str().unwrap_or("busy");
            if kind != "idle" {
                live.status.insert(id.clone(), kind.to_owned());
            }
        }
    }
    live.permissions = requests(&get_json(port, password, &scoped("/permission", directory))?);
    // Older servers have no question endpoint.
    live.questions = get_json(port, password, &scoped("/question", directory)).map(|v| requests(&v)).unwrap_or_default();
    if sessions {
        live.sessions = get_json(port, password, &scoped("/session?roots=true&limit=20", directory)).map(|v| parse_sessions(&v)).unwrap_or_default();
    }
    Ok(live)
}

/// (session, request id) of pending permission or question requests.
fn requests(value: &serde_json::Value) -> Vec<(String, String)> {
    let Some(items) = value.as_array() else { return Vec::new() };
    items.iter().filter_map(|r| Some((r["sessionID"].as_str()?.to_owned(), r["id"].as_str().unwrap_or_default().to_owned()))).collect()
}

fn parse_sessions(value: &serde_json::Value) -> Vec<ServerSession> {
    let Some(items) = value.as_array() else { return Vec::new() };
    let mut sessions: Vec<ServerSession> = items
        .iter()
        .filter_map(|s| {
            Some(ServerSession {
                id: s["id"].as_str()?.to_owned(),
                parent: s["parentID"].as_str().map(str::to_owned),
                directory: s["directory"].as_str().unwrap_or_default().to_owned(),
                created_ms: s["time"]["created"].as_u64().unwrap_or(0),
                title: s["title"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
    sessions
}

/// GET `path` and parse the JSON body; non-2xx statuses are errors.
pub fn get_json(port: u16, password: &str, path: &str) -> Result<serde_json::Value> {
    let (status, body) = get(port, password, path)?;
    if !(200..300).contains(&status) {
        bail!("GET {path}: HTTP {status}");
    }
    serde_json::from_slice(&body).with_context(|| format!("GET {path}: invalid JSON"))
}

/// Ports the canvas stopped talking to (their TUI is quitting) and the
/// number of requests in flight per port.
/// (paused ports, requests in flight, when the last request to a port ended).
type Traffic = (std::collections::BTreeSet<u16>, std::collections::BTreeMap<u16, u32>, std::collections::BTreeMap<u16, Instant>);
static TRAFFIC: std::sync::Mutex<Traffic> =
    std::sync::Mutex::new((std::collections::BTreeSet::new(), std::collections::BTreeMap::new(), std::collections::BTreeMap::new()));

/// How long a server gets after the last request ended to finish with that
/// connection before its TUI is asked to quit.
const SETTLE: Duration = Duration::from_millis(300);

fn traffic() -> std::sync::MutexGuard<'static, Traffic> {
    TRAFFIC.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts one request in flight to a port while it lives.
struct InFlight(u16);

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut traffic = traffic();
        traffic.2.insert(self.0, Instant::now());
        if let Some(count) = traffic.1.get_mut(&self.0) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                traffic.1.remove(&self.0);
            }
        }
    }
}

/// Stops all requests to `port` and waits (at most `max`) until those in
/// flight have finished and the server had `SETTLE` to close them. OpenCode
/// aborts the handlers of connections it is still finishing when it quits and
/// then exits with an unhandled `AbortError` (exit code 1, shown as failed),
/// so a TUI is only asked to quit once nobody has talked to its server for a
/// moment.
pub fn quiesce(port: u16, max: Duration) {
    traffic().0.insert(port);
    let deadline = Instant::now() + max;
    loop {
        let busy = {
            let traffic = traffic();
            traffic.1.contains_key(&port) || traffic.2.get(&port).is_some_and(|ended| ended.elapsed() < SETTLE)
        };
        if !busy || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A new server on `port` (ports are reused): requests are allowed again.
pub fn resume(port: u16) {
    let mut traffic = traffic();
    traffic.0.remove(&port);
    traffic.2.remove(&port);
}

/// GET `path` from `127.0.0.1:port`: (status code, body).
pub fn get(port: u16, password: &str, path: &str) -> Result<(u16, Vec<u8>)> {
    let _in_flight = {
        let mut traffic = traffic();
        if traffic.0.contains(&port) {
            bail!("127.0.0.1:{port} is quitting");
        }
        *traffic.1.entry(port).or_insert(0) += 1;
        InFlight(port)
    };
    let started = Instant::now();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, TIMEOUT).with_context(|| format!("cannot connect to 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let auth = base64(format!("{USERNAME}:{password}").as_bytes());
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Basic {auth}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut header_end = None;
    let mut parsed: Option<(u16, Framing)> = None;
    let mut chunks = Dechunker::default();
    loop {
        if started.elapsed() > DEADLINE {
            bail!("GET {path}: timed out");
        }
        let n = match stream.read(&mut buffer) {
            Ok(n) => n,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => bail!("GET {path}: timed out"),
            Err(error) => return Err(error.into()),
        };
        data.extend_from_slice(&buffer[..n]);
        anyhow::ensure!(data.len() <= MAX_BODY, "GET {path}: response too large");
        if header_end.is_none()
            && let Some(pos) = find(&data, b"\r\n\r\n")
        {
            header_end = Some(pos + 4);
            parsed = Some(parse_head(&data[..pos])?);
        }
        if let (Some(start), Some((status, framing))) = (header_end, parsed) {
            let body = &data[start..];
            match framing {
                Framing::Length(len) if body.len() >= len => return Ok((status, body[..len].to_vec())),
                Framing::Chunked if chunks.feed(body, MAX_BODY).with_context(|| format!("GET {path}"))? => return Ok((status, chunks.out)),
                _ => {}
            }
            if n == 0 {
                return match framing {
                    Framing::Close => Ok((status, body.to_vec())),
                    _ => bail!("GET {path}: connection closed early"),
                };
            }
        } else if n == 0 {
            bail!("GET {path}: connection closed without a response");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Framing {
    Length(usize),
    Chunked,
    Close,
}

fn parse_head(head: &[u8]) -> Result<(u16, Framing)> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).context("malformed HTTP status line")?;
    let mut framing = Framing::Close;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
            framing = Framing::Chunked;
        } else if name.eq_ignore_ascii_case("content-length") && framing != Framing::Chunked {
            framing = Framing::Length(value.parse().context("malformed Content-Length")?);
        }
    }
    Ok((status, framing))
}

/// Incremental `Transfer-Encoding: chunked` decoder: each call continues
/// where the previous one stopped, so a body is parsed once however many
/// reads it takes.
#[derive(Debug, Default)]
struct Dechunker {
    out: Vec<u8>,
    /// Bytes of the body consumed so far.
    pos: usize,
    state: Chunk,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Chunk {
    /// Expecting a size line.
    #[default]
    Size,
    /// Expecting this many data bytes and their CRLF.
    Data(usize),
    /// The last chunk arrived.
    Done,
}

impl Dechunker {
    /// Consumes what it can of `body` (everything received so far); `true`
    /// once the final chunk arrived. Sizes are checked against `limit`.
    fn feed(&mut self, body: &[u8], limit: usize) -> Result<bool> {
        loop {
            let rest = &body[self.pos..];
            match self.state {
                Chunk::Done => return Ok(true),
                Chunk::Size => {
                    let Some(end) = find(rest, b"\r\n") else {
                        anyhow::ensure!(rest.len() <= 1024, "malformed chunk size line");
                        return Ok(false);
                    };
                    let line = std::str::from_utf8(&rest[..end]).context("malformed chunk size")?;
                    let size = usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16).context("malformed chunk size")?;
                    anyhow::ensure!(self.out.len().checked_add(size).is_some_and(|total| total <= limit), "chunked body too large");
                    self.pos += end + 2;
                    self.state = if size == 0 { Chunk::Done } else { Chunk::Data(size) };
                }
                Chunk::Data(size) => {
                    // `size` <= limit, so this cannot overflow; checked anyway.
                    let needed = size.checked_add(2).context("chunk size overflow")?;
                    if rest.len() < needed {
                        return Ok(false);
                    }
                    anyhow::ensure!(&rest[size..needed] == b"\r\n", "malformed chunk");
                    self.out.extend_from_slice(&rest[..size]);
                    self.pos += needed;
                    self.state = Chunk::Size;
                }
            }
        }
    }
}
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiescing_port_refuses_requests_and_waits_for_those_in_flight() {
        let port = serve(vec![("/slow", length("{}"))]);
        let worker = std::thread::spawn(move || get(port, "pw", "/slow").is_ok());
        std::thread::sleep(Duration::from_millis(20));
        quiesce(port, Duration::from_secs(2));
        assert!(!traffic().1.contains_key(&port), "nothing in flight after quiesce");
        assert!(traffic().2.get(&port).is_none_or(|ended| ended.elapsed() >= SETTLE), "the server had time to close the connection");
        assert!(get(port, "pw", "/slow").is_err(), "a quitting server gets no new requests");
        let _ = worker.join();
        resume(port);
        assert!(!traffic().0.contains(&port));
    }
    use std::net::TcpListener;

    /// Serves canned responses (one per connection) and returns the port.
    fn serve(responses: Vec<(&'static str, Vec<u8>)>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let expected = format!("Authorization: Basic {}", base64(b"opencode:pw"));
            for (path, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while find(&request, b"\r\n\r\n").is_none() {
                    let n = stream.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&request);
                let reply = if text.starts_with(&format!("GET {path} ")) && text.contains(&expected) {
                    response
                } else {
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                // Split writes exercise partial reads.
                let (a, b) = reply.split_at(reply.len() / 2);
                stream.write_all(a).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(5));
                stream.write_all(b).unwrap();
            }
        });
        port
    }

    fn length(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn chunked(body: &str) -> Vec<u8> {
        let (a, b) = body.split_at(body.len() / 2);
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x};ext=1\r\n{b}\r\n0\r\n\r\n", a.len(), b.len()).into_bytes()
    }

    #[test]
    fn live_reads_status_permissions_questions_and_sessions_of_the_tuis_folder() {
        let port = serve(vec![
            ("/path", length(r#"{"home":"C:\\Users\\u","directory":"C:\\r x"}"#)),
            ("/session/status?directory=C%3A%5Cr%20x", length(r#"{"ses_a":{"type":"busy"},"ses_b":{"type":"idle"},"ses_c":{"type":"retry","attempt":1,"message":"x","next":5}}"#)),
            ("/permission?directory=C%3A%5Cr%20x", chunked(r#"[{"id":"per_1","sessionID":"ses_a","permission":"bash","patterns":["git push"],"metadata":{},"always":[]}]"#)),
            ("/question?directory=C%3A%5Cr%20x", length(r#"[{"id":"que_1","sessionID":"ses_d","questions":[]}]"#)),
            ("/session?roots=true&limit=20&directory=C%3A%5Cr%20x", chunked(r#"[{"id":"ses_old","directory":"C:/r","title":"a","time":{"created":1}},{"id":"ses_new","directory":"C:/r","title":"b","time":{"created":9}}]"#)),
        ]);
        let live = live(port, "pw").unwrap();
        assert_eq!(live.status.len(), 2);
        assert_eq!(live.status["ses_a"], "busy");
        assert_eq!(live.status["ses_c"], "retry");
        assert_eq!(live.permissions, vec![("ses_a".to_owned(), "per_1".to_owned())]);
        assert_eq!(live.questions, vec![("ses_d".to_owned(), "que_1".to_owned())]);
        assert_eq!(live.waiting().collect::<Vec<_>>(), ["ses_a", "ses_d"]);
        assert_eq!(live.sessions[0].id, "ses_new");
        assert_eq!(live.sessions[1].created_ms, 1);
    }

    #[test]
    fn wrong_password_and_closed_ports_are_errors() {
        let port = serve(vec![("/session/status", length("{}"))]);
        assert!(get_json(port, "wrong", "/session/status").is_err());
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let started = Instant::now();
        assert!(get(closed, "pw", "/").is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"opencode:pw"), "b3BlbmNvZGU6cHc=");
    }

    #[test]
    fn body_without_length_reads_to_close() {
        let port = serve(vec![("/x", b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n[1,2]".to_vec())]);
        assert_eq!(get_json(port, "pw", "/x").unwrap(), serde_json::json!([1, 2]));
    }

    #[test]
    fn chunked_bodies_parse_incrementally() {
        let body = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let mut chunks = Dechunker::default();
        let mut done_at = None;
        for n in 0..=body.len() {
            if chunks.feed(&body[..n], MAX_BODY).unwrap() {
                done_at = Some(n);
                break;
            }
        }
        assert_eq!((done_at, chunks.out.as_slice()), (Some(body.len() - 2), &b"hello world"[..]), "byte by byte");
        assert!(chunks.feed(body, MAX_BODY).unwrap(), "stays done");
    }

    #[test]
    fn chunked_sizes_cannot_overflow_or_exceed_the_limit() {
        for bad in [&b"ffffffffffffffffffff\r\n"[..], b"ffffffffffffffff\r\n", b"fffffffffffffffe\r\n", b"11\r\n", b"zz\r\n", b"3\r\nabcX\r\n"] {
            assert!(Dechunker::default().feed(bad, 16).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let mut chunks = Dechunker::default();
        assert!(chunks.feed(b"8\r\n12345678\r\n9\r\n", 16).is_err(), "the total counts, not each chunk");
        assert!(Dechunker::default().feed(&[b'1'; 2000], 16).is_err(), "an endless size line");
        assert!(!Dechunker::default().feed(b"10\r\n0123456789", 16).unwrap(), "waits for the rest");
        let port = serve(vec![("/big", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffff\r\n".to_vec())]);
        assert!(get(port, "pw", "/big").is_err());
    }
}
