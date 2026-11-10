//! HTTP for `vm::aca`: calls to the data plane with the cached bearer token,
//! retries on throttling and server errors, and errors that carry the status
//! and the service's own code and message, never the token, a request's body
//! or the pack's values (the subscription, groups and endpoint are replaced
//! by their names).

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::blocking::{Body, Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
pub use reqwest::Method;
use serde_json::Value;

use super::model::error_parts;
use super::token::{Cache, Token, scrub};
use crate::vm::provider::Secret;

/// Query values keep only unreserved characters as they are.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
/// Largest JSON answer read.
pub const MAX_JSON: u64 = 32 << 20;
/// Largest error answer read (only its first line is kept).
const MAX_ERROR: u64 = 64 << 10;
/// The time limit of a call that only reads or changes a state.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// How throttled and failed calls are tried again.
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    /// Tries in all, the first one included.
    pub tries: u32,
    /// The longest time one call may take, waits included.
    pub total: Duration,
    /// The first wait after a server error (doubled each time, with jitter).
    pub base: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry { tries: 4, total: Duration::from_secs(60), base: Duration::from_millis(500) }
    }
}

/// A request's body, built again for each try.
#[derive(Clone, Copy)]
pub enum Payload<'a> {
    None,
    /// No bytes, with `Content-Length: 0` (a POST that only names an action).
    Empty,
    Json(&'a Value),
    Bytes(&'a [u8]),
    /// JSON holding a secret: sent from a buffer zeroized after each try.
    Secret(&'a Secret),
}

/// One call to the data plane.
pub struct Call<'a> {
    op: &'a str,
    method: Method,
    url: String,
    payload: Payload<'a>,
    /// A server error may be retried: the call reads or sets a state. Calls
    /// that create or run something are retried only after a 429 (the
    /// service did not take them) or when the connection could not be made.
    repeat: bool,
    timeout: Duration,
    max: u64,
    /// Statuses besides 2xx the caller handles itself (404 on delete, 409 on resume).
    accept: &'a [u16],
    /// Texts never shown in an error (a secret's value).
    hide: Vec<&'a str>,
}

impl<'a> Call<'a> {
    pub fn new(op: &'a str, method: Method, url: String) -> Self {
        Call { op, method, url, payload: Payload::None, repeat: true, timeout: CONTROL_TIMEOUT, max: MAX_JSON, accept: &[], hide: Vec::new() }
    }

    pub fn payload(mut self, payload: Payload<'a>) -> Self {
        self.payload = payload;
        self
    }

    pub fn json(self, value: &'a Value) -> Self {
        self.payload(Payload::Json(value))
    }

    /// Creates or runs something: never sent twice after a server error.
    pub fn once(mut self) -> Self {
        self.repeat = false;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn max(mut self, max: u64) -> Self {
        self.max = max;
        self
    }

    pub fn accept(mut self, statuses: &'a [u16]) -> Self {
        self.accept = statuses;
        self
    }

    pub fn hide(mut self, text: &'a str) -> Self {
        self.hide.push(text);
        self
    }
}

/// A final answer: 2xx, or a status the call accepts.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as JSON (an error never shows the body).
    pub fn json(&self, op: &str) -> Result<Value> {
        serde_json::from_slice(&self.body).map_err(|_| anyhow!("{op}: the answer is not JSON"))
    }

    pub fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The last failed try, kept for the error when no try is left.
enum Last {
    Reply(Reply, Option<u64>),
    Transport(String),
}

/// A secret request body, read by the HTTP client and zeroized when dropped.
struct SecretReader {
    secret: Secret,
    at: usize,
}

impl Read for SecretReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let rest = &self.secret.expose()[self.at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.at += n;
        Ok(n)
    }
}

/// The host (with its port) of a URL.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// Whether a URL names this machine (`http` is allowed only there).
pub fn loopback(url: &str) -> bool {
    let host = host_of(url);
    let host = if host.starts_with('[') { host.split_inclusive(']').next().unwrap_or_default() } else { host.split(':').next().unwrap_or_default() };
    matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// The data plane's base URL: the pack's `aca.endpoint`, else
/// `https://management.<region>.azuredevcompute.io`. Plain `http` only on
/// the loopback (a bearer token never travels in the clear).
pub fn endpoint(aca: &crate::vm::pack::Aca) -> Result<String> {
    let base = match aca.endpoint.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(endpoint) => endpoint.trim_end_matches('/').to_owned(),
        None => {
            ensure!(!aca.region.is_empty() && aca.region.chars().all(|c| c.is_ascii_alphanumeric()), "aca.region is a region name such as eastus2");
            format!("https://management.{}.azuredevcompute.io", aca.region.to_ascii_lowercase())
        }
    };
    let (scheme, rest) = base.split_once("://").context("aca.endpoint is a URL (https://…)")?;
    ensure!(!host_of(&base).is_empty() && !rest.contains(['?', '#', '@', '\\']) && !rest.chars().any(|c| c.is_whitespace() || c.is_control()), "aca.endpoint is a URL without a user, a query or a fragment");
    ensure!(scheme == "https" || (scheme == "http" && loopback(&base)), "aca.endpoint must use https (http only on this machine's loopback)");
    Ok(base)
}

/// `text` with every ASCII-case-insensitive `needle` replaced by `with`.
fn replace_ignore_case(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    let (lower, needle) = (text.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let (mut out, mut last) = (String::with_capacity(text.len()), 0);
    for (at, _) in lower.match_indices(&needle) {
        out.push_str(&text[last..at]);
        out.push_str(with);
        last = at + needle.len();
    }
    out.push_str(&text[last..]);
    out
}

fn jitter(max: Duration) -> Duration {
    let ms = max.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(uuid::Uuid::new_v4().as_u128() as u64 % (ms + 1))
}

/// rustls needs its crypto provider chosen once per process (Linux builds;
/// elsewhere the system TLS needs nothing).
#[cfg(target_os = "linux")]
fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(not(target_os = "linux"))]
fn install_tls_provider() {}

/// The data plane's HTTP client.
pub struct Rest {
    http: Client,
    base: String,
    api_version: String,
    pub token: Cache,
    /// Pack values never shown in errors, and what is shown instead (longest first).
    hidden: Vec<(String, &'static str)>,
    pub retry: Retry,
}

impl Rest {
    pub fn new(base: String, api_version: &str, token: Cache, mut hidden: Vec<(String, &'static str)>) -> Result<Rest> {
        ensure!(!api_version.is_empty() && api_version.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'), "aca.api_version is a version such as 2026-02-01-preview");
        install_tls_provider();
        let mut builder = Client::builder().connect_timeout(Duration::from_secs(15)).user_agent(concat!("atelier/", env!("CARGO_PKG_VERSION")));
        if loopback(&base) {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(|_| anyhow!("cannot start the HTTP client"))?;
        hidden.retain(|(value, _)| value.len() >= 3);
        hidden.push((host_of(&base).to_owned(), "<endpoint>"));
        hidden.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        Ok(Rest { http, base, api_version: api_version.to_owned(), token, hidden, retry: Retry::default() })
    }

    /// `base + path`, the API version and `query` (values encoded).
    pub fn url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let mut url = format!("{}{path}?api-version={}", self.base, utf8_percent_encode(&self.api_version, QUERY));
        for (key, value) in query {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&utf8_percent_encode(value, QUERY).to_string());
        }
        url
    }

    /// A list's next page, only on this endpoint (the token never goes anywhere else).
    pub fn next_url(&self, link: &str) -> Option<String> {
        let rest = link.strip_prefix(&self.base)?;
        if !rest.starts_with('/') || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        Some(if link.contains("api-version=") { link.to_owned() } else { format!("{link}{}api-version={}", if link.contains('?') { '&' } else { '?' }, utf8_percent_encode(&self.api_version, QUERY)) })
    }

    /// The first line of `text`, at most 200 characters, without the token,
    /// anything like one, `hide`, or the pack's values.
    fn clean(&self, text: &str, hide: &[&str]) -> String {
        let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let token = self.token.cached();
        let mut secrets = hide.to_vec();
        if let Some(token) = &token {
            secrets.push(token.reveal());
        }
        let mut out = scrub(line, &secrets);
        for (value, name) in &self.hidden {
            out = replace_ignore_case(&out, value, name);
        }
        crate::vm::hooks::first_line(&out)
    }

    /// The error for an answer the caller cannot use: the operation, the
    /// status and the service's `error.code` and `error.message`.
    pub fn failure(&self, op: &str, reply: &Reply, hide: &[&str]) -> anyhow::Error {
        let (code, message) = error_parts(&reply.body);
        let (code, message) = (self.clean(&code, hide), self.clean(&message, hide));
        let detail = match (code.is_empty(), message.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!(" {code}"),
            (true, false) => format!(": {message}"),
            (false, false) => format!(" {code}: {message}"),
        };
        let hint = match reply.status {
            401 => " (is auth.audience right, and are you signed in?)",
            403 => " (does your account have a data role on the sandbox group?)",
            _ => "",
        };
        anyhow!("{op}: HTTP {}{detail}{hint}", reply.status)
    }

    /// A transport failure in words (never the URL).
    fn transport(&self, error: &reqwest::Error) -> String {
        let mut text = String::from(if error.is_timeout() {
            "the request timed out"
        } else if error.is_connect() {
            "cannot connect to the sandbox service"
        } else {
            "network error"
        });
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            text.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        self.clean(&text, &[])
    }

    fn request(&self, call: &Call, token: &Token) -> Result<RequestBuilder> {
        let mut bearer = Vec::with_capacity(7 + token.reveal().len());
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(token.reveal().as_bytes());
        let bearer = Secret::new(bearer);
        let mut auth = HeaderValue::from_bytes(bearer.expose()).map_err(|_| anyhow!("{}: the access token cannot be sent in a header", call.op))?;
        auth.set_sensitive(true);
        let request = self.http.request(call.method.clone(), &call.url).header(AUTHORIZATION, auth).timeout(call.timeout);
        Ok(match call.payload {
            Payload::None => request,
            Payload::Empty => request.body(Vec::new()),
            Payload::Json(value) => request.header(CONTENT_TYPE, "application/json").body(serde_json::to_vec(value)?),
            Payload::Bytes(bytes) => request.header(CONTENT_TYPE, "application/octet-stream").body(bytes.to_vec()),
            Payload::Secret(secret) => {
                let copy = Secret::new(secret.expose().to_vec());
                let len = copy.expose().len() as u64;
                request.header(CONTENT_TYPE, "application/json").body(Body::sized(SecretReader { secret: copy, at: 0 }, len))
            }
        })
    }

    /// Sends `call`, trying again after a 429 (waiting `Retry-After`
    /// seconds) and, when the call may repeat, after a 5xx or a network
    /// failure (waiting with jitter): at most `retry.tries` tries in
    /// `retry.total`.
    pub fn send(&self, call: Call) -> Result<Reply> {
        let op = call.op;
        let started = Instant::now();
        let mut tries = 0;
        loop {
            tries += 1;
            let token = self.token.get().map_err(|e| anyhow!("{op}: no access token: {}", self.clean(&format!("{e:#}"), &call.hide)))?;
            let last = match self.request(&call, &token)?.send() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = response.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
                    let success = (200..300).contains(&status);
                    let max = if success { call.max } else { MAX_ERROR };
                    let mut body = Vec::new();
                    let read = response.take(max + 1).read_to_end(&mut body);
                    if success {
                        ensure!(read.is_ok(), "{op}: the answer could not be read");
                        ensure!(body.len() as u64 <= max, "{op}: the answer is larger than {max} bytes");
                    }
                    let reply = Reply { status, body };
                    if success || call.accept.contains(&status) {
                        return Ok(reply);
                    }
                    if !(status == 429 || (status >= 500 && call.repeat)) {
                        return Err(self.failure(op, &reply, &call.hide));
                    }
                    Last::Reply(reply, retry_after)
                }
                Err(error) => {
                    let text = self.transport(&error);
                    if !(call.repeat || error.is_connect()) {
                        bail!("{op}: {text}");
                    }
                    Last::Transport(text)
                }
            };
            let wait = match &last {
                Last::Reply(_, Some(seconds)) => Duration::from_secs(*seconds) + jitter(Duration::from_millis(250)),
                _ => {
                    let full = self.retry.base.saturating_mul(1 << (tries - 1).min(6));
                    full / 2 + jitter(full / 2)
                }
            };
            if tries >= self.retry.tries || started.elapsed() + wait > self.retry.total {
                return Err(match last {
                    Last::Reply(reply, _) => self.failure(op, &reply, &call.hide),
                    Last::Transport(text) => anyhow!("{op}: {text}"),
                });
            }
            std::thread::sleep(wait);
        }
    }
}

impl std::fmt::Debug for Rest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest").field("api_version", &self.api_version).field("token", &self.token).finish_non_exhaustive()
    }
}

