//! TCP transport for remote clients. The service only listens on loopback;
//! remote access always goes through a Dev Tunnel that forwards to it. Like
//! `pipe.rs`, a connection splits into a reader, a writer and a closer.
//!
//! Winsock does not wake a blocked `recv` when another thread shuts the socket
//! down, so readers wait for data with `WSAPoll` in short slices and end once
//! the connection was closed. The same slices enforce a read deadline (the
//! service's handshake timeout) without a thread per connection.

use std::io::{self, Read, Write};
#[cfg(windows)]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS, TCP_TABLE_OWNER_PID_LISTENER,
};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, POLLRDNORM, SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, WSAPOLLFD, WSAPoll, setsockopt};

/// How often a waiting reader checks whether its connection was closed.
const POLL_SLICE_MS: i32 = 100;

struct Shared {
    stream: TcpStream,
    closed: AtomicBool,
    /// Reads fail once `epoch` is this many ms old (0: no deadline).
    deadline_ms: AtomicU64,
    epoch: Instant,
}

impl Shared {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Time left before the read deadline (`None`: no deadline).
    fn remaining(&self) -> Option<Duration> {
        match self.deadline_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(ms).saturating_sub(self.epoch.elapsed())),
        }
    }
}

/// Waits up to `ms` for the socket to be readable (data, end of stream or error).
#[cfg(windows)]
fn readable(stream: &TcpStream, ms: i32) -> io::Result<bool> {
    let mut fd = WSAPOLLFD { fd: stream.as_raw_socket() as SOCKET, events: POLLRDNORM, revents: 0 };
    let ready = unsafe { WSAPoll(&mut fd, 1, ms) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready > 0)
}

/// Elsewhere (macOS and iOS: the mock service, tests) a peek with a read
/// timeout does the same; `shutdown` also wakes a blocked read there.
#[cfg(not(windows))]
fn readable(stream: &TcpStream, ms: i32) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_millis(ms.max(1) as u64)))?;
    let result = match stream.peek(&mut [0u8; 1]) {
        Ok(_) => Ok(true),
        Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => Ok(false),
        // Errors (a reset) are for the read itself to report.
        Err(_) => Ok(true),
    };
    let _ = stream.set_read_timeout(None);
    result
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
            let slice = match self.shared.remaining() {
                Some(left) if left.is_zero() => return Err(io::Error::new(io::ErrorKind::TimedOut, "the peer did not finish in time")),
                Some(left) => (left.as_millis() as i32).clamp(1, POLL_SLICE_MS),
                None => POLL_SLICE_MS,
            };
            if readable(&self.stream, slice)? {
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

    /// Reads fail with `TimedOut` once `within` has passed (`None`: no deadline).
    pub fn set_deadline(&self, within: Option<Duration>) {
        let ms = within.map_or(0, |d| (self.0.epoch.elapsed() + d).as_millis().max(1) as u64);
        self.0.deadline_ms.store(ms, Ordering::Release);
    }
}

/// Nagle off (keystrokes and small frames go out at once), keepalive on.
pub fn split(stream: TcpStream) -> io::Result<(TcpReader, TcpWriter, TcpCloser)> {
    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(10)).with_interval(Duration::from_secs(2));
    let _ = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive);
    let shared = Arc::new(Shared { stream: stream.try_clone()?, closed: AtomicBool::new(false), deadline_ms: AtomicU64::new(0), epoch: Instant::now() });
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

