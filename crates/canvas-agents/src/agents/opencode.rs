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

pub fn is_session_id(id: &str) -> bool {
    canvas_model::agent::is_opencode_session_id(id)
}

/// The OpenCode executable: `COPILOT_CANVAS_OPENCODE`, else `opencode.exe` on
/// PATH, else the binary behind npm's `opencode.cmd` shim. Batch scripts are
/// never started (see [`super::ensure_not_batch`]).
pub fn find_cli() -> Result<String> {
    find_cli_in(std::env::var_os("COPILOT_CANVAS_OPENCODE"), std::env::var_os("PATH").unwrap_or_default())
}

fn find_cli_in(custom: Option<std::ffi::OsString>, path: std::ffi::OsString) -> Result<String> {
    if let Some(custom) = custom {
        let custom = custom.to_string_lossy().into_owned();
        super::ensure_not_batch(Path::new(&custom), "COPILOT_CANVAS_OPENCODE")?;
        return Ok(custom);
    }
    for dir in std::env::split_paths(&path) {
        let exe = dir.join("opencode.exe");
        if exe.is_file() {
            return Ok(exe.display().to_string());
        }
        let shim = dir.join("opencode.cmd");
        if shim.is_file() {
            return match shim_target(&dir, &shim) {
                Some(native) => Ok(native.display().to_string()),
                None => anyhow::bail!(
                    "found only npm's {} (a batch script, which cannot be started safely); set COPILOT_CANVAS_OPENCODE to opencode.exe",
                    shim.display()
                ),
            };
        }
    }
    anyhow::bail!("opencode was not found on PATH (set COPILOT_CANVAS_OPENCODE to override)")
}

/// The native `opencode.exe` npm's shim in `dir` runs: the package's
/// `bin\opencode.exe`, else an `.exe` the shim names relative to its folder.
fn shim_target(dir: &Path, shim: &Path) -> Option<PathBuf> {
    let package = dir.join("node_modules").join("opencode-ai");
    let mut candidates = vec![package.join("bin").join("opencode.exe"), package.join("node_modules").join("opencode-windows-x64").join("bin").join("opencode.exe")];
    if let Ok(text) = std::fs::read_to_string(shim) {
        for quoted in text.split('"') {
            let Some(relative) = quoted.strip_prefix("%dp0%\\").or_else(|| quoted.strip_prefix("%~dp0\\")) else { continue };
            if relative.contains('%') || relative.contains("..") {
                continue;
            }
            let target = dir.join(relative);
            candidates.push(if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) { target } else { target.with_extension("exe") });
        }
    }
    candidates.into_iter().find(|p| p.is_file() && !super::is_batch(p))
}

/// A TUI server the canvas started. Its port and password stay in this
/// process: they are not put in the TUI's environment, which every tool the
/// agent runs would inherit.
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
    /// The folder the TUI works in (`GET /path`), once it answered.
    pub directory: Option<String>,
}

struct Slot {
    server: Server,
    state: ServerState,
    /// A watcher thread polls this server.
    watched: bool,
    /// Process id of the terminal's program (the TUI), once it started.
    pid: Option<u32>,
}

static SERVERS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

fn servers_lock() -> std::sync::MutexGuard<'static, Vec<Slot>> {
    SERVERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remembers a TUI server (replacing any earlier one of that terminal).
pub fn register_server(server: Server) {
    opencode_server::resume(server.port);
    let mut servers = servers_lock();
    servers.retain(|s| s.server.terminal != server.terminal);
    let sessions = server.session.iter().cloned().collect();
    servers.push(Slot { server, state: ServerState { sessions, ..Default::default() }, watched: false, pid: None });
}

/// The program of canvas terminal `terminal` (its TUI) runs as `pid`: only
/// listeners inside that process tree get the server's password.
pub fn set_terminal_pid(terminal: &str, pid: u32) {
    if let Some(slot) = servers_lock().iter_mut().find(|s| s.server.terminal == terminal && s.pid.is_none()) {
        slot.pid = Some(pid);
    }
}

/// Who listens on a TUI's server port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listener {
    /// Nobody yet (the TUI is starting).
    None,
    /// Only processes of the TUI's terminal.
    Ours,
    /// Some other process (the port was taken before the TUI bound it).
    Foreign(Vec<u32>),
}

