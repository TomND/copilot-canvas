//! The MCP gateway on the hub (plan VM-37; docs/VM-SESSIONS.md §7): a guest
//! agent's MCP session with one of the hub's servers arrives as a stream on
//! the satellite's link (`SatUp::Mcp`, `canvas_common::sat::McpOpen`); the
//! gateway checks it against the pack's `mcp-policy.yaml`, starts the hub's
//! server for that session and relays JSON-RPC, filtering and refusing what
//! the policy does not allow. The guest never gets a server's credentials.
//!
//! - [`policy`]: the pack's rules for a box (default deny) and the refusals'
//!   record (`<home>/vm/mcp/refusals.jsonl`).
//! - [`gateway`]: the engine (streams, server processes, relaying).
//! - [`server`]: the hub's servers from its Copilot configuration
//!   ([`CopilotServers`]) and a running local server.
//! - [`jsonrpc`]: messages, pinned protocol versions, batches.
//! - [`standin`]: a stand-in server for tests and the probe (`atelier mcp-standin`).
//!
//! The guest's side is `vm::satellite::mcp_proxy`.

pub mod gateway;
pub mod jsonrpc;
pub mod policy;
pub mod server;
pub mod standin;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use canvas_common::sat::{MCP_CLOSE, MCP_ERROR, MCP_OPEN};
use crossbeam_channel::Sender;

pub use self::gateway::{Options, PackSource, Running};
pub use self::server::CopilotServers;

/// How the gateway answers a sandbox's satellite (`SatDown::Mcp`); `false`
/// when its link is down.
pub trait Down: Send + Sync {
    fn send(&self, sandbox: &str, stream: u64, kind: u8, bytes: Vec<u8>) -> bool;
}

/// One of the hub's MCP servers as the hub's agent configuration defines it.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerSpec {
    /// A local process speaking JSON-RPC on stdio.
    Stdio { command: String, args: Vec<String>, env: Vec<(String, String)> },
    /// A remote server (Streamable HTTP).
    Http { url: String, headers: Vec<(String, String)> },
}

/// Where the gateway finds a server's definition (the hub's agent configuration).
pub trait ServerSource: Send + Sync {
    fn spec(&self, server: &str) -> Option<ServerSpec>;
}

/// No servers (until the hub's configuration is read).
pub struct NoServers;

impl ServerSource for NoServers {
    fn spec(&self, _server: &str) -> Option<ServerSpec> {
        None
    }
}

pub struct Gateway {
    events: Sender<gateway::Event>,
    down: Arc<dyn Down>,
    /// Bytes handed to the engine and not taken yet.
    queued: Arc<AtomicUsize>,
    running: Arc<Mutex<Vec<Running>>>,
}

impl Gateway {
    pub fn new(home: &Path, servers: Arc<dyn ServerSource>, down: Arc<dyn Down>) -> Arc<Gateway> {
        Gateway::with_options(home, servers, down, Options::default())
    }

    /// A gateway with its idle time and pack given (tests).
    pub fn with_options(home: &Path, servers: Arc<dyn ServerSource>, down: Arc<dyn Down>, options: Options) -> Arc<Gateway> {
        let (events, rx) = crossbeam_channel::unbounded();
        let queued = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(Mutex::new(Vec::new()));
        let engine = (home.to_path_buf(), servers, down.clone(), options, rx, running.clone(), queued.clone());
        let started = std::thread::Builder::new().name("mcp-gateway".into()).spawn(move || {
            let (home, servers, down, options, rx, running, queued) = engine;
            gateway::run(home, servers, down, options, rx, running, queued);
        });
        if started.is_err() {
            // No engine: every stream is refused (its queue is dropped at once).
            let (refused, _) = crossbeam_channel::unbounded();
            return Arc::new(Gateway { events: refused, down, queued, running });
        }
        Arc::new(Gateway { events, down, queued, running })
    }

    /// One MCP message from `sandbox`'s satellite (the link checked who it
    /// is). It is queued for the gateway's engine: the link's reader never
    /// waits for a server.
    pub fn up(&self, sandbox: &str, stream: u64, kind: u8, bytes: Vec<u8>) {
        let size = bytes.len();
        if self.queued.fetch_add(size, Ordering::Relaxed) + size > gateway::MAX_QUEUED {
            self.queued.fetch_sub(size, Ordering::Relaxed);
            let _ = self.events.send(gateway::Event::Overflow { sandbox: sandbox.to_owned(), stream });
            return;
        }
        if self.events.send(gateway::Event::Up { sandbox: sandbox.to_owned(), stream, kind, bytes }).is_err() {
            self.queued.fetch_sub(size, Ordering::Relaxed);
            if kind == MCP_OPEN {
                let refusal = serde_json::json!({ "message": "the hub's MCP gateway is not running" });
                self.down.send(sandbox, stream, MCP_ERROR, refusal.to_string().into_bytes());
                self.down.send(sandbox, stream, MCP_CLOSE, Vec::new());
            }
        }
    }

    /// `sandbox`'s link ended: its streams are closed (and their servers, unless shared).
    pub fn link_ended(&self, sandbox: &str) {
        let _ = self.events.send(gateway::Event::LinkEnded(sandbox.to_owned()));
    }

    /// A box's program ended: its sessions' servers end.
    pub fn session_ended(&self, term: &str) {
        let _ = self.events.send(gateway::Event::SessionEnded(term.to_owned()));
    }

    /// The server processes running now.
    pub fn running(&self) -> Vec<Running> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
