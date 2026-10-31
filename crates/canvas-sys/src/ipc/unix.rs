//! Unix domain sockets: the socket's folder is 0700 and the socket 0600,
//! every accepted peer's uid must be ours, and one server per name holds an
//! exclusive `flock` on `<socket>.lock` for its whole life before it
//! (re)binds the socket, so a stale socket left by a crash is replaced and a
//! running server is never displaced.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The uid and pid of a connected peer.
fn peer(stream: &UnixStream) -> io::Result<(u32, Option<u32>)> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a socket and an out-buffer of the given length.
        if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((cred.uid, (cred.pid > 0).then_some(cred.pid as u32)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: a socket and two out-pointers.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: LOCAL_PEERPID writes a pid_t.
        let known = unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) } == 0 && pid > 0;
        Ok((uid, known.then_some(pid as u32)))
    }
}

/// One end of a connected socket.
pub struct Pipe {
    stream: UnixStream,
    peer_pid: Option<u32>,
}

impl Pipe {
    /// The other end's process (servers: the client).
    pub fn client_pid(&self) -> Option<u32> {
        self.peer_pid
    }

    pub fn split(self) -> io::Result<(PipeReader, PipeWriter, PipeCloser)> {
        let shared = Arc::new(Shared { stream: self.stream, closed: AtomicBool::new(false) });
        Ok((PipeReader { shared: shared.clone() }, PipeWriter { shared: shared.clone() }, PipeCloser { shared }))
    }
}

struct Shared {
    stream: UnixStream,
    closed: AtomicBool,
}

pub struct PipeReader {
    shared: Arc<Shared>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Ok(0);
        }
        match (&self.shared.stream).read(buf) {
            Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe) => Ok(0),
            other => other,
        }
    }
}

pub struct PipeWriter {
    shared: Arc<Shared>,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        }
        (&self.shared.stream).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Shuts the connection down so blocked reader and writer threads return.
#[derive(Clone)]
pub struct PipeCloser {
    shared: Arc<Shared>,
}

impl PipeCloser {
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        let _ = self.shared.stream.shutdown(std::net::Shutdown::Both);
    }
}

pub struct PipeServer {
    listener: UnixListener,
    path: PathBuf,
    /// Held (with its `flock`) for the server's life.
    _lock: std::fs::File,
}

impl PipeServer {
    /// Binds the socket at `name` (a path). Fails with `AlreadyExists` when
    /// another process serves it.
    pub fn bind(name: &str) -> io::Result<Self> {
        let path = PathBuf::from(name);
        let folder = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path needs a folder"))?;
        crate::fs::private_dir(folder)?;
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(lock_path(&path))?;
        // SAFETY: flock on an open file.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "socket already in use"));
            }
            return Err(error);
        }
        // A socket left by a server that died: nobody holds the lock.
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { listener, path, _lock: lock })
    }

    /// Blocks until a client of this user connects (others are dropped).
    pub fn accept(&mut self) -> io::Result<Pipe> {
        // SAFETY: geteuid cannot fail.
        let own = unsafe { libc::geteuid() };
        loop {
            let (stream, _) = self.listener.accept()?;
            match peer(&stream) {
                Ok((uid, pid)) if uid == own => return Ok(Pipe { stream, peer_pid: pid }),
                _ => drop(stream),
            }
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Connects to a server's socket. Like a pipe that does not exist, a socket
/// nobody serves fails at once (`NotFound` or `ConnectionRefused`).
pub fn connect(name: &str, timeout: Duration) -> io::Result<Pipe> {
    let _ = timeout;
    let stream = UnixStream::connect(name)?;
    let (uid, pid) = peer(&stream)?;
    // SAFETY: geteuid cannot fail.
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{name} is served by another user")));
    }
    Ok(Pipe { stream, peer_pid: pid })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_socket() -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("cs-ipc-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        let name = dir.join("t.sock").to_string_lossy().into_owned();
        (dir, name)
    }

    #[test]
    fn one_server_per_name_and_duplex_halves() {
        let (dir, name) = temp_socket();
        let mut server = PipeServer::bind(&name).unwrap();
        assert_eq!(PipeServer::bind(&name).err().map(|e| e.kind()), Some(io::ErrorKind::AlreadyExists));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let client = std::thread::spawn({
            let name = name.clone();
            move || {
                let (mut reader, mut writer, _closer) = connect(&name, Duration::from_secs(1)).unwrap().split().unwrap();
                writer.write_all(b"ping").unwrap();
                let mut buf = [0u8; 4];
                reader.read_exact(&mut buf).unwrap();
                buf
            }
        });
        let pipe = server.accept().unwrap();
        assert_eq!(pipe.client_pid(), Some(std::process::id()));
        let (mut reader, mut writer, closer) = pipe.split().unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        writer.write_all(b"pong").unwrap();
        assert_eq!(&client.join().unwrap(), b"pong");
        // A closer wakes a blocked reader.
        let blocked = std::thread::spawn(move || reader.read(&mut [0u8; 8]).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        closer.close();
        assert_eq!(blocked.join().unwrap(), 0);
        drop(server);
        // A stale lock file and no server: binding works again.
        drop(PipeServer::bind(&name).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
