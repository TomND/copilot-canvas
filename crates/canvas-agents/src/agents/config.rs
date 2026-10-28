//! Configuration views (plugins, MCP servers, skills, agents, settings) for
//! every agent, in the shape of Copilot's [`crate::copilot_cfg::Snapshot`].
//! CLIs run hidden with timeouts; secrets (MCP env and headers, provider
//! keys) are never read into the snapshot.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::AgentKind;
use crate::copilot_cfg::{self, Agent, Kind, McpServer, Plugin, Skill, Snapshot, json_payload};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TIMEOUT: Duration = Duration::from_secs(60);

/// Which configuration views an agent has: (plugins, MCP, skills, agents, settings).
pub fn caps_for_views(kind: AgentKind) -> (bool, bool, bool, bool, bool) {
    match kind {
        AgentKind::Copilot => (true, true, true, true, true),
        AgentKind::OpenCode => (true, true, true, true, true),
        AgentKind::Claude => (true, true, true, true, true),
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

/// Runs an agent CLI without a window and returns stdout.
pub fn run(program: &str, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Result<String> {
    let temp = std::env::temp_dir().join(format!("copilot-canvas-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)?;
    let out_path = temp.join("out.txt");
    let err_path = temp.join("err.txt");
    let mut command = Command::new(program);
    command.args(args).current_dir(&temp).stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    // Files instead of pipes: no risk of a full pipe blocking the child.
    let mut child = command
        .stdout(std::fs::File::create(&out_path)?)
        .stderr(std::fs::File::create(&err_path)?)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = crate::win::kill_tree(child.id());
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&temp);
            bail!("{} {} timed out", Path::new(program).file_stem().unwrap_or_default().to_string_lossy(), args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&temp);
    if !status.success() {
        let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        bail!("{}", message.lines().last().unwrap_or("the command failed"));
    }
    Ok(stdout)
}

fn opencode(args: &[&str]) -> Result<String> {
    run(&super::opencode::find_cli()?, args, &[("OPENCODE_DISABLE_AUTOUPDATE", "1")], TIMEOUT)
}

fn claude(args: &[&str]) -> Result<String> {
    run(&super::claude::find_cli()?, args, &[("DISABLE_AUTOUPDATER", "1")], TIMEOUT)
}

fn json(output: &str, what: &str) -> Result<Value> {
    let text = json_payload(output).with_context(|| format!("no JSON from {what}"))?;
    serde_json::from_str(text).with_context(|| format!("unexpected output from {what}"))
}

/// Everything the views show for `kind`.
pub fn snapshot(kind: AgentKind) -> Result<Snapshot> {
    match kind {
        AgentKind::Copilot => Ok(copilot_cfg::snapshot()),
        AgentKind::OpenCode => Ok(opencode_snapshot()),
        AgentKind::Claude => Ok(claude_snapshot(&super::claude_history::claude_dir(), &super::claude_history::user_config())),
    }
}

fn unsupported(kind: AgentKind) -> anyhow::Error {
    anyhow::anyhow!("not supported for {}", kind.label())
}

/// Turns a plugin, skill or MCP server on or off.
pub fn set_enabled(kind: AgentKind, what: Kind, name: &str, enabled: bool) -> Result<String> {
    check_name(name)?;
    match (kind, what) {
        (AgentKind::Copilot, _) => copilot_cfg::set_enabled(what, name, enabled),
        (AgentKind::Claude, Kind::Plugin) => claude(&["plugin", if enabled { "enable" } else { "disable" }, name]),
        _ => Err(unsupported(kind)),
    }
}

/// Removes (uninstalls) a plugin, skill or MCP server.
pub fn remove(kind: AgentKind, what: Kind, name: &str) -> Result<String> {
    check_name(name)?;
    match (kind, what) {
        (AgentKind::Copilot, _) => copilot_cfg::remove(what, name),
        (AgentKind::Claude, Kind::Plugin) => claude(&["plugin", "uninstall", name]),
        (AgentKind::Claude, Kind::Mcp) => claude(&["mcp", "remove", "--scope", "user", name]),
        _ => Err(unsupported(kind)),
    }
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '/' | ':')),
        "invalid name"
    );
    Ok(())
}

fn flatten(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let full = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                flatten(&full, child, out);
            }
        }
        Value::String(s) => out.push((prefix.to_owned(), s.clone())),
        other => out.push((prefix.to_owned(), other.to_string())),
    }
}

