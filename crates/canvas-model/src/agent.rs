//! Agent CLIs a canvas box can run (GitHub Copilot CLI, OpenCode, Claude
//! Code) and what each supports. The CLIs themselves are driven by the
//! backend (`canvas_agents`); front ends only need these facts.

use serde::{Deserialize, Serialize};

use crate::doc::Node;
use canvas_common::protocol::SessionSummary;

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

/// What an agent CLI supports (the UI hides what it cannot do).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Caps {
    /// The canvas can choose the session id before launch.
    pub preassigned_ids: bool,
    /// Forks a conversation itself (launch flag).
    pub native_fork: bool,
    /// Forks by copying the history on disk (the backend's `Fork` request).
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

    pub fn caps(self) -> Caps {
        match self {
            AgentKind::Copilot => Caps {
                preassigned_ids: true,
                native_fork: false,
                copy_fork: true,
                mcp_select: true,
                progress: true,
                config: true,
                automations: true,
                effort: true,
                modes: true,
            },
            AgentKind::OpenCode => Caps {
                preassigned_ids: false,
                native_fork: true,
                copy_fork: false,
                mcp_select: true,
                progress: false,
                config: true,
                automations: false,
                effort: false,
                modes: false,
            },
            AgentKind::Claude => Caps {
                preassigned_ids: true,
                native_fork: true,
                copy_fork: false,
                mcp_select: true,
                progress: true,
                config: true,
                automations: false,
                effort: true,
                modes: false,
            },
        }
    }

    /// Whether `id` looks like one of this agent's session ids.
    pub fn is_session_id(self, id: &str) -> bool {
        match self {
            AgentKind::OpenCode => is_opencode_session_id(id),
            AgentKind::Copilot | AgentKind::Claude => is_session_id(id),
        }
    }
}

/// A Copilot or Claude Code session id (a hyphenated UUID).
pub fn is_session_id(text: &str) -> bool {
    uuid::Uuid::try_parse(text).is_ok() && text.len() == 36
}

/// An OpenCode session id (`ses_` and letters or digits).
pub fn is_opencode_session_id(id: &str) -> bool {
    id.len() > 4 && id.len() <= 64 && id.starts_with("ses_") && id[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// The agent a canvas box runs (`None`: Copilot, as on canvases from before
/// agents were selectable).
pub fn agent_of(node: &Node) -> AgentKind {
    node.agent.as_deref().and_then(AgentKind::parse).unwrap_or(AgentKind::Copilot)
}

/// The agent's own session id of a box: `agent_session`, else the box id.
pub fn native_id(node: &Node) -> &str {
    node.agent_session.as_deref().filter(|s| !s.is_empty()).unwrap_or(&node.id)
}

/// The agent a session summary comes from.
pub fn agent_of_summary(summary: &SessionSummary) -> AgentKind {
    match summary.client.as_str() {
        "opencode" => AgentKind::OpenCode,
        "claude-code" => AgentKind::Claude,
        _ if is_opencode_session_id(&summary.id) => AgentKind::OpenCode,
        _ => AgentKind::Copilot,
    }
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
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": "n1", "kind": "session", "name": "n", "x": 0, "y": 0 })).unwrap();
        assert_eq!((agent_of(&node), native_id(&node)), (AgentKind::Copilot, "n1"));
        node.agent = Some("opencode".into());
        node.agent_session = Some("ses_abc".into());
        assert_eq!((agent_of(&node), native_id(&node)), (AgentKind::OpenCode, "ses_abc"));
        node.agent = Some("unknown".into());
        assert_eq!(agent_of(&node), AgentKind::Copilot);
    }

    #[test]
    fn session_ids_and_caps_per_agent() {
        assert!(AgentKind::Copilot.is_session_id("12345678-1234-1234-1234-123456789abc"));
        assert!(!AgentKind::Copilot.is_session_id("ses_abc"));
        assert!(AgentKind::OpenCode.is_session_id("ses_abc") && !AgentKind::OpenCode.is_session_id("ses_a-b"));
        assert!(AgentKind::Copilot.caps().copy_fork && !AgentKind::Copilot.caps().native_fork);
        assert!(AgentKind::Claude.caps().native_fork && AgentKind::OpenCode.caps().native_fork);
        assert_eq!(serde_json::to_string(&AgentKind::OpenCode).unwrap(), "\"opencode\"");
    }
}
