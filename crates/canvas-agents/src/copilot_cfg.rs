//! Copilot CLI configuration: listing via `copilot ... --json` and changes via
//! the CLI itself, so Copilot validates everything it persists.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn copilot_home() -> PathBuf {
    std::env::var_os("COPILOT_HOME").map(PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".copilot"))
}

/// Older terminal services launched Copilot with `--mouse=off`, which Copilot
/// saves as `"mouse": false`; then the mouse wheel can only page. Turns it back
/// on (through the CLI) if it is off. Returns true when it changed something.
pub fn restore_mouse() -> Result<bool> {
    let text = std::fs::read_to_string(copilot_home().join("settings.json")).unwrap_or_default();
    let off = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("mouse").and_then(|m| m.as_bool())) == Some(false);
    if !off {
        return Ok(false);
    }
    run_cli(&["config", "mouse", "true"], Duration::from_secs(30))?;
    Ok(true)
}

pub fn copilot_exe() -> Option<PathBuf> {
    std::env::var_os("COPILOT_CANVAS_COPILOT").map(PathBuf::from).or_else(|| crate::service::find_on_path("copilot.exe"))
}

/// Runs a non-interactive Copilot CLI command and returns stdout.
pub fn run_cli(args: &[&str], timeout: Duration) -> Result<String> {
    let exe = copilot_exe().context("copilot.exe was not found on PATH")?;
    let temp = std::env::temp_dir().join(format!("copilot-canvas-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)?;
    let out_path = temp.join("out.txt");
    let err_path = temp.join("err.txt");
    // Files instead of pipes: no risk of a full pipe blocking the child.
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(&temp)
        .env("COPILOT_AUTO_UPDATE", "false")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path)?)
        .stderr(std::fs::File::create(&err_path)?)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("cannot run copilot")?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = crate::win::kill_tree(child.id());
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&temp);
            bail!("copilot {} timed out", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&temp);
    if !status.success() {
        let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        bail!("{}", message.lines().last().unwrap_or("copilot failed"));
    }
    Ok(stdout)
}

/// Extracts the JSON document from output that may begin with warning lines.
pub fn json_payload(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let open = text.as_bytes()[start];
    let close = if open == b'{' { '}' } else { ']' };
    let end = text.rfind(close)?;
    (end >= start).then(|| &text[start..=end])
}

fn list<T: for<'de> Deserialize<'de>>(args: &[&str]) -> Result<T> {
    let output = run_cli(args, Duration::from_secs(60))?;
    let json = json_payload(&output).with_context(|| format!("no JSON from copilot {}", args.join(" ")))?;
    serde_json::from_str(json).with_context(|| format!("unexpected output from copilot {}", args.join(" ")))
}

pub use canvas_model::config::{Agent, Instruction, Kind, McpServer, Plugin, Skill, Snapshot, parse_mcp_add, split_words};

fn mcp_from_value(value: &serde_json::Value) -> Vec<McpServer> {
    let mut servers = Vec::new();
    let mut add = |name: &str, entry: &serde_json::Value, source: &str| {
        let transport = entry.get("type").and_then(|t| t.as_str()).unwrap_or("stdio").to_owned();
        let target = match entry.get("url").and_then(|u| u.as_str()) {
            Some(url) => url.to_owned(),
            None => {
                let command = entry.get("command").and_then(|c| c.as_str()).unwrap_or_default();
                let args: Vec<&str> = entry.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
                std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
            }
        };
        let enabled = !entry.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false) && entry.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
        let source = match entry.get("sourcePlugin").and_then(|s| s.as_str()) {
            Some(plugin) => format!("plugin: {plugin}"),
            None => entry.get("source").and_then(|s| s.as_str()).unwrap_or(source).to_owned(),
        };
        servers.push(McpServer { name: name.to_owned(), source, transport, target, enabled });
    };
    match value {
        serde_json::Value::Object(map) if map.contains_key("mcpServers") => {
            if let Some(servers) = map["mcpServers"].as_object() {
                for (name, entry) in servers {
                    add(name, entry, "user");
                }
            }
        }
        serde_json::Value::Object(map) => {
            // Grouped by source: { "User": { name: entry }, "Plugin": [...] ... }
            for (source, group) in map {
                match group {
                    serde_json::Value::Object(entries) => entries.iter().for_each(|(name, entry)| add(name, entry, source)),
                    serde_json::Value::Array(entries) => entries.iter().for_each(|entry| {
                        if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                            add(name, entry, source);
                        }
                    }),
                    _ => {}
                }
            }
        }
        serde_json::Value::Array(entries) => entries.iter().for_each(|entry| {
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                add(name, entry, "user");
            }
        }),
        _ => {}
    }
    servers.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    servers
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