/// Keys whose values may be secrets.
fn secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["key", "token", "secret", "password", "header", "auth", "credential", "env."].iter().any(|w| lower.contains(w)) || lower == "env"
}

/// Settings as (key, value) pairs, secrets hidden.
fn settings(value: &Value, skip: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(map) = value.as_object() {
        for (key, child) in map.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
            flatten(key, child, &mut out);
        }
    }
    for (key, value) in &mut out {
        if secret(key) {
            *value = "(hidden)".to_owned();
        }
    }
    out
}

fn mcp_entry(name: &str, entry: &Value, source: &str) -> McpServer {
    let transport = entry["type"].as_str().unwrap_or(if entry.get("url").is_some() { "http" } else { "stdio" }).to_owned();
    let target = match entry["url"].as_str() {
        Some(url) => url.to_owned(),
        None => {
            // Claude: command + args; OpenCode: command is an array.
            let mut words: Vec<&str> = match &entry["command"] {
                Value::String(s) => vec![s.as_str()],
                Value::Array(items) => items.iter().filter_map(|v| v.as_str()).collect(),
                _ => Vec::new(),
            };
            words.extend(entry["args"].as_array().into_iter().flatten().filter_map(|v| v.as_str()));
            words.join(" ")
        }
    };
    let enabled = entry["enabled"].as_bool().unwrap_or(true) && entry["disabled"].as_bool() != Some(true);
    McpServer { name: name.to_owned(), source: source.to_owned(), transport, target, enabled }
}

fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut description = None;
    if let Some(rest) = text.strip_prefix("---") {
        for line in rest.lines().skip(1).take_while(|l| l.trim() != "---") {
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(v.trim().trim_matches(['"', '\'']).to_owned());
            } else if let Some(v) = line.strip_prefix("description:") {
                description = Some(v.trim().trim_matches(['"', '\'']).to_owned());
            }
        }
    }
    (name, description)
}

/// `SKILL.md` files below `root` (a few levels deep).
fn scan_skills(root: &Path, source: &str, depth: usize, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let file = path.join("SKILL.md");
            if file.is_file() {
                let (name, description) = frontmatter(&std::fs::read_to_string(&file).unwrap_or_default());
                out.push(Skill {
                    name: name.unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned()),
                    description: description.unwrap_or_default(),
                    source: source.to_owned(),
                    path: file.display().to_string(),
                    enabled: true,
                });
            } else if depth > 0 {
                scan_skills(&path, source, depth - 1, out);
            }
        }
    }
}

