//! VS Code agent sessions as the canvas syncs them (the backend reads VS
//! Code's storage; see `canvas_agents::vscode`).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct VsSession {
    pub session_id: String,
    pub label: String,
    pub archived: bool,
    pub cwd: Option<String>,
    pub created_ms: Option<u64>,
    /// Most recent request start/end or creation time.
    pub active_ms: Option<u64>,
}

/// One VS Code agent session and the canvas group it belongs in.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct SyncCandidate {
    pub session: VsSession,
    /// Stable key stored on the canvas group (`vscode-group:…` or `vscode-ws:…`).
    pub group_key: String,
    pub group_name: String,
    /// Copilot session of the main chat when this is a sub-session (peer chat).
    pub parent: Option<String>,
    /// Known to have been created in VS Code (not merely listed by it).
    pub from_vscode: bool,
}