pub fn scan_agents(home: &Path) -> Vec<Agent> {
    let mut agents = Vec::new();
    let mut visit = |dir: PathBuf, source: &str| {
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let file = entry.file_name().to_string_lossy().into_owned();
            if !(file.ends_with(".agent.md") || file.ends_with(".md") && dir.ends_with("agents")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let (name, description) = frontmatter(&text);
            agents.push(Agent {
                name: name.unwrap_or_else(|| file.trim_end_matches(".md").trim_end_matches(".agent").to_owned()),
                description: description.unwrap_or_default(),
                source: source.to_owned(),
                path: path.to_string_lossy().into_owned(),
            });
        }
    };
    visit(home.join("agents"), "personal");
    if let Ok(plugins) = std::fs::read_dir(home.join("installed-plugins")) {
        for plugin in plugins.filter_map(|e| e.ok()) {
            let name = plugin.file_name().to_string_lossy().into_owned();
            visit(plugin.path().join("agents"), &format!("plugin: {name}"));
            if let Ok(nested) = std::fs::read_dir(plugin.path()) {
                for child in nested.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                    visit(child.path().join("agents"), &format!("plugin: {name}"));
                }
            }
        }
    }
    agents.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    agents.dedup_by(|a, b| a.path == b.path);
    agents
}

fn flatten(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let full = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                flatten(&full, child, out);
            }
        }
        serde_json::Value::String(s) => out.push((prefix.to_owned(), s.clone())),
        other => out.push((prefix.to_owned(), other.to_string())),
    }
}

