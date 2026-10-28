//! VS Code → Copilot CLI migration items as the preview shows them (the
//! backend plans and applies them; see `canvas_agents::migrate`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Mcp,
    Instruction,
    PromptSkill,
    Agent,
    SkillFolder,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Mcp => "MCP server",
            ItemKind::Instruction => "Instructions",
            ItemKind::PromptSkill => "Prompt → skill",
            ItemKind::Agent => "Agent",
            ItemKind::SkillFolder => "Skill folder",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    New,
    AlreadyPresent,
    Conflict(String),
    Manual(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Action {
    Cli(Vec<String>),
    Write { path: PathBuf, content: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: ItemKind,
    pub name: String,
    pub products: String,
    pub source: String,
    pub target: String,
    pub status: Status,
    pub action: Option<Action>,
}