/// Binds a listener on a loopback IP literal (`127.0.0.1:PORT` or
/// `[::1]:PORT`; names and other addresses are refused) with
/// `SO_EXCLUSIVEADDRUSE`, so no other process can bind the same port.
pub fn listen_loopback(addr: &str) -> io::Result<TcpListener> {
    let parsed: SocketAddr = addr.trim().parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{addr:?} is not an IP address and port; use 127.0.0.1:PORT"))
    })?;
    if !parsed.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{addr} is not a loopback address; remote access goes through a Dev Tunnel"),
        ));
    }
    let socket = socket2::Socket::new(socket2::Domain::for_address(parsed), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    #[cfg(windows)]
    {
        let on: i32 = 1;
        // SAFETY: a valid socket and a 4-byte option value that outlives the call.
        if unsafe { setsockopt(socket.as_raw_socket() as SOCKET, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, (&raw const on).cast(), 4) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    socket.bind(&parsed.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

/// Whether a TCP peer may authenticate with the service token: it connected
/// from loopback and its process runs as `own_user` (a SID string).
pub fn token_peer_allowed(peer: SocketAddr, peer_user: Option<&str>, own_user: &str) -> bool {
    peer.ip().is_loopback() && !own_user.is_empty() && peer_user == Some(own_user)
}

/// The process that owns the other end of a loopback connection whose ends
/// are `local` (ours) and `peer`.
#[cfg(windows)]
pub fn peer_process(local: SocketAddr, peer: SocketAddr) -> io::Result<u32> {
    let family = if peer.is_ipv4() { AF_INET } else { AF_INET6 };
    let table = tcp_table(family as u32, TCP_TABLE_OWNER_PID_CONNECTIONS)?;
    let pid = match (local.ip(), peer.ip()) {
        (IpAddr::V4(ours), IpAddr::V4(theirs)) => rows::<MIB_TCPROW_OWNER_PID>(&table).find_map(|row| {
            let row_local = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
            let row_remote = Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes());
            (row_local == theirs && port(row.dwLocalPort) == peer.port() && row_remote == ours && port(row.dwRemotePort) == local.port()).then_some(row.dwOwningPid)
        }),
        (IpAddr::V6(ours), IpAddr::V6(theirs)) => rows::<MIB_TCP6ROW_OWNER_PID>(&table).find_map(|row| {
            let (row_local, row_remote) = (Ipv6Addr::from(row.ucLocalAddr), Ipv6Addr::from(row.ucRemoteAddr));
            (row_local == theirs && port(row.dwLocalPort) == peer.port() && row_remote == ours && port(row.dwRemotePort) == local.port()).then_some(row.dwOwningPid)
        }),
        _ => None,
    };
    pid.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process owns the connection from {peer}")))
}

/// Every listening TCP socket (IPv4 and IPv6) with the process that owns it.
#[cfg(windows)]
pub fn listeners() -> io::Result<Vec<(SocketAddr, u32)>> {
    let v4 = tcp_table(AF_INET as u32, TCP_TABLE_OWNER_PID_LISTENER)?;
    let v6 = tcp_table(AF_INET6 as u32, TCP_TABLE_OWNER_PID_LISTENER)?;
    let mut found: Vec<(SocketAddr, u32)> =
        rows::<MIB_TCPROW_OWNER_PID>(&v4).map(|row| (SocketAddr::from((Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()), port(row.dwLocalPort))), row.dwOwningPid)).collect();
    found.extend(rows::<MIB_TCP6ROW_OWNER_PID>(&v6).map(|row| (SocketAddr::from((Ipv6Addr::from(row.ucLocalAddr), port(row.dwLocalPort))), row.dwOwningPid)));
    Ok(found)
}

/// The processes listening where a connection to `127.0.0.1:port` (or
/// `[::1]:port`) would go: loopback or wildcard listeners on that port.
pub fn listener_owners(port: u16) -> io::Result<Vec<u32>> {
    let mut owners: Vec<u32> = listeners()?.into_iter().filter(|(addr, _)| addr.port() == port && (addr.ip().is_loopback() || addr.ip().is_unspecified())).map(|(_, pid)| pid).collect();
    owners.sort_unstable();
    owners.dedup();
    Ok(owners)
}

/// Elsewhere the owners of connections and listeners are not looked up.
#[cfg(not(windows))]
pub fn peer_process(_local: SocketAddr, peer: SocketAddr) -> io::Result<u32> {
    Err(io::Error::new(io::ErrorKind::Unsupported, format!("cannot look up the process of {peer} on this platform")))
}

#[cfg(not(windows))]
pub fn listeners() -> io::Result<Vec<(SocketAddr, u32)>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "cannot list TCP listeners on this platform"))
}

/// A port field of a TCP table row (network byte order in the low 16 bits).
#[cfg(windows)]
fn port(raw: u32) -> u16 {
    u16::from_be((raw & 0xffff) as u16)
}