/// Loads everything (several CLI calls run in parallel).
pub fn snapshot() -> Snapshot {
    let mut snapshot = Snapshot::default();
    std::thread::scope(|scope| {
        let plugins = scope.spawn(|| list::<Vec<Plugin>>(&["plugin", "list", "--json"]));
        let skills = scope.spawn(|| list::<Vec<Skill>>(&["skill", "list", "--json"]));
        let mcp = scope.spawn(|| list::<serde_json::Value>(&["mcp", "list", "--json"]));
        let instructions = scope.spawn(|| list::<serde_json::Value>(&["instruction", "list", "--json"]));
        let settings = scope.spawn(|| list::<serde_json::Value>(&["config", "--list", "--json"]));
        // The model choices its help lists (when this version lists them).
        let help = scope.spawn(|| run_cli(&["--help"], Duration::from_secs(30)));
        match plugins.join().expect("thread") {
            Ok(v) => snapshot.plugins = v,
            Err(e) => snapshot.errors.push(format!("plugins: {e:#}")),
        }
        match skills.join().expect("thread") {
            Ok(mut v) => {
                v.sort_by(|a, b| a.source.cmp(&b.source).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                snapshot.skills = v;
            }
            Err(e) => snapshot.errors.push(format!("skills: {e:#}")),
        }
        match mcp.join().expect("thread") {
            Ok(v) => snapshot.mcp = mcp_from_value(&v),
            Err(e) => snapshot.errors.push(format!("MCP: {e:#}")),
        }
        match instructions.join().expect("thread") {
            Ok(v) => {
                let items = match v {
                    serde_json::Value::Array(items) => items,
                    single @ serde_json::Value::Object(_) => vec![single],
                    _ => Vec::new(),
                };
                snapshot.instructions = items.into_iter().filter_map(|i| serde_json::from_value(i).ok()).collect();
            }
            Err(e) => snapshot.errors.push(format!("instructions: {e:#}")),
        }
        match settings.join().expect("thread") {
            Ok(v) => flatten("", &v, &mut snapshot.settings),
            Err(e) => snapshot.errors.push(format!("settings: {e:#}")),
        }
        if let Ok(help) = help.join().expect("thread") {
            snapshot.models = crate::agents::config::help_models(&help);
        }
    });
    snapshot.agents = scan_agents(&copilot_home());
    snapshot
}

pub fn set_enabled(kind: Kind, name: &str, enabled: bool) -> Result<String> {
    run_cli(&[kind.word(), if enabled { "enable" } else { "disable" }, name], Duration::from_secs(60))
}

pub fn remove(kind: Kind, name: &str) -> Result<String> {
    let verb = if kind == Kind::Plugin { "uninstall" } else { "remove" };
    run_cli(&[kind.word(), verb, name], Duration::from_secs(120))
}

pub fn update_plugin(name: &str) -> Result<String> {
    run_cli(&["plugin", "update", name], Duration::from_secs(300))
}

pub fn add_skill(source: &str) -> Result<String> {
    run_cli(&["skill", "add", source], Duration::from_secs(120))
}

pub fn config_set(key: &str, value: &str) -> Result<String> {
    anyhow::ensure!(!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'), "invalid setting key");
    run_cli(&["config", key, "--", value], Duration::from_secs(30))
}

pub fn config_remove(key: &str) -> Result<String> {
    run_cli(&["config", "--rm", key], Duration::from_secs(30))
}

pub fn add_mcp(line: &str) -> Result<String> {
    let args = parse_mcp_add(line)?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cli(&refs, Duration::from_secs(60))
}

pub fn open_in_explorer(path: &Path) -> Result<()> {
    let target = if path.is_file() { path.parent().unwrap_or(path) } else { path };
    anyhow::ensure!(target.exists(), "{} does not exist", target.display());
    Command::new("explorer.exe").arg(target).spawn().context("cannot open File Explorer")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_after_warning_lines() {
        let text = "Warning: something malformed\n{\"mcpServers\": {}}\n";
        assert_eq!(json_payload(text), Some("{\"mcpServers\": {}}"));
        assert_eq!(json_payload("[1,2]"), Some("[1,2]"));
        assert_eq!(json_payload("nothing"), None);
    }

    #[test]
    fn mcp_listing_shapes_are_normalized_without_secrets() {
        let user = serde_json::json!({"mcpServers": {
            "local": {"type": "stdio", "command": "node", "args": ["server.js"], "env": {"TOKEN": "secret"}},
            "remote": {"type": "http", "url": "https://x.example", "headers": {"Authorization": "secret"}, "disabled": true}
        }});
        let servers = mcp_from_value(&user);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].target, "node server.js");
        assert!(!servers[1].enabled);
        assert!(servers.iter().all(|s| !s.target.contains("secret")));
        let grouped = serde_json::json!({"Plugin": [{"name": "p1", "type": "stdio", "command": "x"}], "User": {"u1": {"url": "https://y"}}});
        let servers = mcp_from_value(&grouped);
        assert_eq!(servers.iter().map(|s| (s.name.as_str(), s.source.as_str())).collect::<Vec<_>>(), vec![("p1", "Plugin"), ("u1", "User")]);
    }

    #[test]
    fn agents_are_discovered_with_frontmatter() {
        let home = std::env::temp_dir().join(format!("cc-agents-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::create_dir_all(home.join("installed-plugins").join("p").join("agents")).unwrap();
        std::fs::write(home.join("agents").join("reviewer.agent.md"), "---\nname: Reviewer\ndescription: Reviews code\n---\nbody").unwrap();
        std::fs::write(home.join("installed-plugins").join("p").join("agents").join("helper.md"), "no frontmatter").unwrap();
        let agents = scan_agents(&home);
        assert_eq!(agents.len(), 2);
        assert_eq!((agents[0].name.as_str(), agents[0].source.as_str(), agents[0].description.as_str()), ("Reviewer", "personal", "Reviews code"));
        assert_eq!((agents[1].name.as_str(), agents[1].source.as_str()), ("helper", "plugin: p"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn settings_flatten_to_dot_keys() {
        let mut out = Vec::new();
        flatten("", &serde_json::json!({"effortLevel": "high", "sandbox": {"enabled": true}, "list": ["a"]}), &mut out);
        assert!(out.contains(&("effortLevel".into(), "high".into())));
        assert!(out.contains(&("sandbox.enabled".into(), "true".into())));
        assert!(out.contains(&("list".into(), "[\"a\"]".into())));
    }
}
