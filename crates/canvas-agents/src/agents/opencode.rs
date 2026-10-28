//! OpenCode.
//!
//! Sessions live in OpenCode's SQLite database (read by
//! [`super::opencode_history`]). Every interactive launch gets its own
//! password-protected server port so the canvas can ask the running TUI for
//! what the database does not hold (busy state, permission prompts).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use canvas_common::protocol::{AgentLaunch, SessionSummary};
use serde_json::{Value, json};

use super::opencode_history::{OpenCodeHistory, db_path};
pub use super::opencode_history::{discover_new_session, discover_new_sessions};
use super::opencode_server::{self, Live};
use super::{AgentKind, Caps, Command, History, Integration, Provider};
use crate::paths::now_ms;

pub struct OpenCode;

/// Environment entries that tell the service where a TUI's server listens.
pub const PORT_ENV: &str = "COPILOT_CANVAS_OPENCODE_PORT";
pub const PASSWORD_ENV: &str = "COPILOT_CANVAS_OPENCODE_PASSWORD";

pub fn is_session_id(id: &str) -> bool {
    id.len() > 4 && id.len() <= 64 && id.starts_with("ses_") && id[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// The OpenCode executable: `COPILOT_CANVAS_OPENCODE`, else `opencode.exe` on
/// PATH, else the binary behind npm's `opencode.cmd` shim.
pub fn find_cli() -> Result<String> {
    if let Some(custom) = std::env::var_os("COPILOT_CANVAS_OPENCODE") {
        return Ok(custom.to_string_lossy().into_owned());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let exe = dir.join("opencode.exe");
        if exe.is_file() {
            return Ok(exe.display().to_string());
        }
        if dir.join("opencode.cmd").is_file() {
            // A .cmd shim would run under cmd.exe (Ctrl+C asks "Terminate batch job?").
            let native = dir.join("node_modules").join("opencode-ai").join("bin").join("opencode.exe");
            return Ok(if native.is_file() { native } else { dir.join("opencode.cmd") }.display().to_string());
        }
    }
    anyhow::bail!("opencode was not found on PATH (set COPILOT_CANVAS_OPENCODE to override)")
}

/// A TUI server the canvas started.
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    /// The canvas terminal it runs in.
    pub terminal: String,
    pub port: u16,
    pub password: String,
    /// The session it opened (resume), when known up front.
    pub session: Option<String>,
    pub registered_ms: u64,
}

/// What the canvas last learned from a server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerState {
    /// The latest answer; `None` while the server does not answer.
    pub live: Option<Live>,
    pub ok_ms: u64,
    /// Sessions this server worked on (opened, busy, waiting, or created after it started).
    pub sessions: HashSet<String>,
}

struct Slot {
    server: Server,
    state: ServerState,
    watched: bool,
}

static SERVERS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

fn servers_lock() -> std::sync::MutexGuard<'static, Vec<Slot>> {
    SERVERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remembers a TUI server (replacing any earlier one of that terminal).
pub fn register_server(server: Server) {
    let mut servers = servers_lock();
    servers.retain(|s| s.server.terminal != server.terminal);
    let sessions = server.session.iter().cloned().collect();
    servers.push(Slot { server, state: ServerState { sessions, ..Default::default() }, watched: false });
}

pub fn unregister_server(terminal: &str) {
    servers_lock().retain(|s| s.server.terminal != terminal);
}

/// Registered servers with their latest state.
pub fn servers() -> Vec<(Server, ServerState)> {
    servers_lock().iter().map(|s| (s.server.clone(), s.state.clone())).collect()
}

/// Root sessions the TUI in `terminal` has worked on (its session once the first prompt ran).
pub fn server_sessions(terminal: &str) -> Vec<String> {
    let servers = servers_lock();
    let Some(slot) = servers.iter().find(|s| s.server.terminal == terminal) else { return Vec::new() };
    let mut ids: Vec<String> = slot.state.sessions.iter().cloned().collect();
    ids.sort();
    ids
}