/// `GetExtendedTcpTable` with owning pids as 8-byte aligned memory.
#[cfg(windows)]
fn tcp_table(family: u32, class: TCP_TABLE_CLASS) -> io::Result<Vec<u64>> {
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0u32;
    for _ in 0..4 {
        let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
        let mut capacity = (buffer.len() * 8) as u32;
        // SAFETY: `buffer` has `capacity` writable bytes.
        let result = unsafe { GetExtendedTcpTable(buffer.as_mut_ptr().cast(), &mut capacity, 0, family, class, 0) };
        match result {
            0 => return Ok(buffer),
            ERROR_INSUFFICIENT_BUFFER => size = capacity + 4096,
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
    Err(io::Error::other("the TCP table kept growing"))
}

/// The rows of a `MIB_TCP*TABLE_OWNER_PID` (a u32 count, then the rows).
#[cfg(windows)]
fn rows<T: Copy>(table: &[u64]) -> impl Iterator<Item = T> + '_ {
    let count = table.first().map_or(0, |first| (*first & 0xffff_ffff) as usize);
    // Rows start after the count, at the row type's alignment (4 bytes for both kinds).
    let start = std::mem::align_of::<T>().max(4);
    let available = (table.len() * 8).saturating_sub(start) / std::mem::size_of::<T>();
    let base = table.as_ptr().cast::<u8>();
    // SAFETY: `count` rows of `T` follow the count inside `table` (bounded by its length).
    (0..count.min(available)).map(move |i| unsafe { base.add(start + i * std::mem::size_of::<T>()).cast::<T>().read_unaligned() })
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
    fn only_loopback_ip_literals_are_accepted() {
        assert!(listen_loopback("127.0.0.1:0").is_ok());
        assert_eq!(listen_loopback("0.0.0.0:0").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(listen_loopback("192.0.2.10:0").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(listen_loopback("localhost:0").unwrap_err().kind(), io::ErrorKind::InvalidInput, "names are refused");
        assert_eq!(listen_loopback("127.0.0.1").unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    #[cfg(windows)]
    fn the_owner_of_a_listening_port_is_found() {
        let listener = listen_loopback("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(listener_owners(port).unwrap(), [std::process::id()]);
        assert!(listeners().unwrap().contains(&(SocketAddr::from(([127, 0, 0, 1], port)), std::process::id())));
        let v6 = listen_loopback("[::1]:0").unwrap();
        let port6 = v6.local_addr().unwrap().port();
        assert_eq!(listener_owners(port6).unwrap(), [std::process::id()], "IPv6 listeners count too");
        drop(listener);
        assert!(listener_owners(port).unwrap().is_empty(), "nobody listens once it is closed");
    }

    #[test]
    #[cfg(windows)]
    fn a_bound_port_cannot_be_bound_again() {
        let first = listen_loopback("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        assert!(listen_loopback(&format!("127.0.0.1:{port}")).is_err(), "exclusive address use");
        // Not even with SO_REUSEADDR, the classic way to squat a port on Windows.
        let squatter = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        squatter.set_reuse_address(true).unwrap();
        assert!(squatter.bind(&SocketAddr::from(([127, 0, 0, 1], port)).into()).is_err());
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

    #[test]
    fn a_read_deadline_ends_a_silent_or_trickling_peer() {
        let (server, client) = pair();
        let (mut reader, _writer, closer) = split(server).unwrap();
        let (_creader, mut cwriter, _ccloser) = split(client).unwrap();
        closer.set_deadline(Some(Duration::from_millis(300)));
        let started = Instant::now();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                if cwriter.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let mut sink = Vec::new();
        let error = reader.read_to_end(&mut sink).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(800), "a trickle does not extend the deadline");
        closer.set_deadline(None);
        trickle.join().unwrap();
    }

    #[test]
    fn token_peers_must_be_local_and_the_same_user() {
        let local = SocketAddr::from(([127, 0, 0, 1], 5000));
        let lan = SocketAddr::from(([192, 0, 2, 7], 5000));
        assert!(token_peer_allowed(local, Some("S-1-5-21-1"), "S-1-5-21-1"));
        assert!(!token_peer_allowed(local, Some("S-1-5-21-2"), "S-1-5-21-1"), "another user on this machine");
        assert!(!token_peer_allowed(local, None, "S-1-5-21-1"), "an unknown owner");
        assert!(!token_peer_allowed(lan, Some("S-1-5-21-1"), "S-1-5-21-1"), "not loopback");
        assert!(!token_peer_allowed(local, Some(""), ""), "no own user");
    }

    #[test]
    #[cfg(windows)]
    fn the_peer_of_a_loopback_connection_is_this_process_and_user() {
        let (server, _client) = pair();
        let pid = peer_process(server.local_addr().unwrap(), server.peer_addr().unwrap()).unwrap();
        assert_eq!(pid, std::process::id());
        let own = crate::win::current_user_sid().unwrap();
        assert_eq!(crate::win::process_user_sid(pid).unwrap(), own);
        assert!(token_peer_allowed(server.peer_addr().unwrap(), Some(&own), &own));
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
