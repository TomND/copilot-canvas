//! macOS: every inspectable process's TCP sockets through libproc
//! (`PROC_PIDLISTFDS`, then `PROC_PIDFDSOCKETINFO`). `struct socket_fdinfo`
//! is read at the offsets of `<sys/proc_info.h>` (the same on arm64 and
//! x86_64; a kernel ABI), checked by this module's test against real sockets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const PROC_PIDFDSOCKETINFO: i32 = 3;
const SOCKET_FDINFO_SIZE: usize = 792;
const OFF_KIND: usize = 256;
const OFF_FPORT: usize = 264;
const OFF_LPORT: usize = 268;
const OFF_VFLAG: usize = 288;
const OFF_FADDR: usize = 296;
const OFF_LADDR: usize = 312;
const OFF_STATE: usize = 344;
const SOCKINFO_TCP: i32 = 2;
const INI_IPV4: u8 = 1;
const INI_IPV6: u8 = 2;
const TSI_S_LISTEN: i32 = 1;

struct TcpSocket {
    local: SocketAddr,
    remote: SocketAddr,
    listening: bool,
}

fn int(buffer: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(buffer[at..at + 4].try_into().expect("4 bytes"))
}

fn address(buffer: &[u8], at: usize, port_at: usize, v4: bool) -> SocketAddr {
    let port = u16::from_be(int(buffer, port_at) as u16);
    let bytes: [u8; 16] = buffer[at..at + 16].try_into().expect("16 bytes");
    let ip = if v4 { IpAddr::V4(Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15])) } else { IpAddr::V6(Ipv6Addr::from(bytes)) };
    SocketAddr::new(ip, port)
}

/// The TCP sockets `pid` has open (none when it cannot be inspected).
fn tcp_sockets(pid: u32) -> Vec<TcpSocket> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks for the size.
    let size = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut fds = vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; size as usize / entry + 16];
    // SAFETY: `fds` has the byte size given.
    let filled = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (fds.len() * entry) as i32) };
    fds.truncate(filled.max(0) as usize / entry);
    let mut found = Vec::new();
    let mut buffer = vec![0u8; SOCKET_FDINFO_SIZE];
    for fd in fds.iter().filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
        // SAFETY: `buffer` holds one socket_fdinfo.
        let read = unsafe { libc::proc_pidfdinfo(pid as i32, fd.proc_fd, PROC_PIDFDSOCKETINFO, buffer.as_mut_ptr().cast(), SOCKET_FDINFO_SIZE as i32) };
        if read as usize != SOCKET_FDINFO_SIZE || int(&buffer, OFF_KIND) != SOCKINFO_TCP {
            continue;
        }
        let vflag = buffer[OFF_VFLAG];
        // A dual-stack socket reports both flags: its addresses are IPv6 (v4-mapped).
        let v4 = vflag & INI_IPV4 != 0 && vflag & INI_IPV6 == 0;
        let normalize = |addr: SocketAddr| match addr.ip() {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(addr, |v4| SocketAddr::new(IpAddr::V4(v4), addr.port())),
            _ => addr,
        };
        found.push(TcpSocket {
            local: normalize(address(&buffer, OFF_LADDR, OFF_LPORT, v4)),
            remote: normalize(address(&buffer, OFF_FADDR, OFF_FPORT, v4)),
            listening: int(&buffer, OFF_STATE) == TSI_S_LISTEN,
        });
    }
    found
}

/// Every listening TCP socket of an inspectable process, with its owner.
pub fn listeners() -> io::Result<Vec<(SocketAddr, u32)>> {
    Ok(crate::process::all_pids().into_iter().flat_map(|pid| tcp_sockets(pid).into_iter().filter(|s| s.listening).map(move |s| (s.local, pid))).collect())
}

/// The process that owns the other end of a loopback connection whose ends
/// are `local` (ours) and `peer`.
pub fn peer_process(local: SocketAddr, peer: SocketAddr) -> io::Result<u32> {
    crate::process::all_pids()
        .into_iter()
        .find(|&pid| tcp_sockets(pid).iter().any(|s| !s.listening && s.local == peer && s.remote == local))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process owns the connection from {peer}")))
}
