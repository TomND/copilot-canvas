//! Agent CLIs the canvas hosts: GitHub Copilot CLI, OpenCode and Claude Code.
//!
//! A [`Provider`] knows how to start, resume, fork and run one agent CLI, where
//! its session history lives and how to read status from it, and how to list
//! and change its configuration. Everything agent-specific goes through here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use canvas_common::protocol::{AgentLaunch, SessionSummary};
use serde::{Deserialize, Serialize};

pub mod claude;
pub mod claude_history;
pub mod config;
pub mod copilot;
pub mod opencode;
pub mod opencode_history;
pub mod opencode_server;
#[cfg(test)]
mod probes;

/// Which agent CLI a session runs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Copilot,
    OpenCode,
    Claude,
}

pub const ALL: [AgentKind; 3] = [AgentKind::Copilot, AgentKind::OpenCode, AgentKind::Claude];

impl AgentKind {
    /// The protocol / settings name.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Copilot => "copilot",
            AgentKind::OpenCode => "opencode",
            AgentKind::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Copilot => "GitHub Copilot",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Claude => "Claude Code",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "copilot" | "github copilot" | "gh-copilot" => Some(AgentKind::Copilot),
            "opencode" | "open-code" => Some(AgentKind::OpenCode),
            "claude" | "claude code" | "claude-code" => Some(AgentKind::Claude),
            _ => None,
        }
    }

    pub fn provider(self) -> &'static dyn Provider {
        match self {
            AgentKind::Copilot => &copilot::Copilot,
            AgentKind::OpenCode => &opencode::OpenCode,
            AgentKind::Claude => &claude::Claude,
        }
    }
}

/// The agent new sessions use when nothing else says: `COPILOT_CANVAS_AGENT`,
/// else the first installed of Copilot, OpenCode, Claude Code (Copilot when
/// none is).
pub fn default_agent() -> AgentKind {
    if let Some(kind) = std::env::var("COPILOT_CANVAS_AGENT").ok().and_then(|v| AgentKind::parse(&v)) {
        return kind;
    }
    ALL.into_iter().find(|kind| kind.provider().program().is_ok()).unwrap_or(AgentKind::Copilot)
}

/// The machine's agent for new sessions: `COPILOT_CANVAS_AGENT`, else
/// settings.json `agent` in `home`, else the first installed agent.
pub fn machine_default(home: &Path) -> AgentKind {
    if let Some(kind) = std::env::var("COPILOT_CANVAS_AGENT").ok().and_then(|v| AgentKind::parse(&v)) {
        return kind;
    }
    canvas_common::settings::Settings::load(home).agent.as_deref().and_then(AgentKind::parse).unwrap_or_else(default_agent)
}

/// The agent a canvas box runs (`None`: Copilot, as on canvases from before
/// agents were selectable).
pub fn agent_of(node: &canvas_model::doc::Node) -> AgentKind {
    node.agent.as_deref().and_then(AgentKind::parse).unwrap_or(AgentKind::Copilot)
}

/// The agent's own session id of a box: `agent_session`, else the box id.
pub fn native_id(node: &canvas_model::doc::Node) -> &str {
    node.agent_session.as_deref().filter(|s| !s.is_empty()).unwrap_or(&node.id)
}

/// The agent a session summary comes from.
pub fn agent_of_summary(summary: &SessionSummary) -> AgentKind {
    match summary.client.as_str() {
        "opencode" => AgentKind::OpenCode,
        "claude-code" => AgentKind::Claude,
        _ if opencode::is_session_id(&summary.id) => AgentKind::OpenCode,
        _ => AgentKind::Copilot,
    }
}

/// What a provider supports (the UI hides what it cannot do).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Caps {
    /// The canvas can choose the session id before launch.
    pub preassigned_ids: bool,
    /// Forks a conversation itself (launch flag).
    pub native_fork: bool,
    /// Forks by copying the history on disk (`fork::fork_session`).
    pub copy_fork: bool,
    /// Per-session MCP server selection.
    pub mcp_select: bool,
    /// Reports turn start and end with OSC 9;4 progress.
    pub progress: bool,
    /// Lists of plugins / MCP servers / skills / agents / settings.
    pub config: bool,
    /// Scheduled automations and `/every` loops (Copilot's `data.db`).
    pub automations: bool,
    /// Reasoning effort / mode / context flags.
    pub effort: bool,
    pub modes: bool,
}

/// The command line for a launch.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// Environment; a key starting with '-' (empty value) removes that variable.
    pub env: Vec<(String, String)>,
}