/// Starts a watcher thread for every registered server that has none.
pub fn watch_servers() {
    let mut servers = servers_lock();
    for slot in servers.iter_mut().filter(|s| !s.watched) {
        slot.watched = true;
        let server = slot.server.clone();
        let _ = std::thread::Builder::new().name("opencode-server".into()).spawn(move || watch(server));
    }
}

/// Polls one server until it stops answering for good.
fn watch(server: Server) {
    let mut round = 0u32;
    loop {
        // Root sessions change rarely (the first prompt, `/new`): list them every few rounds.
        let known = servers_lock().iter().find(|s| s.server == server).is_some_and(|s| !s.state.sessions.is_empty());
        let result = opencode_server::live_with(server.port, &server.password, !known || round.is_multiple_of(5));
        round = round.wrapping_add(1);
        let ok = result.is_ok();
        {
            let mut servers = servers_lock();
            let Some(slot) = servers.iter_mut().find(|s| s.server == server) else { return };
            let now = now_ms();
            match result {
                Ok(live) => {
                    let state = &mut slot.state;
                    state.ok_ms = now;
                    state.sessions.extend(live.status.keys().cloned());
                    state.sessions.extend(live.waiting().map(str::to_owned));
                    // The session a new TUI creates on its first prompt.
                    state.sessions.extend(live.sessions.iter().filter(|s| s.parent.is_none() && s.created_ms + 2_000 >= server.registered_ms).map(|s| s.id.clone()));
                    state.live = Some(live);
                }
                Err(_) => {
                    slot.state.live = None;
                    let (since, patience) = if slot.state.ok_ms > 0 { (slot.state.ok_ms, 30_000) } else { (server.registered_ms, 120_000) };
                    if now.saturating_sub(since) > patience {
                        servers.retain(|s| s.server != server);
                        return;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(if ok { 1_000 } else { 2_000 }));
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("no free local port for the OpenCode server")?;
    Ok(listener.local_addr()?.port())
}

/// Deep-merges `overlay` into `base` (objects merge, arrays append).
fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(overlay)) => base.extend(overlay),
        (base, overlay) => *base = overlay,
    }
}

/// `overlay` merged into the JSON of the inherited variable `name`.
fn merged_env(name: &str, overlay: Value) -> String {
    let mut base = std::env::var(name).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    merge(&mut base, overlay);
    base.to_string()
}

/// `external_directory` rules that let the session use `dir`.
fn allow_dir(rules: &mut serde_json::Map<String, Value>, dir: &str) {
    let dir = dir.trim().trim_end_matches(['\\', '/']).replace('\\', "/");
    if !dir.is_empty() {
        rules.insert(dir.clone(), json!("allow"));
        rules.insert(format!("{dir}/*"), json!("allow"));
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

impl OpenCode {
    /// The command line; `register` records the TUI's server for status.
    pub fn command_with(&self, id: &str, launch: &AgentLaunch, integration: &Integration, register: bool) -> Result<Command> {
        let native = launch.native_id.as_deref().filter(|n| is_session_id(n)).or_else(|| is_session_id(id).then_some(id));
        let mut args: Vec<String> = Vec::new();
        let mut env = integration.env.clone();
        let mut rules = serde_json::Map::new();
        let mut config = json!({});
        let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));
        let resumed = if launch.resume { Some(native.context("resuming an OpenCode session needs its session id (ses_…)")?) } else { None };
        if let Some(parent) = launch.fork_from.as_deref() {
            anyhow::ensure!(is_session_id(parent), "refusing to fork an invalid OpenCode session id");
        }
        let headless = launch.headless_prompt.is_some();
        if let Some(prompt) = launch.headless_prompt.as_deref() {
            // Passive run: one turn, never asks; the terminal shows the text output.
            push(&["run", prompt]);
            if let Some(session) = resumed {
                push(&["-s", session]);
            } else if let Some(parent) = launch.fork_from.as_deref() {
                push(&["-s", parent, "--fork"]);
            } else if let Some(name) = launch.name.as_deref().filter(|n| !n.trim().is_empty()) {
                push(&["--title", name.trim()]);
            }
            push(&["--auto"]);
            if let Some(folder) = &launch.group_folder {
                allow_dir(&mut rules, folder);
            }
        } else {
            match (launch.fork_from.as_deref(), resumed) {
                (Some(parent), _) => push(&["-s", parent, "--fork"]),
                (None, Some(session)) => push(&["-s", session]),
                (None, None) => {}
            }
            if let Some(prompt) = launch.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                push(&["--prompt", prompt]);
            }
            if launch.allow_all {
                push(&["--auto"]);
            }
        }
        let mut flag = |name: &str, value: &Option<String>| {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                args.extend([name.to_owned(), value.to_owned()]);
            }
        };
        flag("-m", &launch.model);
        flag("--agent", &launch.custom_agent);
        for dir in &launch.add_dirs {
            allow_dir(&mut rules, dir);
        }
        if !headless && let (Some(folder), Some(name)) = (&launch.group_folder, &launch.group_name) {
            let dirs = super::group_dirs(folder, name)?;
            allow_dir(&mut rules, &dirs.ai_docs.display().to_string());
            allow_dir(&mut rules, &dirs.outputs.display().to_string());
            merge(&mut config, json!({ "instructions": [path_text(&dirs.instructions_file)] }));
        }
        if !launch.mcp_off.is_empty() {
            let off: serde_json::Map<String, Value> = launch.mcp_off.iter().map(|name| (name.clone(), json!({ "enabled": false }))).collect();
            merge(&mut config, json!({ "mcp": off }));
        }
        if let Some(plugin) = &integration.plugin_dir {
            // The canvas skills (OpenCode reads `skills/<name>/SKILL.md` below these paths).
            merge(&mut config, json!({ "skills": { "paths": [path_text(&plugin.join("skills"))] } }));
        }
        if config.as_object().is_some_and(|c| !c.is_empty()) {
            env.push(("OPENCODE_CONFIG_CONTENT".to_owned(), merged_env("OPENCODE_CONFIG_CONTENT", config)));
        }
        if !rules.is_empty() {
            env.push(("OPENCODE_PERMISSION".to_owned(), merged_env("OPENCODE_PERMISSION", json!({ "external_directory": rules }))));
        }
        env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()));
        if !headless {
            let port = free_port()?;
            let password = uuid::Uuid::new_v4().simple().to_string();
            args.extend(["--port".to_owned(), port.to_string()]);
            env.extend([
                ("OPENCODE_SERVER_USERNAME".to_owned(), opencode_server::USERNAME.to_owned()),
                ("OPENCODE_SERVER_PASSWORD".to_owned(), password.clone()),
                (PORT_ENV.to_owned(), port.to_string()),
                (PASSWORD_ENV.to_owned(), password.clone()),
            ]);
            if register {
                register_server(Server { terminal: id.to_owned(), port, password, session: resumed.map(str::to_owned), registered_ms: now_ms() });
            }
        }
        env.extend(launch.env.iter().cloned());
        Ok(Command { program: self.program()?, args, env })
    }
}

