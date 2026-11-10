//! How the satellite reaches its hub (plan VM-23 `dial`): `wss://` (TLS
//! with the platform's roots and the pack's CA, `vm::tls`; a Dev Tunnel's
//! web forwarding), `ws://`, or `tcp://` (the framed protocol as is; tests
//! and `vm::local`). Through `HTTPS_PROXY` / `HTTP_PROXY` (`CONNECT`) unless
//! `NO_PROXY` names the host. Every step is bounded in time.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use canvas_common::wire::Closer;
use canvas_common::ws;
use rustls::pki_types::{CertificateDer, ServerName};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The TLS handshake, a proxy's answer and the WebSocket upgrade.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// A read that gets nothing for this long ends the link (the hub sends a
/// heartbeat every 2 s). Closing a socket does not wake a blocked read on
/// every OS (Windows, when another process still holds the hub's end).
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to the hub as two halves and a way to end both.
pub struct Dialed {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub closer: Closer,
}

/// A hub address.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Reads `ws://host[:port][/path]`, `wss://…` or `tcp://host:port`.
pub fn parse(url: &str) -> Result<Target> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("the hub's address {url:?} has no scheme (wss://, ws:// or tcp://)"))?;
    let scheme = scheme.to_ascii_lowercase();
    let default = match scheme.as_str() {
        "wss" | "https" => 443,
        "ws" | "http" => 80,
        "tcp" => 0,
        other => bail!("the hub's address has scheme {other:?}: wss://, ws:// or tcp://"),
    };
    let scheme = match scheme.as_str() {
        "https" => "wss".to_owned(),
        "http" => "ws".to_owned(),
        _ => scheme,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    anyhow::ensure!(!authority.contains('@'), "the hub's address may not carry credentials");
    let number = |p: &str| p.parse::<u16>().map_err(|_| anyhow!("the hub's port {p:?} is not a number"));
    let (host, port) = match authority.strip_prefix('[') {
        // An IPv6 address: [::1]:port
        Some(rest) => {
            let (host, after) = rest.split_once(']').ok_or_else(|| anyhow!("the hub's address {url:?} has an unclosed ["))?;
            match after.strip_prefix(':') {
                Some(port) => (host, number(port)?),
                None if after.is_empty() => (host, default),
                None => anyhow::bail!("the hub's address {url:?} is not host:port"),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, number(port)?),
            None => (authority, default),
        },
    };
    anyhow::ensure!(!host.is_empty() && port != 0, "the hub's address {url:?} names no host and port");
    anyhow::ensure!(!host.chars().any(|c| c.is_control() || c.is_whitespace()) && !path.chars().any(|c| c.is_control() || c.is_whitespace()), "the hub's address has spaces or control characters");
    Ok(Target { scheme, host: host.to_owned(), port, path: path.to_owned() })
}

/// The proxy for `target` from the environment, unless `NO_PROXY` names its host.
pub fn proxy_for(target: &Target, var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if target.scheme == "tcp" {
        return None;
    }
    let get = |name: &str| var(name).or_else(|| var(&name.to_ascii_lowercase())).filter(|v| !v.trim().is_empty());
    let proxy = if target.scheme == "wss" { get("HTTPS_PROXY").or_else(|| get("ALL_PROXY")) } else { get("HTTP_PROXY").or_else(|| get("ALL_PROXY")) }?;
    let no_proxy = get("NO_PROXY").unwrap_or_default();
    let host = target.host.to_ascii_lowercase();
    let skipped = no_proxy.split(',').map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).any(|entry| {
        if entry == "*" {
            return true;
        }
        // `*.example.com`, `.example.com` and `example.com` cover the domain and its hosts; a port is ignored.
        let domain = entry.trim_start_matches('*').trim_start_matches('.').split(':').next().unwrap_or_default().to_owned();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    });
    (!skipped).then_some(proxy)
}

fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 << 10 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the proxy's answer is too long"));
        }
        match stream.read(&mut byte)? {
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the proxy closed the connection")),
            _ => head.push(byte[0]),
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// A TCP connection to `target`, through the proxy's `CONNECT` when one is given.
fn connect(target: &Target, proxy: Option<&str>) -> Result<TcpStream> {
    let Some(proxy) = proxy else {
        return canvas_common::tcp::connect(&format!("{}:{}", bracket(&target.host), target.port), CONNECT_TIMEOUT).with_context(|| format!("cannot reach the hub at {}:{}", target.host, target.port));
    };
    let via = parse(&format!("http://{}", proxy.split_once("://").map_or(proxy, |(_, rest)| rest))).context("the proxy's address")?;
    let mut stream = canvas_common::tcp::connect(&format!("{}:{}", bracket(&via.host), via.port), CONNECT_TIMEOUT).with_context(|| format!("cannot reach the proxy {}:{}", via.host, via.port))?;
    stream.set_read_timeout(Some(SETUP_TIMEOUT))?;
    let authority = format!("{}:{}", bracket(&target.host), target.port);
    stream.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())?;
    let head = read_head(&mut stream).context("the proxy did not answer")?;
    let status = head.lines().next().unwrap_or_default();
    anyhow::ensure!(status.split_whitespace().nth(1) == Some("200"), "the proxy refused the connection to the hub: {status}");
    stream.set_read_timeout(None)?;
    Ok(stream)
}

fn bracket(host: &str) -> String {
    if host.contains(':') { format!("[{host}]") } else { host.to_owned() }
}

/// Connects to the hub at `url` (`roots`: the pack's CA, added to the platform's).
pub fn dial(url: &str, roots: Vec<CertificateDer<'static>>, headers: &[(String, String)]) -> Result<Dialed> {
    let target = parse(url)?;
    let proxy = proxy_for(&target, &|name| std::env::var(name).ok());
    let stream = connect(&target, proxy.as_deref())?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let closing = stream.try_clone()?;
    let closer = Closer::new(move || {
        let _ = closing.shutdown(Shutdown::Both);
    });
    let (mut reader, mut writer): (Box<dyn Read + Send>, Box<dyn Write + Send>) = if target.scheme == "wss" {
        let (reader, writer) = tls(stream, &target.host, roots)?;
        (Box::new(reader), Box::new(writer))
    } else {
        (Box::new(stream.try_clone()?), Box::new(stream))
    };
    if target.scheme == "tcp" {
        return Ok(Dialed { reader, writer, closer });
    }
    let host = if (target.scheme == "wss" && target.port == 443) || (target.scheme == "ws" && target.port == 80) { bracket(&target.host) } else { format!("{}:{}", bracket(&target.host), target.port) };
    let extra: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    ws::connect(&mut reader, &mut writer, &host, &target.path, &extra).context("the hub did not accept the WebSocket")?;
    let (reader, writer) = ws::split(ws::Role::Client, reader, writer);
    Ok(Dialed { reader: Box::new(reader), writer: Box::new(writer), closer })
}

/// A TLS client connection shared by the two halves (each half locks it
/// only to move records; the socket is read without the lock).
struct TlsShared {
    conn: Mutex<rustls::ClientConnection>,
    socket: TcpStream,
}

pub struct TlsReader {
    shared: Arc<TlsShared>,
    socket: TcpStream,
    buffer: Vec<u8>,
}

pub struct TlsWriter {
    shared: Arc<TlsShared>,
}

fn lock(shared: &TlsShared) -> std::sync::MutexGuard<'_, rustls::ClientConnection> {
    shared.conn.lock().unwrap_or_else(|e| e.into_inner())
}

/// The TLS handshake with `host` on `stream`, then the two halves.
fn tls(mut stream: TcpStream, host: &str, roots: Vec<CertificateDer<'static>>) -> Result<(TlsReader, TlsWriter)> {
    let config = crate::vm::tls::client_config(roots)?;
    let name = ServerName::try_from(host.to_owned()).map_err(|_| anyhow!("{host:?} is not a server name"))?;
    let mut conn = rustls::ClientConnection::new(config, name).context("TLS")?;
    stream.set_read_timeout(Some(SETUP_TIMEOUT))?;
    stream.set_write_timeout(Some(SETUP_TIMEOUT))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut stream).context("the TLS handshake with the hub failed")?;
    }
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(None)?;
    let shared = Arc::new(TlsShared { conn: Mutex::new(conn), socket: stream.try_clone()? });
    Ok((TlsReader { shared: shared.clone(), socket: stream, buffer: vec![0; 32 << 10] }, TlsWriter { shared }))
}

