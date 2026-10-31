//! Who owns a loopback TCP connection or listener: the service's token check
//! for loopback clients, OpenCode's listener check, the tunnel's listener
//! probe. Only processes this user may inspect are found, which is what
//! every caller needs (a connection from another user's process is refused).

use std::io;

#[cfg(windows)]
pub use canvas_common::tcp::{listeners, peer_process};

#[cfg(macos)]
mod macos;
#[cfg(macos)]
pub use macos::{listeners, peer_process};

#[cfg(linux)]
mod linux;
#[cfg(linux)]
pub use linux::{listeners, peer_process};

/// The processes listening where a connection to `127.0.0.1:port` (or
/// `[::1]:port`) would go: loopback or wildcard listeners on that port.
pub fn listener_owners(port: u16) -> io::Result<Vec<u32>> {
    let mut owners: Vec<u32> = listeners()?.into_iter().filter(|(addr, _)| addr.port() == port && (addr.ip().is_loopback() || addr.ip().is_unspecified())).map(|(_, pid)| pid).collect();
    owners.sort_unstable();
    owners.dedup();
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn loopback_connections_and_listeners_are_traced_to_this_process() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(super::listener_owners(addr.port()).unwrap(), vec![std::process::id()]);
        let client = TcpStream::connect(addr).unwrap();
        let (server, peer) = listener.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        assert_eq!(super::peer_process(server.local_addr().unwrap(), peer).unwrap(), std::process::id());
        let six = TcpListener::bind("[::1]:0").unwrap();
        let six_addr = six.local_addr().unwrap();
        let _client6 = TcpStream::connect(six_addr).unwrap();
        let (server6, peer6) = six.accept().unwrap();
        assert_eq!(super::peer_process(server6.local_addr().unwrap(), peer6).unwrap(), std::process::id());
        assert!(super::peer_process(server.local_addr().unwrap(), "127.0.0.1:9".parse().unwrap()).is_err());
    }
}
