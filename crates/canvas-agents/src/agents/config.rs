//! Configuration views (plugins, MCP servers, skills, agents, settings) for
//! every agent, in the shape of Copilot's [`crate::copilot_cfg::Snapshot`].
//! CLIs run hidden with timeouts, their output in memory only. Secrets never
//! reach the snapshot or error messages: MCP targets lose URL credentials and
//! secret-looking arguments, settings hide secret-looking paths (see
//! [`redact_setting`]).

use std::io::Read;
use canvas_sys::spawn::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::AgentKind;
use crate::copilot_cfg::{self, Agent, Kind, McpServer, Plugin, Skill, Snapshot, json_payload};
pub use canvas_model::config::{caps_for_changes, caps_for_views};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Output kept per stream of a CLI run; the rest is dropped.
const MAX_OUTPUT: usize = 4 << 20;
/// What replaces a secret in views and messages.
const HIDDEN: &str = "(hidden)";

/// An empty working folder for a CLI run (so it picks up no project
/// configuration), removed when dropped, retrying while the child lets go.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("copilot-canvas-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for attempt in 1..=10 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50 * attempt));
        }
    }
}

/// A child's output stream, drained on its own thread into a bounded buffer.
struct Capture {
    data: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Capture {
    fn start(stream: Option<impl Read + Send + 'static>) -> Self {
        let data = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::channel();
        if let Some(mut stream) = stream {
            let sink = data.clone();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                while let Ok(n) = stream.read(&mut buffer)
                    && n > 0
                {
                    let mut data = sink.lock().unwrap_or_else(|e| e.into_inner());
                    let room = MAX_OUTPUT.saturating_sub(data.len());
                    data.extend_from_slice(&buffer[..n.min(room)]);
                }
                let _ = tx.send(());
            });
        }
        Self { data, done }
    }

    /// What was read. Waits briefly for the end of the stream: a process the
    /// CLI started may keep it open after the CLI exited.
    fn finish(self) -> String {
        let _ = self.done.recv_timeout(Duration::from_secs(2));
        String::from_utf8_lossy(&self.data.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
}

fn kill(child: &mut std::process::Child) {
    let _ = canvas_sys::process::kill_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs an agent CLI without a window and returns stdout. Output goes
/// through pipes drained by threads: it may hold secrets (`opencode debug
/// config` prints API keys) and never touches the disk.
pub fn run(program: &str, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Result<String> {
    let cwd = TempDir::new()?;
    let mut command = Command::new(program);
    command.args(args).current_dir(&cwd.0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).no_window();
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().with_context(|| format!("cannot run {program}"))?;
    let stdout = Capture::start(child.stdout.take());
    let stderr = Capture::start(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                kill(&mut child);
                bail!("{} {} timed out", Path::new(program).file_stem().unwrap_or_default().to_string_lossy(), args.join(" "));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                kill(&mut child);
                return Err(error.into());
            }
        }
    };
    let (stdout, stderr) = (stdout.finish(), stderr.finish());
    if !status.success() {
        bail!("{}", failure_message(&stderr, &stdout, status));
    }
    Ok(stdout)
}

/// A failed CLI's last line of output, redacted, when it is short; else a generic message.
fn failure_message(stderr: &str, stdout: &str, status: std::process::ExitStatus) -> String {
    let text = if stderr.trim().is_empty() { stdout } else { stderr };
    match text.trim().lines().last().map(str::trim) {
        Some(line) if !line.is_empty() && line.chars().count() <= 200 => redact_text(line),
        _ => format!("the command failed ({status})"),
    }
}

/// Words naming a secret (flags, `KEY=VALUE` keys, header names).
fn secret_name(name: &str) -> bool {
    let lower = name.trim_start_matches('-').to_ascii_lowercase();
    ["api-key", "apikey", "api_key", "token", "password", "passwd", "secret", "auth", "bearer", "credential"].iter().any(|w| lower.contains(w))
}

/// A URL without user info, query string or fragment.
fn redact_url(url: &str) -> String {
    let main = &url[..url.find(['?', '#']).unwrap_or(url.len())];
    let Some(scheme) = main.find("://") else { return main.to_owned() };
    let rest = &main[scheme + 3..];
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    match authority.rfind('@') {
        Some(at) => format!("{}{}", &main[..scheme + 3], &rest[at + 1..]),
        None => main.to_owned(),
    }
}

/// One word with secrets replaced: URL credentials, `KEY=VALUE` and
/// `Header: value` with secret-looking names.
fn redact_word(word: &str) -> String {
    if let Some(scheme) = word.find("://")
        && word.find('=').is_none_or(|eq| scheme < eq)
    {
        return redact_url(word);
    }
    if let Some((key, value)) = word.split_once('=') {
        return if secret_name(key) { format!("{key}={HIDDEN}") } else { format!("{key}={}", redact_word(value)) };
    }
    match word.split_once(':') {
        Some((name, _)) if secret_name(name) => format!("{name}: {HIDDEN}"),
        _ => word.to_owned(),
    }
}

/// Command-line words with secrets replaced: [`redact_word`], plus the word
/// after a secret-looking flag (`--api-key X`) or `Bearer`.
fn redact_words<'a>(words: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut hide_next = false;
    for word in words {
        if std::mem::take(&mut hide_next) {
            out.push(HIDDEN.to_owned());
            continue;
        }
        hide_next = !word.contains('=') && !word.contains(':') && ((word.starts_with('-') && secret_name(word)) || word.eq_ignore_ascii_case("bearer"));
        out.push(redact_word(word));
    }
    out
}

