//! Local IPC between the service and its clients on this machine (and the
//! GUI's test hooks): owner-only named pipes on Windows, Unix domain sockets
//! on macOS and Linux (MAC-LINUX-PLAN D4). The endpoint name is
//! `Instance::pipe_name()`: a pipe name on Windows, a socket path elsewhere.
//!
//! The same shape on every OS: `PipeServer::bind` fails with `AlreadyExists`
//! while another process serves the name; a connection splits into a reader,
//! a writer and a closer that wakes a blocked reader.

#[cfg(windows)]
pub use canvas_common::pipe::{Pipe, PipeCloser, PipeReader, PipeServer, PipeWriter, connect};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