/// The decision behind [`listener_of`]: `owners` listen on the port, `tree`
/// is the terminal's process tree.
pub fn classify_listener(owners: &[u32], tree: &[u32]) -> Listener {
    let foreign: Vec<u32> = owners.iter().copied().filter(|pid| !tree.contains(pid)).collect();
    match (owners.is_empty(), foreign.is_empty()) {
        (true, _) => Listener::None,
        (false, true) => Listener::Ours,
        (false, false) => Listener::Foreign(foreign),
    }
}

/// Who listens on `port` relative to the process tree of `root`. The port was
/// chosen by binding port 0 and releasing it, so another process could have
/// taken it before the TUI did: the password goes only to the TUI's own tree.
pub fn listener_of(port: u16, root: u32) -> Result<Listener> {
    let owners = canvas_common::tcp::listener_owners(port).context("cannot list TCP listeners")?;
    if owners.is_empty() {
        return Ok(Listener::None);
    }
    let entries = crate::win::process_entries();
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for (pid, parent, _) in &entries {
        children.entry(*parent).or_default().push(*pid);
    }
    Ok(classify_listener(&owners, &crate::win::process_tree(root, &children)))
}

/// Messages for the service log (each said once per server).
static NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Messages for the service log since the last call.
pub fn take_notes() -> Vec<String> {
    std::mem::take(&mut *NOTES.lock().unwrap_or_else(|e| e.into_inner()))
}

fn note(message: String) {
    let mut notes = NOTES.lock().unwrap_or_else(|e| e.into_inner());
    if notes.len() < 50 {
        notes.push(message);
    }
}

/// Forgets the TUI server of canvas terminal `terminal` (its watcher stops
/// at its next round). The service calls this when the terminal exits.
pub fn unregister_terminal(terminal: &str) {
    servers_lock().retain(|s| s.server.terminal != terminal);
}

/// The TUI in canvas terminal `terminal` is asked to quit: stop polling its
/// server and wait until no request is in flight (see `opencode_server::quiesce`).
pub fn quiesce_terminal(terminal: &str) {
    if let Some(server) = server(terminal) {
        opencode_server::quiesce(server.port, Duration::from_millis(800));
        unregister_terminal(terminal);
    }
}

