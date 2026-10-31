//! Who owns a loopback TCP connection or listener (the service's token
//! check for loopback clients, OpenCode's listener check, the tunnel's
//! listener probe).

pub use canvas_common::tcp::{listener_owners, listeners, peer_process};
