//! The guest side of the MCP gateway (plan VM-37; worker P-mcp,
//! docs/VM-WORKERS.md): an MCP server (Streamable HTTP) on the guest's
//! loopback, `/mcp/<term>/<server>`, whose sessions go to the hub as streams
//! on the link (`SatUp::Mcp`); the hub's answers come back through
//! [`McpProxy::down`]. Its port is in `HelloInfo::mcp_port`.
//!
//! This is the interface the satellite wires; until the worker's package
//! lands it serves nothing (port 0).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use canvas_common::sat::SatUp;
use crossbeam_channel::Sender;

#[derive(Default)]
pub struct McpProxy {
    up: Mutex<Option<Sender<SatUp>>>,
}

impl McpProxy {
    /// Starts serving on the guest's loopback (`port` 0: any free port).
    pub fn start(port: u16) -> Result<Arc<McpProxy>> {
        let _ = port;
        Ok(Arc::default())
    }

    /// The port it serves on (0: none).
    pub fn port(&self) -> u16 {
        0
    }

    /// The link its streams go up on now (`None`: no link; every stream has ended).
    pub fn attach(&self, up: Option<Sender<SatUp>>) {
        *self.up.lock().unwrap_or_else(|e| e.into_inner()) = up;
    }

    /// One message from the hub for stream `stream`.
    pub fn down(&self, stream: u64, kind: u8, bytes: Vec<u8>) {
        let _ = (stream, kind, bytes);
    }

    /// The link dropped: every stream ends (the guest's MCP clients start new sessions).
    pub fn link_lost(&self) {}
}