/// The server registered for canvas terminal `terminal`.
pub fn server(terminal: &str) -> Option<Server> {
    servers_lock().iter().find(|s| s.server.terminal == terminal).map(|s| s.server.clone())
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

/// The service decided that the TUI in `terminal` created `session` (a first
/// turn too quick to be seen busy): it counts as open there.
pub fn claim_session(terminal: &str, session: &str) {
    if let Some(slot) = servers_lock().iter_mut().find(|s| s.server.terminal == terminal) {
        slot.state.sessions.insert(session.to_owned());
    }
}

/// Starts a watcher thread for every registered server that has none.
pub fn watch_servers() {
    let mut servers = servers_lock();
    for slot in servers.iter_mut().filter(|s| !s.watched) {
        let server = slot.server.clone();
        slot.watched = std::thread::Builder::new().name("opencode-server".into()).spawn(move || watch(server)).is_ok();
    }
}

/// Clears its server's `watched` flag when a watcher ends (however it ends),
/// so `watch_servers` can start another while the server is registered.
struct Watching(Server);

impl Drop for Watching {
    fn drop(&mut self) {
        if let Some(slot) = servers_lock().iter_mut().find(|s| s.server == self.0) {
            slot.watched = false;
        }
    }
}

/// Polls one server until it stops answering for good. No request (and so
/// no password) goes out until the port's listener is known to belong to the
/// TUI's own process tree; after a failed request that is checked again.
fn watch(server: Server) {
    let _watching = Watching(server.clone());
    // Every request is scoped to the TUI's folder (see `opencode_server::live_with`).
    let mut directory: Option<String> = None;
    let mut trusted = false;
    // (a foreign listener was logged, a failed check was logged)
    let mut noted = (false, false);
    loop {
        let pid = servers_lock().iter().find(|s| s.server == server).and_then(|s| s.pid);
        if !trusted {
            match pid.map(|pid| listener_of(server.port, pid)) {
                Some(Ok(Listener::Ours)) => trusted = true,
                Some(Ok(Listener::Foreign(owners))) if !noted.0 => {
                    noted.0 = true;
                    note(format!(
                        "opencode: port {} of terminal {} is held by process(es) {owners:?} outside its process tree; the server password is not sent there",
                        server.port, server.terminal
                    ));
                }
                Some(Err(error)) if !noted.1 => {
                    noted.1 = true;
                    note(format!("opencode: cannot check who listens on port {} of terminal {}: {error:#}", server.port, server.terminal));
                }
                _ => {}
            }
        }
        if trusted && directory.is_none() {
            directory = opencode_server::directory(server.port, &server.password).ok();
        }
        // The folder's session list is not asked for: it holds other processes' sessions too.
        let result = match directory.as_deref() {
            Some(dir) if trusted => opencode_server::live_with(server.port, &server.password, dir, false),
            _ if !trusted => Err(anyhow::anyhow!("the TUI's server is not listening (or not verified) yet")),
            _ => Err(anyhow::anyhow!("the TUI's server has not answered yet")),
        };
        let ok = result.is_ok();
        // Whoever listens may have changed (the TUI exited): check again first.
        trusted &= ok;
        {
            let mut servers = servers_lock();
            let Some(slot) = servers.iter_mut().find(|s| s.server == server) else { return };
            let now = now_ms();
            match result {
                Ok(live) => {
                    let state = &mut slot.state;
                    state.ok_ms = now;
                    state.directory.clone_from(&directory);
                    // Only what this process works on: busy or waiting here. A new
                    // session in the same folder may be another process's (see `claim_session`).
                    state.sessions.extend(live.status.keys().cloned());
                    state.sessions.extend(live.waiting().map(str::to_owned));
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

/// `overlay` merged into the JSON value of `name` the child would otherwise
/// see: the service's environment, overridden by `env` (integration, then
/// launch entries; a `-NAME` entry removes it). Malformed or non-object values count as `{}`.
fn merged_env(name: &str, env: &[(String, String)], overlay: Value) -> String {
    let removed = format!("-{name}");
    let mut text = std::env::var(name).ok();
    for (key, value) in env {
        if *key == name {
            text = Some(value.clone());
        } else if *key == removed && value.is_empty() {
            text = None;
        }
    }
    let mut base = text.and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    merge(&mut base, overlay);
    base.to_string()
}

/// Sets `name` to the merged value, replacing every earlier entry for it.
fn set_merged(env: &mut Vec<(String, String)>, name: &str, overlay: Value) {
    let value = merged_env(name, env, overlay);
    let removed = format!("-{name}");
    env.retain(|(key, _)| *key != name && *key != removed);
    env.push((name.to_owned(), value));
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
        let program = self.program()?;
        let native = launch.native_id.as_deref().filter(|n| is_session_id(n)).or_else(|| is_session_id(id).then_some(id));
        let mut args: Vec<String> = Vec::new();
        let mut rules = serde_json::Map::new();
        let mut config = json!({});
        let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));
        // Free text never parses as an option (yargs would read `--prompt -x` as two options).
        let positional = super::positional;
        let resumed = if launch.resume { Some(native.context("resuming an OpenCode session needs its session id (ses_…)")?) } else { None };
        if let Some(parent) = launch.fork_from.as_deref() {
            anyhow::ensure!(is_session_id(parent), "refusing to fork an invalid OpenCode session id");
        }
        let headless = launch.headless_prompt.is_some();
        if let Some(prompt) = launch.headless_prompt.as_deref() {
            // Passive run: one turn, never asks; the terminal shows the text output.
            push(&["run", &positional(prompt)]);
            if let Some(session) = resumed {
                push(&["-s", session]);
            } else if let Some(parent) = launch.fork_from.as_deref() {
                push(&["-s", parent, "--fork"]);
            } else if let Some(name) = launch.name.as_deref().filter(|n| !n.trim().is_empty()) {
                push(&["--title", &positional(name.trim())]);
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
                push(&["--prompt", &positional(prompt)]);
            }
            if launch.allow_all {
                push(&["--auto"]);
            }
        }
        // A resumed or forked TUI takes the model of the session's last message
        // itself (over any `-m`), so `-m` goes to new sessions and to every
        // headless run only.
        let model = if headless || (resumed.is_none() && launch.fork_from.is_none()) { &launch.model } else { &None };
        for (name, value) in [("-m", model), ("--agent", &launch.custom_agent)] {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                args.extend([name.to_owned(), super::option_value(name, value)?.to_owned()]);
            }
        }
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
        for plugin in &integration.skill_plugins {
            // A canvas-wide agent's skills (merging appends to the list).
            merge(&mut config, json!({ "skills": { "paths": [path_text(&plugin.join("skills"))] } }));
        }
        let mut env = integration.env.clone();
        env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()));
        let mut server = None;
        if !headless {
            // The credentials reach OpenCode only; the canvas keeps them in its registry.
            let port = free_port()?;
            let password = uuid::Uuid::new_v4().simple().to_string();
            args.extend(["--port".to_owned(), port.to_string()]);
            env.extend([
                ("OPENCODE_SERVER_USERNAME".to_owned(), opencode_server::USERNAME.to_owned()),
                ("OPENCODE_SERVER_PASSWORD".to_owned(), password.clone()),
            ]);
            server = Some(Server { terminal: id.to_owned(), port, password, session: resumed.map(str::to_owned), registered_ms: now_ms() });
        }
        env.extend(launch.env.iter().cloned());
        // The canvas settings are merged into what the launch and integration set, not replaced by it.
        if config.as_object().is_some_and(|c| !c.is_empty()) {
            set_merged(&mut env, "OPENCODE_CONFIG_CONTENT", config);
        }
        if !rules.is_empty() {
            set_merged(&mut env, "OPENCODE_PERMISSION", json!({ "external_directory": rules }));
        }
        if register && let Some(server) = server {
            register_server(server);
        }
        Ok(Command { program, args, env })
    }
}
impl Provider for OpenCode {
    fn kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    fn caps(&self) -> Caps {
        AgentKind::OpenCode.caps()
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
        unregister_terminal(id);
    }