/// An HTTP/1.1 stand-in for the data plane on `127.0.0.1:0`: one thread,
/// one connection per request (`Connection: close`), answers from a
/// closure, every request recorded. Tests only: nothing reaches a network.
#[cfg(test)]
pub mod standin {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use percent_encoding::percent_decode_str;
    use serde_json::Value;

    #[derive(Clone, Debug)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub query: Vec<(String, String)>,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
        pub at: Instant,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }

        pub fn param(&self, name: &str) -> Option<&str> {
            self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }

        pub fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }

        /// `METHOD path` with the test group's prefix written `G`.
        pub fn line(&self) -> String {
            format!("{} {}", self.method, self.path.replace(GROUP, "G"))
        }
    }

    /// The group path of the tests' pack (placeholders only).
    pub const GROUP: &str = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/contoso-rg/sandboxGroups/contoso-sandboxes";

    #[derive(Clone)]
    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Reply {
        pub fn json(status: u16, value: Value) -> Reply {
            Reply { status, headers: vec![("Content-Type".into(), "application/json".into())], body: value.to_string().into_bytes() }
        }

        pub fn text(status: u16, text: &str) -> Reply {
            Reply { status, headers: Vec::new(), body: text.as_bytes().to_vec() }
        }

        pub fn status(status: u16) -> Reply {
            Reply::text(status, "")
        }

        pub fn header(mut self, name: &str, value: &str) -> Reply {
            self.headers.push((name.into(), value.into()));
            self
        }
    }

    type Handler = Box<dyn FnMut(&Req) -> Reply + Send>;

    pub struct StandIn {
        pub url: String,
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<Req>>>,
        stop: Arc<AtomicBool>,
    }

    impl StandIn {
        pub fn start(handler: impl FnMut(&Req) -> Reply + Send + 'static) -> StandIn {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (log, stopped) = (requests.clone(), stop.clone());
            let mut handler: Handler = Box::new(handler);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(stream) = stream {
                        let _ = serve(stream, &mut handler, &log);
                    }
                }
            });
            StandIn { url: format!("http://{addr}"), addr, requests, stop }
        }

        pub fn requests(&self) -> Vec<Req> {
            self.requests.lock().unwrap().clone()
        }

        pub fn lines(&self) -> Vec<String> {
            self.requests().iter().map(Req::line).collect()
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        }
    }

    fn serve(stream: TcpStream, handler: &mut Handler, log: &Mutex<Vec<Req>>) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((k, v)) = header.split_once(':') {
                headers.push((k.trim().to_owned(), v.trim().to_owned()));
            }
        }
        let find = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v): &(String, String)| v.clone());
        let mut body = Vec::new();
        if let Some(length) = find("content-length").and_then(|l| l.parse::<usize>().ok()) {
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else if find("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size)?;
                let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                let mut chunk = vec![0; size + 2];
                reader.read_exact(&mut chunk)?;
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        }
        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        let query = query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode(k), decode(v))).collect();
        let req = Req { method, path: decode(path), query, headers, body, at: Instant::now() };
        log.lock().unwrap().push(req.clone());
        let reply = handler(&req);
        let mut out = format!("HTTP/1.1 {} Stand-in\r\nContent-Length: {}\r\nConnection: close\r\n", reply.status, reply.body.len());
        for (k, v) in &reply.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut stream = stream;
        stream.write_all(out.as_bytes())?;
        stream.write_all(&reply.body)?;
        stream.flush()?;
        let _ = stream.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_except_on_the_loopback() {
        let aca = |endpoint: Option<&str>, region: &str| crate::vm::pack::Aca { region: region.into(), endpoint: endpoint.map(str::to_owned), ..Default::default() };
        assert_eq!(endpoint(&aca(None, "EastUS2")).unwrap(), "https://management.eastus2.azuredevcompute.io");
        assert_eq!(endpoint(&aca(Some("https://sandbox.example.com/"), "")).unwrap(), "https://sandbox.example.com");
        assert_eq!(endpoint(&aca(Some("http://127.0.0.1:8080"), "")).unwrap(), "http://127.0.0.1:8080");
        assert!(endpoint(&aca(Some("http://[::1]:80"), "")).is_ok());
        for bad in ["http://sandbox.example.com", "https://user@example.com", "https://example.com/?x", "ftp://example.com", "example.com", "https://"] {
            assert!(endpoint(&aca(Some(bad), "")).is_err(), "{bad}");
        }
        assert!(endpoint(&aca(None, "east us")).is_err());
        assert!(endpoint(&aca(None, "")).is_err());
        assert!(!loopback("http://127.0.0.1.example.com") && loopback("http://localhost:1/x"));
    }

    #[test]
    fn pack_values_are_replaced_whatever_their_case() {
        assert_eq!(replace_ignore_case("Group Contoso-RG not found in contoso-rg", "contoso-rg", "<resource-group>"), "Group <resource-group> not found in <resource-group>");
        assert_eq!(replace_ignore_case("x", "", "y"), "x");
    }
}