/// A line of text (an error message) with secrets replaced.
fn redact_text(line: &str) -> String {
    let words = redact_words(line.split_whitespace());
    // "Authorization: Bearer x" spans words: a hidden header hides the rest of the line.
    match words.iter().position(|w| w.ends_with(&format!(": {HIDDEN}"))) {
        Some(at) => words[..=at].join(" "),
        None => words.join(" "),
    }
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

/// Setting paths whose values may be secrets (any part of the dotted path).
fn secret(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    ["environment", "headers", "env", "token", "key", "secret", "password", "auth", "credential", "cookie"].iter().any(|w| lower.contains(w))
}

/// The last part of a dotted setting path, lowercased.
fn leaf(path: &str) -> String {
    path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase()
}

/// The program of a command (a string or an array of words): its file name only.
fn program_name(command: &Value) -> Value {
    let first = match command {
        Value::String(s) => s.split_whitespace().next(),
        Value::Array(items) => items.first().and_then(Value::as_str),
        _ => None,
    };
    let first = first.unwrap_or_default().trim_matches('"');
    Value::String(first.rsplit(['\\', '/']).next().unwrap_or(first).to_owned())
}

/// A setting value with secrets replaced, walking objects and arrays: values
/// below secret-looking paths are hidden, URLs lose credentials and queries,
/// commands show their program only.
fn redact_setting(path: &str, value: &Value) -> Value {
    match value {
        _ if leaf(path).contains("command") && !secret(path) && !value.is_object() => program_name(value),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(key, child)| (key.clone(), redact_setting(&if path.is_empty() { key.clone() } else { format!("{path}.{key}") }, child))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| redact_setting(path, item)).collect()),
        _ if secret(path) => Value::String(HIDDEN.to_owned()),
        Value::String(s) if leaf(path).contains("url") => Value::String(redact_url(s)),
        other => other.clone(),
    }
}

/// Settings as (key, value) pairs, secrets hidden.
fn settings(value: &Value, skip: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(map) = value.as_object() {
        for (key, child) in map.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
            flatten(key, &redact_setting(key, child), &mut out);
        }
    }
    out
}

fn mcp_entry(name: &str, entry: &Value, source: &str) -> McpServer {
    let transport = entry["type"].as_str().unwrap_or(if entry.get("url").is_some() { "http" } else { "stdio" }).to_owned();
    // Shown in the UI: never with credentials (URL user info and queries,
    // values of secret-looking flags and KEY=VALUE arguments).
    let target = match entry["url"].as_str() {
        Some(url) => redact_url(url),
        None => {
            // Claude: command + args; OpenCode: command is an array.
            let mut words: Vec<&str> = match &entry["command"] {
                Value::String(s) => vec![s.as_str()],
                Value::Array(items) => items.iter().filter_map(|v| v.as_str()).collect(),
                _ => Vec::new(),
            };
            words.extend(entry["args"].as_array().into_iter().flatten().filter_map(|v| v.as_str()));
            redact_words(words).join(" ")
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
        let models = scope.spawn(|| opencode(&["models"]));
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
        match models.join().expect("thread") {
            Ok(text) => snapshot.models = model_list(&text),
            Err(e) => snapshot.errors.push(format!("models: {e:#}")),
        }
    });
    sort(&mut snapshot);
    snapshot
}

/// `opencode models`: one `provider/model` id per line (other lines are skipped).
pub fn model_list(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in text.lines().map(str::trim).filter(|l| l.contains('/') && canvas_model::models::valid_id(l)) {
        if !out.iter().any(|o| o == id) {
            out.push(id.to_owned());
        }
    }
    out
}