fn sort(snapshot: &mut Snapshot) {
    snapshot.skills.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    snapshot.mcp.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    snapshot.agents.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

fn opencode_snapshot() -> Snapshot {
    let mut snapshot = Snapshot::default();
    std::thread::scope(|scope| {
        let config = scope.spawn(|| opencode(&["debug", "config"]).and_then(|o| json(&o, "opencode debug config")));
        let skills = scope.spawn(|| opencode(&["debug", "skill"]).and_then(|o| json(&o, "opencode debug skill")));
        let agents = scope.spawn(|| opencode(&["agent", "list"]));
        match config.join().expect("thread") {
            Ok(config) => opencode_config(&config, &mut snapshot),
            Err(e) => snapshot.errors.push(format!("config: {e:#}")),
        }
        match skills.join().expect("thread") {
            Ok(skills) => snapshot.skills = opencode_skills(&skills, &super::home()),
            Err(e) => snapshot.errors.push(format!("skills: {e:#}")),
        }
        match agents.join().expect("thread") {
            Ok(text) => {
                let configured = std::mem::take(&mut snapshot.agents);
                snapshot.agents = opencode_agents(&text, &configured);
            }
            Err(e) => snapshot.errors.push(format!("agents: {e:#}")),
        }
    });
    sort(&mut snapshot);
    snapshot
}

/// MCP servers, plugins, custom agents and settings from `opencode debug config`.
fn opencode_config(config: &Value, snapshot: &mut Snapshot) {
    if let Some(servers) = config["mcp"].as_object() {
        snapshot.mcp = servers.iter().map(|(name, entry)| mcp_entry(name, entry, "config")).collect();
    }
    for plugin in config["plugin"].as_array().into_iter().flatten().filter_map(|p| p.as_str()) {
        let (name, version) = match plugin.rsplit_once('@').filter(|(n, _)| !n.is_empty()) {
            Some((name, version)) => (name.to_owned(), version.to_owned()),
            None => (plugin.to_owned(), String::new()),
        };
        snapshot.plugins.push(Plugin { name, marketplace: "npm".into(), version, enabled: true, source: plugin.to_owned() });
    }
    if let Some(agents) = config["agent"].as_object() {
        for (name, agent) in agents {
            snapshot.agents.push(Agent {
                name: name.clone(),
                description: agent["description"].as_str().unwrap_or_default().to_owned(),
                source: "config".into(),
                path: String::new(),
            });
        }
    }
    snapshot.settings = settings(config, &["$schema", "mcp", "agent", "mode", "plugin", "command", "provider"]);
}

fn opencode_skills(value: &Value, home: &Path) -> Vec<Skill> {
    let claude = home.join(".claude").display().to_string().to_lowercase();
    let personal = home.join(".config").join("opencode").display().to_string().to_lowercase();
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let location = s["location"].as_str().unwrap_or_default();
            let lower = location.to_lowercase();
            let source = if location.starts_with('<') {
                "built-in"
            } else if lower.starts_with(&claude) {
                "claude"
            } else if lower.starts_with(&personal) {
                "personal"
            } else {
                "project"
            };
            Some(Skill {
                name: s["name"].as_str()?.to_owned(),
                description: s["description"].as_str().unwrap_or_default().to_owned(),
                source: source.to_owned(),
                path: location.to_owned(),
                enabled: true,
            })
        })
        .collect()
}

/// Agents from `opencode agent list` ("name (mode)" lines), with configured descriptions.
fn opencode_agents(text: &str, configured: &[Agent]) -> Vec<Agent> {
    const HIDDEN: [&str; 3] = ["compaction", "title", "summary"];
    let mut agents: Vec<Agent> = text
        .lines()
        .filter(|l| !l.starts_with(char::is_whitespace))
        .filter_map(|l| {
            let (name, mode) = l.trim().rsplit_once(" (")?;
            let mode = mode.strip_suffix(')')?;
            (!name.contains(char::is_whitespace) && !HIDDEN.contains(&name)).then(|| (name.to_owned(), mode.to_owned()))
        })
        .map(|(name, mode)| {
            let custom = configured.iter().find(|a| a.name == name);
            Agent {
                description: custom.map(|a| a.description.clone()).unwrap_or_default(),
                source: if custom.is_some() { "config".into() } else { format!("built-in ({mode})") },
                path: String::new(),
                name,
            }
        })
        .collect();
    for agent in configured {
        if !agents.iter().any(|a| a.name == agent.name) {
            agents.push(agent.clone());
        }
    }
    agents
}

fn claude_snapshot(dir: &Path, user_config: &Path) -> Snapshot {
    let mut snapshot = Snapshot::default();
    match claude(&["plugin", "list", "--json"]).and_then(|o| json(&o, "claude plugin list")) {
        Ok(value) => snapshot.plugins = claude_plugins(&value),
        Err(e) => snapshot.errors.push(format!("plugins: {e:#}")),
    }
    claude_local(dir, user_config, &mut snapshot);
    sort(&mut snapshot);
    snapshot
}