    fn terminal_stopping(&self, id: &str) {
        quiesce_terminal(id);
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
        let mut warnings = std::mem::take(&mut self.warnings);
        warnings.extend(take_notes());
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SES: &str = "ses_e7fbbd911ffeOynBIm3IrWFsUl";
    const PARENT: &str = "ses_0bd2fb5f8ffezRuY9i1WNnJajX";

    fn integration() -> Integration {
        Integration { plugin_dir: Some(PathBuf::from(r"C:\h\plugin")), env: vec![("COPILOT_CANVAS_SESSION".into(), "t1".into())], ..Default::default() }
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
        assert_eq!(env(&cmd, "OPENCODE_SERVER_PASSWORD").unwrap().len(), 32);
        assert!(port > 0);
        assert!(!cmd.env.iter().any(|(k, _)| k.starts_with("COPILOT_CANVAS_OPENCODE")), "no server credentials for the agent's tools");
        assert_eq!(env(&cmd, "OPENCODE_DISABLE_AUTOUPDATE"), Some("1"));
        let config: Value = serde_json::from_str(env(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(config["skills"]["paths"], json!(["C:/h/plugin/skills"]));
        assert_eq!(cmd.env[0], ("COPILOT_CANVAS_SESSION".into(), "t1".into()));

        let resume = AgentLaunch { resume: true, native_id: Some(SES.into()), prompt: None, allow_all: false, model: None, ..new.clone() };
        assert_eq!(OpenCode.command_with("t1", &resume, &integration(), false).unwrap().args[..3], ["-s", SES, "--port"]);
        // The session's recorded model is not passed: OpenCode restores the model of its last message.
        let recorded = AgentLaunch { model: Some("github-copilot/claude-opus-5.5".into()), ..resume.clone() };
        let args = OpenCode.command_with("t1", &recorded, &integration(), false).unwrap().args;
        assert!(!args.iter().any(|a| a == "-m"), "{args:?}");
        let forked = OpenCode.command_with("t1", &AgentLaunch { resume: false, fork_from: Some(PARENT.into()), ..recorded.clone() }, &integration(), false).unwrap().args;
        assert!(!forked.iter().any(|a| a == "-m"), "a fork continues with its parent's model: {forked:?}");
        assert!(OpenCode.command_with("t1", &AgentLaunch { native_id: None, ..resume.clone() }, &integration(), false).is_err(), "no session id");
        assert_eq!(OpenCode.command_with(SES, &AgentLaunch { native_id: None, ..resume.clone() }, &integration(), false).unwrap().args[..2], ["-s", SES]);

        let fork = AgentLaunch { fork_from: Some(PARENT.into()), ..resume.clone() };
        assert_eq!(OpenCode.command_with("t1", &AgentLaunch { resume: false, ..fork.clone() }, &integration(), false).unwrap().args[..3], ["-s", PARENT, "--fork"]);
        assert!(OpenCode.command_with("t1", &AgentLaunch { fork_from: Some("x".into()), ..fork }, &integration(), false).is_err());

        let passive = AgentLaunch { headless_prompt: Some("check".into()), group_folder: Some(r"C:\g".into()), ..resume };
        let cmd = OpenCode.command_with("t1", &passive, &integration(), false).unwrap();
        assert_eq!(cmd.args, ["run", "check", "-s", SES, "--auto"]);
        let with_model = OpenCode.command_with("t1", &AgentLaunch { model: Some("opencode/x-free".into()), ..passive.clone() }, &integration(), false).unwrap().args;
        assert_eq!(with_model, ["run", "check", "-s", SES, "--auto", "-m", "opencode/x-free"], "headless runs always carry their model");
        assert!(env(&cmd, "OPENCODE_SERVER_PASSWORD").is_none(), "no server for passive runs");
        let rules: Value = serde_json::from_str(env(&cmd, "OPENCODE_PERMISSION").unwrap()).unwrap();
        assert_eq!(rules["external_directory"]["C:/g/*"], "allow");
        let first = AgentLaunch { resume: false, native_id: None, ..passive };
        assert_eq!(OpenCode.command_with("t1", &first, &integration(), false).unwrap().args, ["run", "check", "--title", "Fix", "--auto"]);
        // A canvas-wide agent's skills: their folder joins the canvas skills.
        let skilled = Integration { skill_plugins: vec![PathBuf::from(r"C:\h\canvas-agents\c\agents\a\plugin")], ..integration() };
        let cmd = OpenCode.command_with("t1", &first, &skilled, false).unwrap();
        let config: Value = serde_json::from_str(env(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(config["skills"]["paths"], json!(["C:/h/plugin/skills", "C:/h/canvas-agents/c/agents/a/plugin/skills"]));
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
        unregister_terminal("reg-a");
        assert!(server_sessions("reg-a").is_empty());
    }

    #[test]
    fn the_server_lives_in_the_registry_not_the_environment() {
        setup();
        let terminal = format!("reg-{}", uuid::Uuid::new_v4().simple());
        let launch = AgentLaunch { agent: "opencode".into(), ..Default::default() };
        let cmd = OpenCode.command_with(&terminal, &launch, &integration(), true).unwrap();
        let registered = server(&terminal).expect("registered");
        assert_eq!(cmd.args[cmd.args.len() - 1], registered.port.to_string());
        assert_eq!(env(&cmd, "OPENCODE_SERVER_PASSWORD"), Some(registered.password.as_str()));
        assert!(!cmd.env.iter().any(|(k, v)| k.starts_with("COPILOT_CANVAS_OPENCODE") || (k != "OPENCODE_SERVER_PASSWORD" && v.contains(&registered.password))));
        unregister_terminal(&terminal);
        assert!(server(&terminal).is_none());
    }

    #[test]
    fn launch_and_integration_config_is_merged_not_replaced() {
        setup();
        let mut integration = integration();
        integration.env.push(("OPENCODE_PERMISSION".into(), r#"{"bash":"ask"}"#.into()));
        let launch = AgentLaunch {
            agent: "opencode".into(),
            mcp_off: vec!["github".into()],
            add_dirs: vec![r"C:\extra".into()],
            env: vec![("OPENCODE_CONFIG_CONTENT".into(), r#"{"model":"x/y","mcp":{"mine":{"type":"local"}}}"#.into())],
            ..Default::default()
        };
        let cmd = OpenCode.command_with("t1", &launch, &integration, false).unwrap();
        for key in ["OPENCODE_CONFIG_CONTENT", "OPENCODE_PERMISSION"] {
            assert_eq!(cmd.env.iter().filter(|(k, _)| k == key).count(), 1, "{key} set once");
        }
        let config: Value = serde_json::from_str(env(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!((config["model"].as_str(), config["mcp"]["mine"]["type"].as_str()), (Some("x/y"), Some("local")), "the launch's config stays");
        assert_eq!(config["mcp"]["github"], json!({ "enabled": false }), "the canvas overlay applies");
        let rules: Value = serde_json::from_str(env(&cmd, "OPENCODE_PERMISSION").unwrap()).unwrap();
        assert_eq!((rules["bash"].as_str(), rules["external_directory"]["C:/extra"].as_str()), (Some("ask"), Some("allow")));
        // A removal entry means nothing inherited; malformed values count as empty.
        let env = vec![("OPENCODE_PERMISSION".to_owned(), "{not json".to_owned()), ("-OPENCODE_CONFIG_CONTENT".to_owned(), String::new())];
        assert_eq!(merged_env("OPENCODE_PERMISSION", &env, json!({ "a": 1 })), r#"{"a":1}"#);
        assert_eq!(merged_env("OPENCODE_CONFIG_CONTENT", &env, json!({ "b": 2 })), r#"{"b":2}"#);
    }

    #[test]
    fn prompts_titles_and_option_values_cannot_inject_options() {
        setup();
        let launch = AgentLaunch { agent: "opencode".into(), prompt: Some("-v x".into()), ..Default::default() };
        assert_eq!(OpenCode.command_with("t1", &launch, &integration(), false).unwrap().args[..2], ["--prompt", " -v x"]);
        let passive = AgentLaunch { agent: "opencode".into(), headless_prompt: Some("--help".into()), name: Some("-t".into()), ..Default::default() };
        assert_eq!(OpenCode.command_with("t1", &passive, &integration(), false).unwrap().args, ["run", " --help", "--title", " -t", "--auto"]);
        for bad in [AgentLaunch { model: Some("-x".into()), ..launch.clone() }, AgentLaunch { custom_agent: Some("--evil".into()), ..launch.clone() }, AgentLaunch { model: Some("a\u{7}b".into()), ..launch }] {
            assert!(OpenCode.command_with("t1", &bad, &integration(), false).is_err());
        }
    }

    #[test]
    fn npm_shims_are_resolved_to_the_binary_or_refused() {
        let root = std::env::temp_dir().join(format!("cc-oc-cli-{}", uuid::Uuid::new_v4()));
        let npm = root.join("npm");
        std::fs::create_dir_all(&npm).unwrap();
        std::fs::write(npm.join("opencode.cmd"), "@ECHO off\r\n\"%_prog%\"  \"%dp0%\\node_modules\\opencode-ai\\bin\\opencode\" %*\r\n").unwrap();
        let path = std::env::join_paths([&npm]).unwrap();
        let error = find_cli_in(None, path.clone()).unwrap_err().to_string();
        assert!(error.contains("COPILOT_CANVAS_OPENCODE") && error.contains("opencode.cmd"), "{error}");
        let bin = npm.join("node_modules").join("opencode-ai").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("opencode.exe"), "").unwrap();
        assert_eq!(find_cli_in(None, path.clone()).unwrap(), bin.join("opencode.exe").display().to_string());
        for script in [r"C:\x\opencode.cmd", r"C:\x\opencode.Bat"] {
            let error = find_cli_in(Some(script.into()), path.clone()).unwrap_err().to_string();
            assert!(error.contains("batch script") && error.contains("COPILOT_CANVAS_OPENCODE"), "{error}");
        }
        assert_eq!(find_cli_in(Some(r"C:\x\opencode.exe".into()), path).unwrap(), r"C:\x\opencode.exe");
        // The shim's own target when the package layout differs.
        let other = root.join("other");
        std::fs::create_dir_all(other.join("pkg")).unwrap();
        std::fs::write(other.join("opencode.cmd"), "\"%dp0%\\pkg\\oc.exe\" %*").unwrap();
        std::fs::write(other.join("pkg").join("oc.exe"), "").unwrap();
        assert_eq!(find_cli_in(None, std::env::join_paths([&other]).unwrap()).unwrap(), other.join("pkg").join("oc.exe").display().to_string());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_password_only_goes_to_listeners_in_the_terminals_process_tree() {
        let tree = [10, 11, 12];
        assert_eq!(classify_listener(&[], &tree), Listener::None, "not listening yet: wait");
        assert_eq!(classify_listener(&[11], &tree), Listener::Ours);
        assert_eq!(classify_listener(&[11, 12], &tree), Listener::Ours, "several listeners of the TUI (IPv4 and IPv6)");
        assert_eq!(classify_listener(&[11, 99], &tree), Listener::Foreign(vec![99]), "one foreign listener is enough to refuse");
        assert_eq!(classify_listener(&[99], &[]), Listener::Foreign(vec![99]));
        // A real listener: this process owns it; a tree without this process does not.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(listener_of(port, std::process::id()).unwrap(), Listener::Ours);
        let mut other = std::process::Command::new("cmd.exe").args(["/d", "/c", "ping -n 5 127.0.0.1 >nul"]).spawn().unwrap();
        assert_eq!(listener_of(port, other.id()).unwrap(), Listener::Foreign(vec![std::process::id()]));
        drop(listener);
        assert_eq!(listener_of(port, std::process::id()).unwrap(), Listener::None);
        let _ = crate::win::kill_tree(other.id());
        let _ = other.wait();
    }

    #[test]
    fn a_watcher_never_sends_the_password_to_a_foreign_listener() {
        // A "server" this process owns, registered for a terminal whose program is another process.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut other = std::process::Command::new("cmd.exe").args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]).spawn().unwrap();
        let terminal = format!("foreign-{}", uuid::Uuid::new_v4().simple());
        register_server(Server { terminal: terminal.clone(), port, password: "secret".into(), session: None, registered_ms: now_ms() });
        set_terminal_pid(&terminal, other.id());
        watch_servers();
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock), "no connection was made");
        let notes = take_notes();
        assert!(notes.iter().any(|n| n.contains(&terminal) && n.contains("not sent")), "{notes:?}");
        unregister_terminal(&terminal);
        let _ = crate::win::kill_tree(other.id());
        let _ = other.wait();
    }

    /// A stand-in for a TUI's server: answers every request by path prefix.
    fn fake_server(routes: Vec<(&'static str, String)>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text.split_whitespace().nth(1).unwrap_or_default().to_owned();
                let body = routes.iter().find(|(prefix, _)| path.starts_with(prefix)).map(|(_, body)| body.clone()).unwrap_or_else(|| "[]".into());
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
            }
        });
        port
    }

    #[test]
    fn a_tui_owns_only_the_sessions_it_works_on_or_was_given() {
        let terminal = format!("own-{}", uuid::Uuid::new_v4().simple());
        let (mine, other) = (format!("ses_mine{}", &terminal[4..12]), format!("ses_other{}", &terminal[4..12]));
        let port = fake_server(vec![
            ("/path", r#"{"directory":"C:\\work"}"#.into()),
            ("/session/status", format!(r#"{{"{mine}":{{"type":"busy"}}}}"#)),
            // The folder's list holds another process's new session too.
            ("/session?", format!(r#"[{{"id":"{other}","time":{{"created":{}}}}},{{"id":"{mine}","time":{{"created":{}}}}}]"#, now_ms(), now_ms())),
        ]);
        register_server(Server { terminal: terminal.clone(), port, password: "pw".into(), session: None, registered_ms: now_ms().saturating_sub(1_000) });
        set_terminal_pid(&terminal, std::process::id());
        watch_servers();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while servers().iter().all(|(s, state)| s.terminal != terminal || state.ok_ms == 0) {
            assert!(std::time::Instant::now() < deadline, "the watcher never reached the server");
            std::thread::sleep(Duration::from_millis(50));
        }
        let state = servers().into_iter().find(|(s, _)| s.terminal == terminal).unwrap().1;
        assert_eq!(state.directory.as_deref(), Some(r"C:\work"));
        assert_eq!(server_sessions(&terminal), std::slice::from_ref(&mine), "busy here: its own; listed in the folder: not");
        assert!(OpenCode.is_live(&mine) && !OpenCode.is_live(&other), "the other process's session may be resumed");
        claim_session(&terminal, &other);
        assert!(OpenCode.is_live(&other), "a session the service gave this TUI is open there");
        unregister_terminal(&terminal);
    }

    #[test]
    fn a_watcher_clears_its_flag_when_it_ends() {
        let terminal = format!("watch-{}", uuid::Uuid::new_v4().simple());
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        register_server(Server { terminal: terminal.clone(), port: closed, password: "p".into(), session: None, registered_ms: now_ms() });
        let watched = || servers_lock().iter().find(|s| s.server.terminal == terminal).map(|s| s.watched);
        watch_servers();
        assert_eq!(watched(), Some(true));
        // A guard of an ended watcher (here: dropped by hand) clears the flag.
        drop(Watching(server(&terminal).unwrap()));
        assert_eq!(watched(), Some(false));
        unregister_terminal(&terminal);
    }
}