/// Model ids an agent CLI's `--help` offers for its `--model` option: the
/// quoted words of that option's description (`(choices: "a", "b")` in
/// Copilot CLI, `(e.g. 'fable', 'opus', or 'sonnet')` in Claude Code).
pub fn help_models(help: &str) -> Vec<String> {
    let lines: Vec<&str> = help.lines().collect();
    let is_model = |line: &str| {
        let line = line.trim_start();
        ["--model ", "--model=", "-m, --model"].iter().any(|p| line.starts_with(p))
    };
    let Some(start) = lines.iter().position(|l| is_model(l)) else { return Vec::new() };
    // The description runs on to the next option (or a blank line).
    let mut text = lines[start].to_owned();
    for line in lines[start + 1..].iter().map(|l| l.trim_start()).take_while(|l| !l.is_empty() && !l.starts_with('-')) {
        text.push(' ');
        text.push_str(line);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let quote = chars[i];
        // A quote that opens a word (not the apostrophe in "model's").
        if (quote == '"' || quote == '\'') && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(len) = chars[i + 1..].iter().position(|c| *c == quote)
        {
            let word: String = chars[i + 1..i + 1 + len].iter().collect();
            if canvas_model::models::valid_id(&word) && !out.contains(&word) {
                out.push(word);
            }
            i += len + 2;
            continue;
        }
        i += 1;
    }
    out
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
        snapshot.plugins.push(Plugin { name, marketplace: "npm".into(), version, enabled: true, source: plugin.to_owned(), ..Default::default() });
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
    let (plugins, help) = std::thread::scope(|scope| {
        let help = scope.spawn(|| claude(&["--help"]));
        let plugins = claude(&["plugin", "list", "--json"]).and_then(|o| json(&o, "claude plugin list"));
        (plugins, help.join().expect("thread"))
    });
    match plugins {
        Ok(value) => snapshot.plugins = claude_plugins(&value),
        Err(e) => snapshot.errors.push(format!("plugins: {e:#}")),
    }
    // Its model aliases (a model's full name works too).
    if let Ok(help) = help {
        snapshot.models = help_models(&help);
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
                ..Default::default()
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

    #[test]
    fn mcp_targets_never_show_credentials() {
        let remote = mcp_entry("r", &json!({ "url": "https://user:SECRET1@mcp.example:8443/x?api_key=SECRET2#SECRET3" }), "user");
        assert_eq!(remote.target, "https://mcp.example:8443/x");
        let args = json!([
            "-y", "srv", "--api-key", "SECRET4", "--token=SECRET5", "GITHUB_PAT_TOKEN=SECRET6", "--port", "80", "-H", "Authorization: Bearer SECRET7",
            "--url=https://u:SECRET8@h/q?x=SECRET9", "Bearer", "SECRET10", "--password", "SECRET11", "--verbose", "https://plain.example/a?SECRET12"
        ]);
        let local = mcp_entry("l", &json!({ "command": "npx", "args": args }), "user");
        assert_eq!(
            local.target,
            "npx -y srv --api-key (hidden) --token=(hidden) GITHUB_PAT_TOKEN=(hidden) --port 80 -H Authorization: (hidden) --url=https://h/q Bearer (hidden) --password (hidden) --verbose https://plain.example/a"
        );
        let opencode = mcp_entry("o", &json!({ "type": "local", "command": ["uvx", "srv", "--auth-token", "SECRET13"] }), "config");
        assert_eq!(opencode.target, "uvx srv --auth-token (hidden)");
        assert!(!format!("{remote:?}{local:?}{opencode:?}").contains("SECRET"));
    }

    #[test]
    fn settings_hide_secrets_in_nested_objects_arrays_urls_and_commands() {
        let value = json!({
            "model": "opus",
            "env": { "API_KEY": "SECRET1", "PLAIN": "SECRET2" },
            "servers": [{ "name": "a", "token": "SECRET3" }, { "name": "b", "headers": { "X": "SECRET4" } }],
            "proxy": { "url": "http://u:SECRET5@proxy:8080/?k=SECRET6" },
            "statusLine": { "type": "command", "command": "C:\\tools\\status.exe --password SECRET7" },
            "hooks": { "Stop": [{ "matcher": "*", "command": ["node", "hook.js", "--secret", "SECRET8"] }] },
            "apiKeyHelper": "SECRET9",
            "cookies": ["SECRET10"],
            "credentials": { "list": [1, 2] },
            "theme": "dark"
        });
        let out = settings(&value, &[]);
        assert!(!format!("{out:?}").contains("SECRET"), "{out:?}");
        for expected in [
            ("model", "opus"),
            ("theme", "dark"),
            ("env.API_KEY", HIDDEN),
            ("env.PLAIN", HIDDEN),
            ("proxy.url", "http://proxy:8080/"),
            ("statusLine.type", "command"),
            ("statusLine.command", "status.exe"),
            ("apiKeyHelper", HIDDEN),
        ] {
            assert!(out.contains(&(expected.0.to_owned(), expected.1.to_owned())), "{expected:?} in {out:?}");
        }
        let servers = &out.iter().find(|(k, _)| k == "servers").unwrap().1;
        assert_eq!(servers, r#"[{"name":"a","token":"(hidden)"},{"headers":{"X":"(hidden)"},"name":"b"}]"#);
        assert_eq!(out.iter().find(|(k, _)| k == "hooks.Stop").unwrap().1, r#"[{"command":"node","matcher":"*"}]"#);
    }

    #[test]
    fn cli_errors_are_short_and_redacted() {
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        // Exit code 3 (Unix wait statuses keep the code in the second byte).
        let status = std::process::ExitStatus::from_raw(if cfg!(windows) { 3 } else { 3 << 8 });
        let message = failure_message("warming up\nError: login failed for https://u:SECRET@x.example/y?token=SECRET --token SECRET\n", "", status);
        assert_eq!(message, "Error: login failed for https://x.example/y --token (hidden)");
        assert_eq!(failure_message("", "out\nAuthorization: Bearer SECRET rest", status), "Authorization: (hidden)");
        assert_eq!(failure_message(&format!("{}\n", "x".repeat(300)), "", status), format!("the command failed ({status})"), "long lines are not echoed");
        assert_eq!(failure_message("  \n", "", status), format!("the command failed ({status})"));
    }

    #[test]
    fn cli_runs_use_pipes_time_out_and_clean_up() {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
        assert_eq!(run(&comspec, &["/d", "/c", "echo hello"], &[], TIMEOUT).unwrap().trim(), "hello");
        let error = run(&comspec, &["/d", "/c", "echo --token SECRET 1>&2 & exit 3"], &[], TIMEOUT).unwrap_err().to_string();
        assert_eq!(error, "--token (hidden)");
        let started = Instant::now();
        let error = run(&comspec, &["/d", "/c", "ping -n 30 127.0.0.1"], &[], Duration::from_millis(300)).unwrap_err().to_string();
        assert!(error.contains("timed out") && started.elapsed() < Duration::from_secs(10), "{error}");
        // A large output never blocks the child.
        let big = run(&comspec, &["/d", "/c", "for /l %i in (1,1,3000) do @echo 0123456789012345678901234567890123456789"], &[], TIMEOUT).unwrap();
        assert_eq!(big.lines().count(), 3000);
    }

    #[test]
    fn model_lists_come_from_each_clis_own_output() {
        // `opencode models` (1.19.4): provider/model per line.
        let listed = model_list("opencode/big-pickle\r\ngithub-copilot/claude-opus-5.5\nWarning: cache refreshed\n\ngithub-copilot/claude-opus-5.5\n");
        assert_eq!(listed, ["opencode/big-pickle", "github-copilot/claude-opus-5.5"]);
        // Claude Code 2.1.309 `--help`: aliases in the description, the apostrophe of "model's" is no quote.
        let claude = "  --mcp-config <configs...>  Load MCP servers\n  --model <model>                       Model for the current session. Provide\n                                        an alias for the latest model (e.g.\n                                        'fable', 'opus', or 'sonnet') or a\n                                        model's full name.\n  -n, --name <name>                     Set a display name ('not-this')\n";
        assert_eq!(help_models(claude), ["fable", "opus", "sonnet"]);
        // Copilot CLI (commander): `(choices: ...)`, wrapped.
        let copilot = "Options:\n  --allow-all  Allow everything\n  --model <model>  Set the AI model to use (choices: \"claude-opus-5.5\",\n                   \"gpt-5.5\", \"claude-sonnet-5\")\n  --no-color  Disable color\n";
        assert_eq!(help_models(copilot), ["claude-opus-5.5", "gpt-5.5", "claude-sonnet-5"]);
        assert!(help_models("Usage: copilot [options]\n  --model <model>  Set the AI model to use\n").is_empty(), "no choices listed");
        assert!(help_models("").is_empty());
    }

    #[test]
    fn temporary_folders_are_removed_even_while_briefly_in_use() {
        let dir = TempDir::new().unwrap();
        let path = dir.0.clone();
        let file = std::fs::File::create(path.join("busy.txt")).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(file);
        });
        drop(dir);
        holder.join().unwrap();
        assert!(!path.exists());
    }
}
