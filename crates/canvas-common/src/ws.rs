//! WebSocket transport (RFC 6455): the iPhone client reaches the service
//! through Dev Tunnels *web forwarding*, which carries HTTP and WebSockets but
//! not raw TCP. A WebSocket connection carries exactly the byte stream a TCP
//! connection does (length-prefixed postcard frames, zstd per direction):
//! binary messages are chunks of that stream, so framing, limits and the
//! handshake are unchanged.
//!
//! The service recognises a WebSocket on its remote port by the first bytes
//! of the connection ([`is_http_prefix`]): `GET ` read as a frame length is
//! about 542 MB, far above the pre-authentication limit, so no valid frame
//! starts that way. Only what the protocol needs is implemented: binary and
//! continuation frames, ping/pong, close; no extensions, no text frames.
//! Readers stream payloads (a claimed length allocates nothing).

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use sha1::{Digest, Sha1};

/// Longest HTTP request or response head accepted (the upgrade).
pub const MAX_HEAD: usize = 8 * 1024;
/// Longest single WebSocket frame accepted (payloads are streamed; this only
/// bounds what a peer may claim).
pub const MAX_WS_FRAME: u64 = 64 << 20;
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Whether the first four bytes of a connection start an HTTP request.
pub fn is_http_prefix(first: &[u8]) -> bool {
    first.starts_with(b"GET ")
}

/// The `Sec-WebSocket-Accept` value for a client's key.
pub fn accept_key(key: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(key.trim().as_bytes());
    hash.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hash.finalize())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reads an HTTP head (through the blank line) one byte at a time, so nothing
/// after it is consumed. `prefix` holds bytes already read (the sniffed `GET `).
fn read_head(prefix: &[u8], reader: &mut dyn Read) -> io::Result<String> {
    let mut head = prefix.to_vec();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(invalid("the HTTP request is too large"));
        }
        if reader.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside the HTTP request"));
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).map_err(|_| invalid("the HTTP request is not text"))
}

/// An HTTP head: the first line and the headers (names lower-case).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Head {
    pub line: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    fn parse(text: &str) -> Self {
        let mut lines = text.split("\r\n");
        let line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .filter(|l| !l.is_empty())
            .filter_map(|l| l.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        Self { line, headers }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.header(name).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }

    /// The request target (`/` …) of a request line.
    pub fn path(&self) -> &str {
        self.line.split(' ').nth(1).unwrap_or_default()
    }
}

/// Server side of the upgrade. `prefix`: bytes of the request already read.
/// Answers `101 Switching Protocols`, or `400` (no text) and an error.
pub fn accept(prefix: &[u8], reader: &mut dyn Read, writer: &mut dyn Write) -> io::Result<Head> {
    let head = Head::parse(&read_head(prefix, reader)?);
    // Relays in front of the port (Dev Tunnels web forwarding) may rewrite
    // `Connection` or drop the version; the upgrade and the key are what count.
    let valid = head.line.starts_with("GET ")
        && head.line.contains(" HTTP/1.")
        && head.has_token("upgrade", "websocket")
        && head.header("sec-websocket-version").is_none_or(|v| v.split(',').any(|t| t.trim() == "13"));
    let key = head.header("sec-websocket-key").filter(|k| base64::engine::general_purpose::STANDARD.decode(k.trim()).is_ok_and(|b| b.len() == 16));
    let Some(key) = key.filter(|_| valid) else {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return Err(invalid("not a WebSocket upgrade"));
    };
    let response = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n", accept_key(key));
    writer.write_all(response.as_bytes())?;
    writer.flush()?;
    Ok(head)
}

/// Client side of the upgrade (tests, the mock service's own checks).
/// `host` is the `Host` header; `headers` are extra request headers.
pub fn connect(reader: &mut dyn Read, writer: &mut dyn Write, host: &str, path: &str, headers: &[(&str, &str)]) -> io::Result<Head> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
    let key = base64::engine::general_purpose::STANDARD.encode(nonce);
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    writer.write_all(request.as_bytes())?;
    writer.flush()?;
    let head = Head::parse(&read_head(&[], reader)?);
    if !head.line.starts_with("HTTP/1.1 101") {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("the server did not upgrade: {}", head.line)));
    }
    if head.header("sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(invalid("the server's WebSocket accept key is wrong"));
    }
    Ok(head)
}

/// Which end of the connection: clients mask what they send, servers do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