fn claude_plugins(value: &Value) -> Vec<Plugin> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = p["id"].as_str().or(p["name"].as_str())?;
            let (name, marketplace) = id.split_once('@').map(|(n, m)| (n.to_owned(), m.to_owned())).unwrap_or((id.to_owned(), String::new()));
            Some(Plugin {
                name,
                marketplace: p["marketplace"].as_str().map(str::to_owned).unwrap_or(marketplace),
                version: p["version"].as_str().unwrap_or_default().to_owned(),
                enabled: p["enabled"].as_bool().unwrap_or(true),
                source: p["scope"].as_str().or(p["installPath"].as_str()).unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// MCP servers, skills, agents and settings Claude Code keeps in files.
fn claude_local(dir: &Path, user_config: &Path, snapshot: &mut Snapshot) {
    match std::fs::read_to_string(user_config).map(|t| serde_json::from_str::<Value>(&t)) {
        Ok(Ok(config)) => {
            for (name, entry) in config["mcpServers"].as_object().into_iter().flatten() {
                snapshot.mcp.push(mcp_entry(name, entry, "user"));
            }
            for (project, settings) in config["projects"].as_object().into_iter().flatten() {
                for (name, entry) in settings["mcpServers"].as_object().into_iter().flatten() {
                    snapshot.mcp.push(mcp_entry(name, entry, &format!("local: {project}")));
                }
            }
        }
        Ok(Err(e)) => snapshot.errors.push(format!("MCP: {} is not valid JSON ({e})", user_config.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => snapshot.errors.push(format!("MCP: {e}")),
    }
    scan_skills(&dir.join("skills"), "personal", 3, &mut snapshot.skills);
    snapshot.agents = copilot_cfg::scan_agents(dir);
    for agent in &mut snapshot.agents {
        if agent.source == "personal" {
            agent.source = "user".into();
        }
    }
    let settings_path: PathBuf = dir.join("settings.json");
    match std::fs::read_to_string(&settings_path).map(|t| serde_json::from_str::<Value>(&t)) {
        Ok(Ok(value)) => snapshot.settings = settings(&value, &[]),
        Ok(Err(e)) => snapshot.errors.push(format!("settings: {} is not valid JSON ({e})", settings_path.display())),
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opencode_config_lists_servers_plugins_agents_and_safe_settings() {
        let config = json!({
            "$schema": "x",
            "model": "opencode/big-pickle",
            "mcp": { "unity": { "type": "remote", "url": "http://127.0.0.1:8080/mcp", "enabled": false, "headers": { "Authorization": "secret" } },
                     "local": { "type": "local", "command": ["npx", "-y", "srv"], "environment": { "TOKEN": "secret" } } },
            "plugin": ["opencode-foo@1.2.0", "@scope/bar"],
            "agent": { "review": { "description": "Reviews code", "mode": "subagent" } },
            "provider": { "x": { "options": { "apiKey": "secret" } } },
            "username": "me",
            "experimental": { "token_budget": 5 }
        });
        let mut snapshot = Snapshot::default();
        opencode_config(&config, &mut snapshot);
        let unity = snapshot.mcp.iter().find(|s| s.name == "unity").unwrap();
        assert_eq!((unity.transport.as_str(), unity.target.as_str(), unity.enabled), ("remote", "http://127.0.0.1:8080/mcp", false));
        assert_eq!(snapshot.mcp.iter().find(|s| s.name == "local").unwrap().target, "npx -y srv");
        assert_eq!(snapshot.plugins.iter().map(|p| (p.name.as_str(), p.version.as_str())).collect::<Vec<_>>(), [("opencode-foo", "1.2.0"), ("@scope/bar", "")]);
        assert!(snapshot.settings.contains(&("model".into(), "opencode/big-pickle".into())));
        assert!(snapshot.settings.contains(&("experimental.token_budget".into(), "(hidden)".into())));
        let all = format!("{snapshot:?}");
        assert!(!all.contains("secret"), "no secrets in the snapshot");
        let agents = opencode_agents("build (primary)\n  [\n  {\"permission\": \"*\"}\n  ]\nreview (subagent)\ncompaction (primary)\n", &snapshot.agents);
        assert_eq!(agents.iter().map(|a| (a.name.as_str(), a.source.as_str())).collect::<Vec<_>>(), [("build", "built-in (primary)"), ("review", "config")]);
        assert_eq!(agents[1].description, "Reviews code");
    }

    #[test]
    fn opencode_skills_are_grouped_by_where_they_live() {
        let home = PathBuf::from(r"C:\Users\me");
        let value = json!([
            { "name": "customize-opencode", "description": "d", "location": "<built-in>" },
            { "name": "pdf", "location": r"C:\Users\me\.claude\skills\pdf\SKILL.md" },
            { "name": "mine", "location": r"C:\Users\me\.config\opencode\skills\mine\SKILL.md" },
            { "name": "repo", "location": r"C:\repos\x\.opencode\skills\repo\SKILL.md" }
        ]);
        let sources: Vec<_> = opencode_skills(&value, &home).into_iter().map(|s| s.source).collect();
        assert_eq!(sources, ["built-in", "claude", "personal", "project"]);
    }

    #[test]
    fn claude_files_give_servers_skills_agents_and_settings_without_secrets() {
        let dir = std::env::temp_dir().join(format!("cc-cfg-{}", uuid::Uuid::new_v4()));
        let claude = dir.join(".claude");
        std::fs::create_dir_all(claude.join("skills").join("synced").join("x").join("deep")).unwrap();
        std::fs::create_dir_all(claude.join("agents")).unwrap();
        std::fs::write(claude.join("skills").join("synced").join("x").join("deep").join("SKILL.md"), "---\nname: deep\ndescription: Nested\n---\n").unwrap();
        std::fs::write(claude.join("agents").join("reviewer.md"), "---\nname: reviewer\ndescription: Reviews\n---\n").unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"model":"opus","tui":"fullscreen","env":{"API_KEY":"secret"},"permissions":{"allow":["Bash(git *)"]}}"#).unwrap();
        let config = dir.join(".claude.json");
        std::fs::write(
            &config,
            r#"{"oauthAccount":{"emailAddress":"secret"},"mcpServers":{"blender":{"type":"stdio","command":"uvx","args":["blender-mcp"],"env":{"K":"secret"}}},"projects":{"C:/repos/x":{"mcpServers":{"UnityMCP":{"type":"http","url":"http://localhost:8080/mcp","headers":{"A":"secret"}}}}}}"#,
        )
        .unwrap();
        let mut snapshot = Snapshot::default();
        claude_local(&claude, &config, &mut snapshot);
        assert_eq!(snapshot.mcp.iter().map(|s| (s.name.as_str(), s.source.as_str(), s.target.as_str())).collect::<Vec<_>>(), [
            ("blender", "user", "uvx blender-mcp"),
            ("UnityMCP", "local: C:/repos/x", "http://localhost:8080/mcp")
        ]);
        assert_eq!(snapshot.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["deep"]);
        assert_eq!((snapshot.agents[0].name.as_str(), snapshot.agents[0].source.as_str()), ("reviewer", "user"));
        assert!(snapshot.settings.contains(&("tui".into(), "fullscreen".into())));
        assert!(snapshot.settings.contains(&("env.API_KEY".into(), "(hidden)".into())));
        assert!(!format!("{snapshot:?}").contains("secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_plugin_listing_and_unsupported_changes() {
        let plugins = claude_plugins(&json!([{ "id": "review@official", "version": "1.0.0", "enabled": false, "scope": "user" }, { "name": "local" }]));
        assert_eq!((plugins[0].name.as_str(), plugins[0].marketplace.as_str(), plugins[0].enabled), ("review", "official", false));
        assert_eq!(plugins[1].name, "local");
        let error = set_enabled(AgentKind::OpenCode, Kind::Mcp, "x", false).unwrap_err().to_string();
        assert_eq!(error, "not supported for OpenCode");
        assert!(remove(AgentKind::Claude, Kind::Skill, "x").is_err());
        assert!(remove(AgentKind::Claude, Kind::Mcp, "--scope").is_err(), "names are never options");
        assert_eq!(caps_for_changes(AgentKind::Claude, Kind::Plugin), (true, true));
    }
}
