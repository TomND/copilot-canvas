//! Local IPC between the service and its clients on this machine (and the
//! GUI's test hooks): owner-only named pipes on Windows, Unix domain sockets
//! on macOS and Linux (MAC-LINUX-PLAN D4). The endpoint name is
//! `Instance::pipe_name()`: a pipe name on Windows, a socket path elsewhere.
//!
//! The same shape on every OS: `PipeServer::bind` fails with `AlreadyExists`
//! while another process serves the name; a connection splits into a reader,
//! a writer and a closer that wakes a blocked reader.

/// The endpoint for a per-process local name (the GUI's test hooks):
/// `\\.\pipe\NAME` on Windows, `<runtime or temp>/atelier-<user>/NAME.sock`
/// elsewhere (a 0700 folder of this user; `/tmp` when the temporary folder's
/// path is too long for a socket address).
pub fn local_name(name: &str) -> String {
    #[cfg(windows)]
    return format!(r"\\.\pipe\{name}");
    #[cfg(not(windows))]
    canvas_common::paths::short_socket_path(&format!("{name}.sock"))
}

/// Whether `name` is a local endpoint [`local_name`] made.
pub fn is_local_name(name: &str) -> bool {
    name.starts_with(r"\\.\pipe\") || name.starts_with('/')
}

#[cfg(windows)]
pub use canvas_common::pipe::{Pipe, PipeCloser, PipeReader, PipeServer, PipeWriter, connect};

/// Whether a server holds the endpoint `name`, also one too busy to take a
/// connection now (a pipe whose instances are all connected; a socket whose
/// server holds its lock): only a missing endpoint, or a socket nobody
/// serves, is free. For "never two services" checks, where a connection that
/// fails for a moment must not count as no server.
#[cfg(windows)]
pub fn is_served(name: &str) -> bool {
    match connect(name, std::time::Duration::from_millis(500)) {
        Ok(_) => true,
        // ERROR_FILE_NOT_FOUND (2), ERROR_PATH_NOT_FOUND (3): no such pipe.
        Err(error) => !matches!(error.raw_os_error(), Some(2 | 3)),
    }
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