/// Encodes one frame (FIN set).
fn frame(role: Role, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let masked = if role == Role::Client { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => out.push(masked | n as u8),
        n if n <= u16::MAX as usize => {
            out.push(masked | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(masked | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    if role == Role::Client {
        let mut mask = [0u8; 4];
        let _ = getrandom::fill(&mut mask);
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    } else {
        out.extend_from_slice(payload);
    }
    out
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Splits an upgraded connection into a reader and a writer of the byte
/// stream inside its binary messages.
pub fn split(role: Role, reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> (WsReader, WsWriter) {
    let shared: SharedWriter = Arc::new(Mutex::new(writer));
    (WsReader { inner: reader, role, remaining: 0, mask: None, offset: 0, closed: false, writer: shared.clone() }, WsWriter { role, inner: shared })
}

/// The byte stream inside the peer's binary messages; answers pings and
/// close frames through the shared writer. A close frame ends the stream.
pub struct WsReader {
    inner: Box<dyn Read + Send>,
    role: Role,
    /// Payload bytes left in the current data frame.
    remaining: u64,
    mask: Option<[u8; 4]>,
    /// Position in the current frame's payload (for unmasking).
    offset: u64,
    closed: bool,
    writer: SharedWriter,
}

impl WsReader {
    fn send(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(&frame(self.role, opcode, payload))?;
        writer.flush()
    }

    /// Reads a frame header; returns (opcode, length).
    fn header(&mut self) -> io::Result<Option<(u8, u64)>> {
        let mut two = [0u8; 2];
        match self.inner.read_exact(&mut two) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if two[0] & 0x70 != 0 {
            return Err(invalid("WebSocket extensions are not supported"));
        }
        let opcode = two[0] & 0x0f;
        let masked = two[1] & 0x80 != 0;
        // Clients must mask; servers must not (RFC 6455 §5.1).
        if masked != (self.role == Role::Server) {
            return Err(invalid("a WebSocket frame with the wrong masking"));
        }
        let len = match two[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                self.inner.read_exact(&mut b)?;
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0u8; 8];
                self.inner.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };
        if len > MAX_WS_FRAME {
            return Err(invalid("a WebSocket frame is too large"));
        }
        self.mask = if masked {
            let mut key = [0u8; 4];
            self.inner.read_exact(&mut key)?;
            Some(key)
        } else {
            None
        };
        self.offset = 0;
        Ok(Some((opcode, len)))
    }

    fn unmask(&mut self, bytes: &mut [u8]) {
        if let Some(key) = self.mask {
            for (i, b) in bytes.iter_mut().enumerate() {
                *b ^= key[((self.offset + i as u64) % 4) as usize];
            }
        }
        self.offset += bytes.len() as u64;
    }
}

impl Read for WsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.closed || buf.is_empty() {
                return Ok(0);
            }
            if self.remaining > 0 {
                let want = (buf.len() as u64).min(self.remaining) as usize;
                let n = self.inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside a WebSocket frame"));
                }
                self.unmask(&mut buf[..n]);
                self.remaining -= n as u64;
                return Ok(n);
            }
            let Some((opcode, len)) = self.header()? else {
                self.closed = true;
                return Ok(0);
            };
            match opcode {
                OP_BINARY | OP_CONTINUATION => self.remaining = len,
                OP_TEXT => return Err(invalid("WebSocket text messages are not used")),
                OP_CLOSE | OP_PING | OP_PONG => {
                    if len > 125 {
                        return Err(invalid("a WebSocket control frame is too large"));
                    }
                    let mut payload = vec![0u8; len as usize];
                    self.inner.read_exact(&mut payload)?;
                    self.unmask(&mut payload);
                    match opcode {
                        OP_PING => self.send(OP_PONG, &payload)?,
                        OP_CLOSE => {
                            // Echo the status code; the stream ends.
                            let _ = self.send(OP_CLOSE, payload.get(..2).unwrap_or_default());
                            self.closed = true;
                            return Ok(0);
                        }
                        _ => {}
                    }
                }
                _ => return Err(invalid("an unknown WebSocket opcode")),
            }
        }
    }
}

/// Writes the byte stream as binary messages: every `write` is one message,
/// so a frame written with one `write_all` travels as one message.
pub struct WsWriter {
    role: Role,
    inner: SharedWriter,
}

impl WsWriter {
    /// Sends a close frame (best effort).
    pub fn close(&self) {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(&frame(self.role, OP_CLOSE, &1000u16.to_be_bytes()));
        let _ = writer.flush();
    }
}

impl Write for WsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(&frame(self.role, OP_BINARY, buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMsg, ServiceMsg};
    use crate::wire::{FrameReader, FrameWriter};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn the_accept_key_follows_the_rfc_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert!(is_http_prefix(b"GET / HTTP/1.1"));
        assert!(!is_http_prefix(&20u32.to_le_bytes()));
        // "GET " read as a frame length is far beyond any pre-auth frame.
        assert!(u32::from_le_bytes(*b"GET ") as usize > crate::protocol::PRE_AUTH_FRAME);
    }

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || TcpStream::connect(addr).unwrap());
        let (server, _) = listener.accept().unwrap();
        (server, client.join().unwrap())
    }

    #[test]
    fn frames_travel_through_an_upgraded_connection_both_ways() {
        let (server, client) = pair();
        let service = std::thread::spawn(move || {
            let mut reader: Box<dyn Read + Send> = Box::new(server.try_clone().unwrap());
            let mut writer: Box<dyn Write + Send> = Box::new(server);
            let mut first = [0u8; 4];
            reader.read_exact(&mut first).unwrap();
            assert!(is_http_prefix(&first));
            let head = accept(&first, &mut reader, &mut writer).unwrap();
            assert_eq!(head.path(), "/canvas");
            assert_eq!(head.header("x-tunnel-authorization"), Some("tunnel abc"));
            let (r, w) = split(Role::Server, reader, writer);
            let mut frames = FrameReader::new(Box::new(r));
            let mut out = FrameWriter::new(Box::new(w));
            out.compress().unwrap();
            frames.accept_compressed().unwrap();
            let got: ClientMsg = frames.recv().unwrap().unwrap();
            assert_eq!(got, ClientMsg::Claim { id: "x".repeat(70_000) });
            out.send(&ServiceMsg::Welcome { build: "b".repeat(100_000) }).unwrap();
            out.send(&ServiceMsg::Heartbeat { input_ack: 7 }).unwrap();
        });
        let mut reader: Box<dyn Read + Send> = Box::new(client.try_clone().unwrap());
        let mut writer: Box<dyn Write + Send> = Box::new(client);
        connect(&mut reader, &mut writer, "localhost", "/canvas", &[("X-Tunnel-Authorization", "tunnel abc")]).unwrap();
        let (r, w) = split(Role::Client, reader, writer);
        let mut frames = FrameReader::new(Box::new(r));
        let mut out = FrameWriter::new(Box::new(w));
        out.compress().unwrap();
        frames.accept_compressed().unwrap();
        out.send(&ClientMsg::Claim { id: "x".repeat(70_000) }).unwrap();
        assert_eq!(frames.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Welcome { build: "b".repeat(100_000) }));
        assert_eq!(frames.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Heartbeat { input_ack: 7 }));
        service.join().unwrap();
        assert_eq!(frames.recv::<ServiceMsg>().unwrap(), None, "the service closed");
    }

    #[test]
    fn pings_are_answered_and_close_ends_the_stream() {
        let (server, client) = pair();
        let (mut r, _w) = split(Role::Server, Box::new(server.try_clone().unwrap()), Box::new(server));
        let mut raw = client.try_clone().unwrap();
        raw.write_all(&frame(Role::Client, OP_PING, b"hi")).unwrap();
        raw.write_all(&frame(Role::Client, OP_BINARY, b"abc")).unwrap();
        raw.write_all(&frame(Role::Client, OP_CLOSE, &1000u16.to_be_bytes())).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"abc");
        // The pong (unmasked, server role) and the echoed close came back.
        let mut back = [0u8; 8];
        let mut client = client;
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back[..4], &[0x80 | OP_PONG, 2, b'h', b'i']);
        assert_eq!(&back[4..], &[0x80 | OP_CLOSE, 2, 0x03, 0xe8]);
    }

    #[test]
    fn bad_requests_and_unmasked_client_frames_are_refused() {
        let mut out = Vec::new();
        let request = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(accept(b"", &mut request.as_slice(), &mut out).is_err());
        // What a relay may forward: no Connection header, the upgrade and the key.
        let relayed = b"GET / HTTP/1.1\r\nHost: t-1.usw2.devtunnels.ms\r\nUpgrade: WebSocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let mut answer = Vec::new();
        assert!(accept(b"", &mut relayed.as_slice(), &mut answer).is_ok());
        assert!(String::from_utf8_lossy(&answer).contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        let old_version = b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 8\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        assert!(accept(b"", &mut old_version.as_slice(), &mut Vec::new()).is_err());
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 400"));
        let huge = vec![b'a'; MAX_HEAD + 10];
        assert!(accept(b"GET ", &mut huge.as_slice(), &mut Vec::new()).is_err(), "an endless head");
        let (server, client) = pair();
        let (mut r, _w) = split(Role::Server, Box::new(server.try_clone().unwrap()), Box::new(server));
        let mut raw = client;
        raw.write_all(&frame(Role::Server, OP_BINARY, b"abc")).unwrap();
        assert_eq!(r.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_claimed_huge_frame_is_refused_without_allocating() {
        let mut bytes = vec![0x80 | OP_BINARY, 0x80 | 127];
        bytes.extend_from_slice(&(MAX_WS_FRAME + 1).to_be_bytes());
        bytes.extend_from_slice(&[0; 4]);
        let (mut r, _w) = split(Role::Server, Box::new(io::Cursor::new(bytes)), Box::new(Vec::new()));
        assert_eq!(r.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
