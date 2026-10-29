//! Agent configuration as the views show it (plugins, MCP servers, skills,
//! custom agents, instructions, settings), in the shape of Copilot's lists.
//! The backend reads and changes it through each agent's CLI; secrets never
//! reach a [`Snapshot`].

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Plugin {
    pub name: String,
    #[serde(default)]
    pub marketplace: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source: String,
    /// Its folder under Copilot's `installed-plugins`, when one exists (filled
    /// in by the backend; empty: nothing to open).
    #[serde(default)]
    pub local_folder: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Instruction {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub location: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "sourcePath", default)]
    pub source_path: String,
    #[serde(rename = "defaultDisabled", default)]
    pub default_disabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct McpServer {
    pub name: String,
    pub source: String,
    pub transport: String,
    /// Command line or URL; never environment values or headers.
    pub target: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub source: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub plugins: Vec<Plugin>,
    pub skills: Vec<Skill>,
    pub mcp: Vec<McpServer>,
    pub instructions: Vec<Instruction>,
    pub agents: Vec<Agent>,
    /// User settings, secrets redacted.
    pub settings: Vec<(String, String)>,
    pub errors: Vec<String>,
    /// Model ids the agent's CLI lists (`opencode models`, the choices in
    /// `copilot --help`, the aliases in `claude --help`); may be empty.
    #[serde(default)]
    pub models: Vec<String>,
}

/// What a configuration change applies to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plugin,
    Skill,
    Mcp,
}

impl Kind {
    /// The Copilot CLI command group.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Plugin => "plugin",
            Kind::Skill => "skill",
            Kind::Mcp => "mcp",
        }
    }
}

/// Which configuration views an agent has: (plugins, MCP, skills, agents, settings).
pub fn caps_for_views(kind: AgentKind) -> (bool, bool, bool, bool, bool) {
    match kind {
        AgentKind::Copilot | AgentKind::OpenCode | AgentKind::Claude => (true, true, true, true, true),
    }
}

/// Which changes an agent's CLI supports for a view: (enable/disable, remove).
pub fn caps_for_changes(kind: AgentKind, what: Kind) -> (bool, bool) {
    match (kind, what) {
        (AgentKind::Copilot, _) => (true, true),
        (AgentKind::Claude, Kind::Plugin) => (true, true),
        (AgentKind::Claude, Kind::Mcp) => (false, true),
        _ => (false, false),
    }
}

/// `name -- command args` (stdio) or `name https://url` (http), like `copilot mcp add`.
pub fn parse_mcp_add(line: &str) -> Result<Vec<String>> {
    let words = split_words(line)?;
    let Some((name, rest)) = words.split_first() else { bail!("enter: name -- command [args]   or   name https://url") };
    anyhow::ensure!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'), "invalid server name");
    let mut args = vec!["mcp".to_owned(), "add".to_owned()];
    match rest {
        [url] if url.starts_with("http://") || url.starts_with("https://") => {
            args.extend(["--transport".into(), "http".into(), name.clone(), url.clone()]);
        }
        [dashes, command @ ..] if dashes == "--" && !command.is_empty() => {
            args.push(name.clone());
            args.push("--".into());
            args.extend(command.iter().cloned());
        }
        _ => bail!("enter: name -- command [args]   or   name https://url"),
    }
    Ok(args)
}

/// Splits on whitespace, honoring double quotes.
pub fn split_words(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has {
                    words.push(std::mem::take(&mut current));
                    has = false;
                }
            }
            c => {
                current.push(c);
                has = true;
            }
        }
    }
    anyhow::ensure!(!quoted, "unbalanced quotes");
    if has {
        words.push(current);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_add_lines_follow_cli_syntax() {
        assert_eq!(parse_mcp_add("ctx -- npx -y \"@scope/server name\"").unwrap(), ["mcp", "add", "ctx", "--", "npx", "-y", "@scope/server name"]);
        assert_eq!(parse_mcp_add("web https://mcp.example/v1").unwrap(), ["mcp", "add", "--transport", "http", "web", "https://mcp.example/v1"]);
        assert!(parse_mcp_add("bad name -- x").is_err());
        assert!(parse_mcp_add("only").is_err());
        assert!(split_words("a \"b").is_err());
    }

    #[test]
    fn snapshots_round_trip_with_copilot_field_names() {
        let instruction: Instruction = serde_json::from_str(r#"{"label":"x","type":"file","sourcePath":"C:\\a.md","defaultDisabled":true}"#).unwrap();
        assert_eq!((instruction.kind.as_str(), instruction.default_disabled), ("file", true));
        let snapshot = Snapshot { instructions: vec![instruction], settings: vec![("a".into(), "b".into())], ..Default::default() };
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(back, snapshot);
        assert_eq!(caps_for_changes(AgentKind::Claude, Kind::Mcp), (false, true));
    }
}
