//! Linux: `/proc/net/tcp` and `/proc/net/tcp6` (addresses, state, socket
//! inode), and `/proc/<pid>/fd` to find the process holding an inode.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

struct Row {
    local: SocketAddr,
    remote: SocketAddr,
    listening: bool,
    inode: u64,
}

/// `0100007F:1F90` (IPv4) or 32 hex digits and a port (IPv6), each 32-bit
/// word in host byte order.
fn parse_address(text: &str) -> Option<SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Vec<u32> = (0..ip.len() / 8).map(|i| u32::from_str_radix(&ip[i * 8..i * 8 + 8], 16).ok()).collect::<Option<_>>()?;
    let ip = match words.as_slice() {
        [v4] => IpAddr::V4(Ipv4Addr::from(v4.to_ne_bytes())),
        [a, b, c, d] => {
            let mut bytes = [0u8; 16];
            for (i, word) in [a, b, c, d].iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn rows() -> Vec<Row> {
    let mut rows = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(remote), Some(state), Some(inode)) = (fields.get(1), fields.get(2), fields.get(3), fields.get(9)) else { continue };
            let (Some(local), Some(remote), Ok(inode)) = (parse_address(local), parse_address(remote), inode.parse::<u64>()) else { continue };
            rows.push(Row { local, remote, listening: *state == "0A", inode });
        }
    }
    rows
}

/// The processes (that this user may inspect) holding each of `inodes`.
fn owners(inodes: &[u64]) -> Vec<(u64, u32)> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return found };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            let target = target.to_string_lossy();
            if let Some(inode) = target.strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')).and_then(|t| t.parse::<u64>().ok())
                && inodes.contains(&inode)
            {
                found.push((inode, pid));
            }
        }
    }
    found
}

/// Every listening TCP socket with the process that owns it.
pub fn listeners() -> io::Result<Vec<(SocketAddr, u32)>> {
    let listening: Vec<Row> = rows().into_iter().filter(|r| r.listening && r.inode != 0).collect();
    let inodes: Vec<u64> = listening.iter().map(|r| r.inode).collect();
    let owners = owners(&inodes);
    Ok(listening.iter().flat_map(|row| owners.iter().filter(|(inode, _)| *inode == row.inode).map(|(_, pid)| (row.local, *pid))).collect())
}

/// The process that owns the other end of a loopback connection whose ends
/// are `local` (ours) and `peer`.
pub fn peer_process(local: SocketAddr, peer: SocketAddr) -> io::Result<u32> {
    let missing = || io::Error::new(io::ErrorKind::NotFound, format!("no process owns the connection from {peer}"));
    let row = rows().into_iter().find(|r| !r.listening && r.local == peer && r.remote == local && r.inode != 0).ok_or_else(missing)?;
    owners(&[row.inode]).first().map(|(_, pid)| *pid).ok_or_else(missing)
}