/// Paths of the canvas integration the service provides to every session.
#[derive(Clone, Debug, Default)]
pub struct Integration {
    /// Canvas skills (`canvas-events`, `canvas-passive`).
    pub plugin_dir: Option<PathBuf>,
    /// Environment: `COPILOT_CANVAS_HOME`, `COPILOT_CANVAS_SESSION`, `PATH`.
    pub env: Vec<(String, String)>,
}

/// A group's folders, created on demand.
#[derive(Clone, Debug)]
pub struct GroupDirs {
    pub folder: PathBuf,
    pub ai_docs: PathBuf,
    pub outputs: PathBuf,
    /// Folder of `canvas-group.instructions.md`.
    pub instructions: PathBuf,
    pub instructions_file: PathBuf,
}

/// Creates the group's folders and its instructions file.
pub fn group_dirs(folder: &str, name: &str) -> Result<GroupDirs> {
    let group = canvas_model::doc::Group { name: name.to_owned(), folder: Some(folder.to_owned()), ..Default::default() };
    let instructions = canvas_model::doc::prepare_group_folders(&group)?.unwrap_or_else(|| PathBuf::from(folder).join(".canvas"));
    Ok(GroupDirs {
        folder: PathBuf::from(folder),
        ai_docs: group.ai_docs().unwrap_or_default(),
        outputs: group.outputs().unwrap_or_default(),
        instructions_file: instructions.join("canvas-group.instructions.md"),
        instructions,
    })
}

/// A source of session summaries (one per agent's history store).
pub trait History: Send {
    /// Refreshes metadata (everything every `full_every`, tracked sessions
    /// always) and live state of tracked sessions. Returns changed summaries
    /// and removed ids.
    fn poll(&mut self, tracked: &HashSet<String>, full_every: Duration) -> (Vec<SessionSummary>, Vec<String>);
    /// Pull-request and push activity found in tracked sessions:
    /// (session, event kind, data).
    fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)>;
    fn take_warnings(&mut self) -> Vec<String>;
}

/// One agent CLI.
pub trait Provider: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn caps(&self) -> Caps;
    /// The executable to start (environment override, then PATH).
    fn program(&self) -> Result<String>;
    /// Whether `id` looks like one of this agent's session ids.
    fn is_session_id(&self, id: &str) -> bool;
    /// The command for a launch. `id` is the terminal id; the session is
    /// `launch.native_id` when set, else `id`.
    fn command(&self, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command>;
    /// Arguments a launch must never carry (they would select another session).
    fn reserved_args(&self) -> &'static [&'static str];
    /// The session's history store.
    fn history(&self) -> Box<dyn History>;
    /// Open in another process (never start a second copy).
    fn is_live(&self, native_id: &str) -> bool;
    /// The session's history exists (it can be resumed or forked).
    fn has_history(&self, native_id: &str) -> bool;
    /// Default root of the history store (shown by `doctor`).
    fn history_root(&self) -> PathBuf;
    /// A terminal that ran this agent exited: drop what its launch left
    /// behind. `home` is the canvas data folder.
    fn terminal_exited(&self, _id: &str, _home: &Path) {}
}

/// `program` from `env` or the first of `names` on PATH.
pub fn find_program(env: &str, names: &[&str]) -> Result<String> {
    if let Some(custom) = std::env::var_os(env) {
        return Ok(custom.to_string_lossy().into_owned());
    }
    names
        .iter()
        .find_map(|name| crate::service::find_on_path(name))
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("{} was not found on PATH (set {env} to override)", names[0]))
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_and_boxes_name_their_agent() {
        let summary = |id: &str, client: &str| SessionSummary { id: id.into(), client: client.into(), ..Default::default() };
        assert_eq!(agent_of_summary(&summary("ses_e7fbbd911ffeOynBIm3IrWFsUl", "")), AgentKind::OpenCode);
        assert_eq!(agent_of_summary(&summary("x", "opencode")), AgentKind::OpenCode);
        assert_eq!(agent_of_summary(&summary("12345678-1234-1234-1234-123456789abc", "claude-code")), AgentKind::Claude);
        for client in ["github/cli", "vscode-agent-host", ""] {
            assert_eq!(agent_of_summary(&summary("12345678-1234-1234-1234-123456789abc", client)), AgentKind::Copilot, "{client}");
        }
        let mut node: canvas_model::doc::Node =
            serde_json::from_value(serde_json::json!({ "id": "n1", "kind": "session", "name": "n", "x": 0, "y": 0 })).unwrap();
        assert_eq!((agent_of(&node), native_id(&node)), (AgentKind::Copilot, "n1"));
        node.agent = Some("opencode".into());
        node.agent_session = Some("ses_abc".into());
        assert_eq!((agent_of(&node), native_id(&node)), (AgentKind::OpenCode, "ses_abc"));
        node.agent = Some("unknown".into());
        assert_eq!(agent_of(&node), AgentKind::Copilot);
    }
}
