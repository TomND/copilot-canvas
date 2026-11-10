//! The MCP gateway on the hub (plan VM-37; worker P-mcp, docs/VM-WORKERS.md):
//! a guest agent's MCP session with one of the hub's servers arrives as a
//! stream on the satellite's link (`SatUp::Mcp`, `canvas_common::sat::McpOpen`);
//! the gateway checks it against the pack's `mcp-policy.yaml`, starts the
//! hub's server for that session and relays JSON-RPC, filtering and refusing
//! what the policy does not allow. The guest never gets a server's
//! credentials.
//!
//! This is the interface the lead wires (`vm::link` calls [`Gateway::up`],
//! [`Gateway::link_ended`], [`Gateway::session_ended`]); until the worker's
//! package lands, every stream is refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use canvas_common::sat::{MCP_CLOSE, MCP_ERROR, MCP_OPEN};

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
    #[allow(dead_code)]
    home: PathBuf,
    #[allow(dead_code)]
    servers: Arc<dyn ServerSource>,
    down: Arc<dyn Down>,
}

impl Gateway {
    pub fn new(home: &Path, servers: Arc<dyn ServerSource>, down: Arc<dyn Down>) -> Arc<Gateway> {
        Arc::new(Gateway { home: home.to_path_buf(), servers, down })
    }

    /// One MCP message from `sandbox`'s satellite (the link checked who it is).
    pub fn up(&self, sandbox: &str, stream: u64, kind: u8, _bytes: Vec<u8>) {
        if kind == MCP_OPEN {
            let refusal = serde_json::json!({ "message": "MCP servers are not available in VM sessions yet" });
            self.down.send(sandbox, stream, MCP_ERROR, refusal.to_string().into_bytes());
            self.down.send(sandbox, stream, MCP_CLOSE, Vec::new());
        }
    }

    /// `sandbox`'s link ended: its streams are closed.
    pub fn link_ended(&self, _sandbox: &str) {}

    /// A box's program ended: its sessions' servers end.
    pub fn session_ended(&self, _term: &str) {}
}