impl Provider for OpenCode {
    fn kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    fn caps(&self) -> Caps {
        Caps {
            preassigned_ids: false,
            native_fork: true,
            copy_fork: false,
            mcp_select: true,
            progress: false,
            config: true,
            automations: false,
            effort: false,
            modes: false,
        }
    }

    fn program(&self) -> Result<String> {
        find_cli()
    }

    fn is_session_id(&self, id: &str) -> bool {
        is_session_id(id)
    }

    fn reserved_args(&self) -> &'static [&'static str] {
        &["-s", "--session", "-c", "--continue", "--fork", "--port", "--hostname"]
    }

    fn command(&self, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command> {
        self.command_with(id, launch, integration, true)
    }

    fn history(&self) -> Box<dyn History> {
        Box::new(OpenCodeHistory::new(db_path()))
    }

    fn is_live(&self, native_id: &str) -> bool {
        let fresh = now_ms().saturating_sub(5_000);
        servers().iter().any(|(_, state)| state.ok_ms >= fresh && state.sessions.contains(native_id))
    }

    fn has_history(&self, native_id: &str) -> bool {
        is_session_id(native_id) && super::opencode_history::session_exists(&db_path(), native_id)
    }

    fn history_root(&self) -> PathBuf {
        db_path()
    }

    fn terminal_exited(&self, id: &str, _home: &Path) {
        unregister_server(id);
    }
}