impl Read for TlsReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut conn = lock(&self.shared);
                match conn.reader().read(out) {
                    Ok(n) => return Ok(n),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            let n = self.socket.read(&mut self.buffer)?;
            if n == 0 {
                return Ok(0);
            }
            let mut conn = lock(&self.shared);
            let mut records = &self.buffer[..n];
            while !records.is_empty() {
                conn.read_tls(&mut records)?;
                conn.process_new_packets().map_err(io::Error::other)?;
            }
            // Alerts and key updates are answered here.
            while conn.wants_write() {
                conn.write_tls(&mut &self.shared.socket)?;
            }
        }
    }
}

impl Write for TlsWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut conn = lock(&self.shared);
        let n = conn.writer().write(data)?;
        while conn.wants_write() {
            conn.write_tls(&mut &self.shared.socket)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut conn = lock(&self.shared);
        conn.writer().flush()?;
        while conn.wants_write() {
            conn.write_tls(&mut &self.shared.socket)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn hub_addresses_are_read_strictly() {
        let t = parse("wss://atelier-48001.usw2.devtunnels.ms").unwrap();
        assert_eq!(t, Target { scheme: "wss".into(), host: "atelier-48001.usw2.devtunnels.ms".into(), port: 443, path: "/".into() });
        assert_eq!(parse("ws://127.0.0.1:48010/sat").unwrap(), Target { scheme: "ws".into(), host: "127.0.0.1".into(), port: 48010, path: "/sat".into() });
        assert_eq!(parse("tcp://[::1]:48011").unwrap().host, "::1");
        assert_eq!(parse("https://example.com").unwrap().scheme, "wss");
        for bad in ["example.com", "ftp://example.com", "tcp://example.com", "ws://user:pw@example.com", "ws://:80", "ws://example.com:x", "ws://exa mple.com"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn proxies_come_from_the_environment_unless_no_proxy_names_the_host() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        let hub = parse("wss://hub.example.com").unwrap();
        assert_eq!(proxy_for(&hub, &env(&[("HTTPS_PROXY", "http://proxy:3128")])), Some("http://proxy:3128".into()));
        assert_eq!(proxy_for(&hub, &env(&[("https_proxy", "http://proxy:3128")])), Some("http://proxy:3128".into()));
        assert_eq!(proxy_for(&hub, &env(&[("HTTP_PROXY", "http://proxy:3128")])), None, "wss uses HTTPS_PROXY");
        for no in ["example.com", ".example.com", "hub.example.com", "*", "other.org, example.com:443"] {
            let pairs: &'static [(&'static str, &'static str)] = Box::leak(vec![("HTTPS_PROXY", "http://proxy:3128"), ("NO_PROXY", no)].into_boxed_slice());
            assert_eq!(proxy_for(&hub, &env(pairs)), None, "{no:?}");
        }
        assert!(proxy_for(&hub, &env(&[("HTTPS_PROXY", "http://proxy:3128"), ("NO_PROXY", "ample.com")])).is_some());
        assert_eq!(proxy_for(&parse("tcp://127.0.0.1:1").unwrap(), &env(&[("ALL_PROXY", "http://proxy:3128")])), None);
    }

    /// A WebSocket server on loopback that echoes one binary message.
    fn echo_server() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader: Box<dyn Read + Send> = Box::new(stream.try_clone().unwrap());
            let mut writer: Box<dyn Write + Send> = Box::new(stream);
            ws::accept(&[], &mut reader, &mut writer).unwrap();
            let (mut r, mut w) = ws::split(ws::Role::Server, reader, writer);
            let mut buffer = [0u8; 5];
            r.read_exact(&mut buffer).unwrap();
            w.write_all(&buffer).unwrap();
            w.flush().unwrap();
        });
        (port, handle)
    }

    #[test]
    fn a_websocket_reaches_the_hub_directly_and_through_a_connect_proxy() {
        let (port, server) = echo_server();
        let mut dialed = dial(&format!("ws://127.0.0.1:{port}/"), Vec::new(), &[]).unwrap();
        dialed.writer.write_all(b"hello").unwrap();
        dialed.writer.flush().unwrap();
        let mut back = [0u8; 5];
        dialed.reader.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello");
        server.join().unwrap();

        // A CONNECT proxy stand-in.
        let (port, server) = echo_server();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let asked = Arc::new(Mutex::new(String::new()));
        let seen = asked.clone();
        let relay = std::thread::spawn(move || {
            let (mut client, _) = proxy.accept().unwrap();
            let head = read_head(&mut client).unwrap();
            *seen.lock().unwrap() = head.lines().next().unwrap_or_default().to_owned();
            let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            let (mut a, mut b) = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            let up = std::thread::spawn(move || {
                let _ = io::copy(&mut a, &mut b);
            });
            let (mut c, mut d) = (upstream, client);
            let _ = io::copy(&mut c, &mut d);
            let _ = up.join();
        });
        let target = parse(&format!("ws://127.0.0.1:{port}/")).unwrap();
        let stream = connect(&target, Some(&format!("http://127.0.0.1:{proxy_port}"))).unwrap();
        let mut reader: Box<dyn Read + Send> = Box::new(stream.try_clone().unwrap());
        let mut writer: Box<dyn Write + Send> = Box::new(stream.try_clone().unwrap());
        ws::connect(&mut reader, &mut writer, &format!("127.0.0.1:{port}"), "/", &[]).unwrap();
        let (mut r, mut w) = ws::split(ws::Role::Client, reader, writer);
        w.write_all(b"proxy").unwrap();
        w.flush().unwrap();
        let mut back = [0u8; 5];
        r.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"proxy");
        assert_eq!(*asked.lock().unwrap(), format!("CONNECT 127.0.0.1:{port} HTTP/1.1"));
        server.join().unwrap();
        let _ = stream.shutdown(Shutdown::Both);
        let _ = relay.join();
    }

    /// A TLS stream shared by the test server's two halves.
    #[derive(Clone)]
    struct Locked(Arc<Mutex<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>);

    impl Read for Locked {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.0.lock().unwrap().read(out)
        }
    }

    impl Write for Locked {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(data)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }

    /// A `wss://` server on loopback with the test certificate (testdata/:
    /// a throwaway CA, P-256, valid 2019–2099, made with OpenSSL 3.6 `req -x509
    /// -not_before 20190101000000Z -not_after 20991231235959Z`, its key
    /// discarded; the server's for `localhost` and 127.0.0.1) that echoes one
    /// binary message; `None` when the handshake failed.
    fn tls_echo_server() -> (u16, std::thread::JoinHandle<Option<()>>) {
        use rustls::pki_types::PrivateKeyDer;
        use rustls::pki_types::pem::PemObject;
        let chain = crate::vm::tls::pem_certificates(include_bytes!("testdata/test-server.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("testdata/test-server.key")).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(chain, key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
            let mut conn = rustls::ServerConnection::new(Arc::new(config)).ok()?;
            let mut socket = stream;
            while conn.is_handshaking() {
                conn.complete_io(&mut socket).ok()?;
            }
            let shared = Locked(Arc::new(Mutex::new(rustls::StreamOwned::new(conn, socket))));
            let (mut reader, mut writer) = (shared.clone(), shared.clone());
            ws::accept(&[], &mut reader, &mut writer).ok()?;
            let (mut r, mut w) = ws::split(ws::Role::Server, Box::new(reader), Box::new(writer));
            let mut buffer = [0u8; 5];
            r.read_exact(&mut buffer).ok()?;
            w.write_all(&buffer).ok()?;
            w.flush().ok()?;
            Some(())
        });
        (port, handle)
    }

    #[test]
    fn wss_trusts_the_packs_ca_and_refuses_a_server_without_it() {
        let ca = crate::vm::tls::pem_certificates(include_bytes!("testdata/test-ca.pem")).unwrap();
        let (port, server) = tls_echo_server();
        let mut dialed = dial(&format!("wss://localhost:{port}/"), ca, &[]).unwrap_or_else(|e| panic!("{e:#}"));
        dialed.writer.write_all(b"tls!!").unwrap();
        dialed.writer.flush().unwrap();
        let mut back = [0u8; 5];
        dialed.reader.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"tls!!");
        dialed.closer.close();
        assert_eq!(server.join().unwrap(), Some(()));

        // The platform's roots alone do not know the test CA.
        let (port, server) = tls_echo_server();
        let error = dial(&format!("wss://localhost:{port}/"), Vec::new(), &[]).err().expect("an unknown CA is refused");
        assert!(format!("{error:#}").contains("TLS handshake"), "{error:#}");
        assert_eq!(server.join().unwrap(), None);
    }
}

