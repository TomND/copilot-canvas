//! TCP transport for remote clients. The service only listens on loopback;
//! remote access always goes through a Dev Tunnel that forwards to it. Like
//! `pipe.rs`, a connection splits into a reader, a writer and a closer.
//!
//! Winsock does not wake a blocked `recv` when another thread shuts the socket
//! down, so readers wait for data with `WSAPoll` in short slices and end once
//! the connection was closed.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};

/// How often a waiting reader checks whether its connection was closed.
const POLL_SLICE_MS: i32 = 100;

struct Shared {
    stream: TcpStream,
    closed: AtomicBool,
}

impl Shared {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Waits up to `ms` for the socket to be readable (data, end of stream or error).
fn readable(stream: &TcpStream, ms: i32) -> io::Result<bool> {
    let mut fd = WSAPOLLFD { fd: stream.as_raw_socket() as SOCKET, events: POLLRDNORM, revents: 0 };
    let ready = unsafe { WSAPoll(&mut fd, 1, ms) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready > 0)
}

pub struct TcpReader {
    stream: TcpStream,
    shared: Arc<Shared>,
}

impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.shared.closed() {
                return Ok(0);
            }
            if readable(&self.stream, POLL_SLICE_MS)? {
                break;
            }
        }
        match self.stream.read(buf) {
            // A reset or aborted connection ends the stream like a close.
            Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => Ok(0),
            other => other,
        }
    }
}

pub struct TcpWriter {
    stream: TcpStream,
    shared: Arc<Shared>,
}

impl Write for TcpWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.shared.closed() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        }
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Ends the connection: the peer sees it close and waiting readers return.
#[derive(Clone)]
pub struct TcpCloser(Arc<Shared>);

impl TcpCloser {
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        let _ = self.0.stream.shutdown(Shutdown::Both);
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.0.stream.peer_addr().ok()
    }
}

/// Nagle off (keystrokes and small frames go out at once), keepalive on.
pub fn split(stream: TcpStream) -> io::Result<(TcpReader, TcpWriter, TcpCloser)> {
    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(10)).with_interval(Duration::from_secs(2));
    let _ = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive);
    let shared = Arc::new(Shared { stream: stream.try_clone()?, closed: AtomicBool::new(false) });
    Ok((
        TcpReader { stream: stream.try_clone()?, shared: shared.clone() },
        TcpWriter { stream, shared: shared.clone() },
        TcpCloser(shared),
    ))
}

/// Connects to `HOST:PORT`, trying each resolved address.
pub fn connect(addr: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = None;
    for candidate in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&candidate, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{addr} did not resolve"))))
}

/// Binds a listener; addresses that are not loopback are refused.
pub fn listen_loopback(addr: &str) -> io::Result<TcpListener> {
    let candidates: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    let Some(first) = candidates.first() else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{addr} did not resolve")));
    };
    if candidates.iter().any(|a| !a.ip().is_loopback()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{addr} is not a loopback address; remote access goes through a Dev Tunnel"),
        ));
    }
    TcpListener::bind(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = listen_loopback("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let client = std::thread::spawn(move || connect(&addr, Duration::from_secs(2)).unwrap());
        let (server, _) = listener.accept().unwrap();
        (server, client.join().unwrap())
    }

    #[test]
    fn only_loopback_addresses_are_accepted() {
        assert!(listen_loopback("127.0.0.1:0").is_ok());
        assert_eq!(listen_loopback("0.0.0.0:0").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(listen_loopback("192.0.2.10:0").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn closing_unblocks_a_blocked_reader_and_ends_the_peer() {
        let (server, client) = pair();
        let (mut reader, _writer, closer) = split(server).unwrap();
        let (mut creader, mut cwriter, _ccloser) = split(client).unwrap();
        cwriter.write_all(b"hi").unwrap();
        let blocked = std::thread::spawn(move || {
            let mut buf = [0u8; 2];
            reader.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hi");
            let mut more = [0u8; 8];
            reader.read(&mut more).unwrap()
        });
        std::thread::sleep(Duration::from_millis(150));
        let started = std::time::Instant::now();
        closer.close();
        assert_eq!(blocked.join().unwrap(), 0, "the blocked read ends");
        assert!(started.elapsed() < Duration::from_secs(1));
        let mut rest = Vec::new();
        creader.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the peer sees the connection end");
    }

    /// Loopback round trip through the polling reader:
    /// `cargo test --release -p canvas-common round_trip -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn round_trip_time() {
        let (server, client) = pair();
        let (mut sr, mut sw, _sc) = split(server).unwrap();
        let (mut cr, mut cw, _cc) = split(client).unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8];
            while sr.read_exact(&mut buf).is_ok() {
                sw.write_all(&buf).unwrap();
            }
        });
        let mut samples = Vec::new();
        for _ in 0..2000 {
            let t = std::time::Instant::now();
            cw.write_all(&[1; 8]).unwrap();
            let mut buf = [0u8; 8];
            cr.read_exact(&mut buf).unwrap();
            samples.push(t.elapsed().as_secs_f64() * 1e6);
            std::thread::sleep(Duration::from_millis(1));
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("RTT us p50 {:.0} p99 {:.0}", samples[1000], samples[1980]);
    }
}