impl History for OpenCodeHistory {
    fn poll(&mut self, tracked: &HashSet<String>, full_every: Duration) -> (Vec<SessionSummary>, Vec<String>) {
        OpenCodeHistory::poll(self, tracked, full_every)
    }

    fn take_detected(&mut self) -> Vec<(String, String, Value)> {
        OpenCodeHistory::take_detected(self)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SES: &str = "ses_e7fbbd911ffeOynBIm3IrWFsUl";
    const PARENT: &str = "ses_0bd2fb5f8ffezRuY9i1WNnJajX";

    fn integration() -> Integration {
        Integration { plugin_dir: Some(PathBuf::from(r"C:\h\plugin")), env: vec![("COPILOT_CANVAS_SESSION".into(), "t1".into())] }
    }

    fn env<'a>(cmd: &'a Command, key: &str) -> Option<&'a str> {
        cmd.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn setup() {
        // SAFETY: tests in this crate only read these variables.
        unsafe { std::env::set_var("COPILOT_CANVAS_OPENCODE", r"C:\bin\opencode.exe") };
    }

    #[test]
    fn new_resume_fork_and_headless_command_lines() {
        setup();
        let new = AgentLaunch { agent: "opencode".into(), name: Some("Fix".into()), prompt: Some("hello".into()), model: Some("opencode/x-free".into()), allow_all: true, ..Default::default() };
        let cmd = OpenCode.command_with("t1", &new, &integration(), false).unwrap();
        assert_eq!(cmd.program, r"C:\bin\opencode.exe");
        assert_eq!(cmd.args[..6], ["--prompt", "hello", "--auto", "-m", "opencode/x-free", "--port"]);
        let port: u16 = cmd.args[6].parse().unwrap();
        assert_eq!(env(&cmd, PORT_ENV), Some(cmd.args[6].as_str()));
        assert_eq!(env(&cmd, "OPENCODE_SERVER_PASSWORD"), env(&cmd, PASSWORD_ENV));
        assert_eq!(env(&cmd, "OPENCODE_SERVER_PASSWORD").unwrap().len(), 32);
        assert!(port > 0);
        assert_eq!(env(&cmd, "OPENCODE_DISABLE_AUTOUPDATE"), Some("1"));
        let config: Value = serde_json::from_str(env(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(config["skills"]["paths"], json!(["C:/h/plugin/skills"]));
        assert_eq!(cmd.env[0], ("COPILOT_CANVAS_SESSION".into(), "t1".into()));

        let resume = AgentLaunch { resume: true, native_id: Some(SES.into()), prompt: None, allow_all: false, model: None, ..new.clone() };
        assert_eq!(OpenCode.command_with("t1", &resume, &integration(), false).unwrap().args[..3], ["-s", SES, "--port"]);
        assert!(OpenCode.command_with("t1", &AgentLaunch { native_id: None, ..resume.clone() }, &integration(), false).is_err(), "no session id");
        assert_eq!(OpenCode.command_with(SES, &AgentLaunch { native_id: None, ..resume.clone() }, &integration(), false).unwrap().args[..2], ["-s", SES]);

        let fork = AgentLaunch { fork_from: Some(PARENT.into()), ..resume.clone() };
        assert_eq!(OpenCode.command_with("t1", &AgentLaunch { resume: false, ..fork.clone() }, &integration(), false).unwrap().args[..3], ["-s", PARENT, "--fork"]);
        assert!(OpenCode.command_with("t1", &AgentLaunch { fork_from: Some("x".into()), ..fork }, &integration(), false).is_err());

        let passive = AgentLaunch { headless_prompt: Some("check".into()), group_folder: Some(r"C:\g".into()), ..resume };
        let cmd = OpenCode.command_with("t1", &passive, &integration(), false).unwrap();
        assert_eq!(cmd.args, ["run", "check", "-s", SES, "--auto"]);
        assert!(env(&cmd, "OPENCODE_SERVER_PASSWORD").is_none(), "no server for passive runs");
        let rules: Value = serde_json::from_str(env(&cmd, "OPENCODE_PERMISSION").unwrap()).unwrap();
        assert_eq!(rules["external_directory"]["C:/g/*"], "allow");
        let first = AgentLaunch { resume: false, native_id: None, ..passive };
        assert_eq!(OpenCode.command_with("t1", &first, &integration(), false).unwrap().args, ["run", "check", "--title", "Fix", "--auto"]);
    }

    #[test]
    fn mcp_off_dirs_and_group_folders_become_config_and_permissions() {
        setup();
        let root = std::env::temp_dir().join(format!("cc-oc-cmd-{}", uuid::Uuid::new_v4()));
        let folder = root.join("group");
        let launch = AgentLaunch {
            agent: "opencode".into(),
            mcp_off: vec!["github".into(), "unityMCP".into()],
            add_dirs: vec![r"C:\extra\".into()],
            group_folder: Some(folder.display().to_string()),
            group_name: Some("Team".into()),
            custom_agent: Some("plan".into()),
            ..Default::default()
        };
        let cmd = OpenCode.command_with("t1", &launch, &integration(), false).unwrap();
        assert_eq!(cmd.args[..2], ["--agent", "plan"]);
        let config: Value = serde_json::from_str(env(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(config["mcp"], json!({ "github": { "enabled": false }, "unityMCP": { "enabled": false } }));
        let instructions = path_text(&folder.join(".canvas").join("canvas-group.instructions.md"));
        assert_eq!(config["instructions"], json!([instructions]));
        let rules: Value = serde_json::from_str(env(&cmd, "OPENCODE_PERMISSION").unwrap()).unwrap();
        let rules = rules["external_directory"].as_object().unwrap();
        assert_eq!(rules["C:/extra"], "allow");
        assert_eq!(rules["C:/extra/*"], "allow");
        assert_eq!(rules[&format!("{}/*", path_text(&folder.join("ai-docs")))], "allow");
        assert_eq!(rules[&format!("{}/*", path_text(&folder.join("human-outputs")))], "allow");
        assert!(!cmd.args.iter().any(|a| a == "--auto"), "asks unless allow-all");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn json_overlays_merge_objects_and_append_arrays() {
        let mut base = json!({ "mcp": { "a": { "type": "remote", "enabled": true } }, "instructions": ["x"] });
        merge(&mut base, json!({ "mcp": { "a": { "enabled": false }, "b": { "enabled": false } }, "instructions": ["y"] }));
        assert_eq!(base, json!({ "mcp": { "a": { "type": "remote", "enabled": false }, "b": { "enabled": false } }, "instructions": ["x", "y"] }));
    }

    #[test]
    fn registered_servers_are_listed_and_replaced_per_terminal() {
        let server = |terminal: &str, port| Server { terminal: terminal.into(), port, password: "p".into(), session: Some(SES.into()), registered_ms: 1 };
        register_server(server("reg-a", 1));
        register_server(server("reg-a", 2));
        let mine: Vec<_> = servers().into_iter().filter(|(s, _)| s.terminal == "reg-a").collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].0.port, 2);
        assert_eq!(server_sessions("reg-a"), [SES]);
        unregister_server("reg-a");
        assert!(server_sessions("reg-a").is_empty());
    }
}
