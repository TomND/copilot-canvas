//! Claude Code (provider implemented in a later step).

use std::path::PathBuf;

use anyhow::Result;
use canvas_common::protocol::{AgentLaunch, SessionSummary};

use super::{AgentKind, Caps, Command, History, Integration, Provider};

pub struct Claude;

struct NoHistory;

impl History for NoHistory {
    fn poll(&mut self, _: &std::collections::HashSet<String>, _: std::time::Duration) -> (Vec<SessionSummary>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
    fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)> {
        Vec::new()
    }
    fn take_warnings(&mut self) -> Vec<String> {
        Vec::new()
    }
}

impl Provider for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn program(&self) -> Result<String> {
        super::find_program("COPILOT_CANVAS_CLAUDE", &["claude.exe"])
    }
    fn is_session_id(&self, id: &str) -> bool {
        crate::index::is_session_id(id)
    }
    fn reserved_args(&self) -> &'static [&'static str] {
        &[]
    }
    fn command(&self, _id: &str, _launch: &AgentLaunch, _integration: &Integration) -> Result<Command> {
        anyhow::bail!("Claude Code sessions are not supported yet")
    }
    fn history(&self) -> Box<dyn History> {
        Box::new(NoHistory)
    }
    fn is_live(&self, _native_id: &str) -> bool {
        false
    }
    fn has_history(&self, _native_id: &str) -> bool {
        false
    }
    fn history_root(&self) -> PathBuf {
        super::home()
    }
}