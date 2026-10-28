//! The L3 canvas backend inside the service process ("agent supervisor"):
//! the session-history index, sessions started inside terminals, the event
//! bus and passive sessions. The terminals themselves belong to the L1 host
//! (`canvas_host::server`); this module plugs into it as its [`HostApp`].

mod factory_watch;
mod passive;
pub use canvas_host::{keys, term};

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use canvas_host::server::{self, Client, Host, HostApp, Resolved};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::index::is_session_id;
use crate::paths::Instance;
use crate::agents::{AgentKind, Integration};
use crate::protocol::{AgentLaunch, ClientMsg, LaunchKind, ServiceMsg, SessionSummary};
pub use canvas_common::auth::{read_token, write_token};
pub use server::log;
use term::lock;

/// The backend's state; dereferences to the terminal host it runs in.
pub struct Agents {
    host: Arc<Host>,
    sessions: Mutex<HashMap<String, SessionSummary>>,
    index_wake: Sender<()>,
    /// Sessions started inside a hosted terminal: child id → message.
    subsessions: Mutex<HashMap<String, ServiceMsg>>,
    /// Groups and passive sessions from the canvas, and the bus state.
    routing: Mutex<crate::bus::Routing>,
    bus: Mutex<crate::bus::BusState>,
    bus_tx: Sender<crate::bus::Event>,
    /// When each terminal last got keyboard input (messages wait while you type).
    input_at: Mutex<HashMap<String, u64>>,
    /// Sessions each client wants indexed (its canvas members).
    tracked: Mutex<HashMap<u64, HashSet<String>>>,
}

/// Passive sessions and the factory watch use the backend under this name.
type Shared = Agents;

impl Deref for Agents {
    type Target = Host;
    fn deref(&self) -> &Host {
        &self.host
    }
}

pub fn run(instance: Instance) -> Result<()> {
    server::run(instance, |host| Agents::start(host.clone()))
}

impl Agents {
    fn start(host: Arc<Host>) -> Arc<dyn HostApp> {
        let instance = host.instance.clone();
        crate::agents::copilot::set_sessions_dir(instance.sessions_dir.clone());
        let (index_tx, index_rx) = bounded::<()>(1);
        let (bus_tx, bus_rx) = unbounded::<crate::bus::Event>();
        let agents = Arc::new(Agents {
            host,
            sessions: Mutex::new(HashMap::new()),
            index_wake: index_tx,
            subsessions: Mutex::new(HashMap::new()),
            routing: Mutex::new(passive::load_routing(&instance.home)),
            bus: Mutex::new(passive::load_state(&instance.home)),
            bus_tx,
            input_at: Mutex::new(HashMap::new()),
            tracked: Mutex::new(HashMap::new()),
        });
        server::spawn_named("index", { let a = agents.clone(); move || index_loop(a, index_rx) });
        server::spawn_named("subsessions", { let a = agents.clone(); move || subsession_loop(a) });
        // The skills and the `copilot-canvas` command that canvas sessions use for events.
        if let Err(error) = crate::cli::write_plugin(&instance.home) {
            log(&instance, &format!("cannot write the canvas plugin: {error:#}"));
        }
        if let Err(error) = crate::cli::install_cli(&instance.home) {
            log(&instance, &format!("cannot install the copilot-canvas command: {error:#}"));
        }
        server::spawn_named("bus", { let a = agents.clone(); move || passive::bus_loop(a, bus_rx) });
        agents
    }
}

impl Agents {
    /// The canvas integration every session gets: the skills plugin, its
    /// identity and the `copilot-canvas` command on PATH.
    pub fn integration(&self, id: &str) -> Integration {
        let home = &self.instance.home;
        let plugin = home.join("plugin");
        Integration {
            plugin_dir: plugin.is_dir().then_some(plugin),
            env: vec![
                ("COPILOT_CANVAS_HOME".to_owned(), home.display().to_string()),
                ("COPILOT_CANVAS_SESSION".to_owned(), id.to_owned()),
                ("PATH".to_owned(), format!("{};{}", home.join("cli").display(), std::env::var("PATH").unwrap_or_default())),
            ],
        }
    }

    fn resolve_agent(&self, id: &str, launch: AgentLaunch) -> Result<Resolved> {
        let kind = AgentKind::parse(&launch.agent).with_context(|| format!("unknown agent {:?}", launch.agent))?;
        let provider = kind.provider();
        let session = launch.native_id.clone().unwrap_or_else(|| id.to_owned());
        // Never become a second writer for a session running elsewhere.
        anyhow::ensure!(
            !launch.resume || !provider.is_live(&session),
            "this session is already running in another terminal; use that terminal"
        );
        // Structured launches carry no free-form arguments; the environment
        // must not impersonate another session.
        anyhow::ensure!(
            !launch.env.iter().any(|(k, _)| k.trim_start_matches('-').eq_ignore_ascii_case("COPILOT_CANVAS_SESSION")),
            "launch options may not select a different session"
        );
        let command = provider.command(id, &launch, &self.integration(id))?;
        Ok(Resolved { program: command.program, args: command.args, env: command.env, agent: true })
    }
}

impl HostApp for Agents {
    fn resolve_launch(&self, id: &str, kind: LaunchKind) -> Result<Resolved> {
        let (new_name, prompt, resume, extra, env) = match kind {
            LaunchKind::Copilot { new_name, prompt, resume } => (new_name, prompt, resume, Vec::new(), Vec::new()),
            LaunchKind::CopilotWith { new_name, prompt, resume, args, env } => (new_name, prompt, resume, args, env),
            LaunchKind::Agent(launch) => return self.resolve_agent(id, *launch),
            LaunchKind::Command { .. } => anyhow::bail!("plain commands are launched by the host"),
        };
        anyhow::ensure!(is_session_id(id), "refusing to launch an invalid session id");
        // Never become a second writer for a session running elsewhere.
        anyhow::ensure!(
            !session_live(&self.instance.sessions_dir, id),
            "this session is already running in another terminal; use that terminal"
        );
        anyhow::ensure!(
            !extra.iter().any(|a| a == "--session-id" || a == "--resume" || a == "-r" || a == "--continue"),
            "launch options may not select a different session"
        );
        let program = copilot_program()?;
        // No --mouse flag: Copilot saves it to the user's global settings.
        let mut args = vec!["--session-id".to_owned(), id.to_owned()];
        if !resume && let Some(name) = new_name.filter(|n| !n.trim().is_empty()) {
            args.extend(["--name".to_owned(), name]);
        }
        // A first prompt also applies to a forked subsession (resumed from its copy).
        if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
            args.extend(["-i".to_owned(), prompt]);
        }
        args.extend(extra);
        Ok(Resolved { program, args, env, agent: true })
    }

    fn client_connected(&self, client: &Arc<Client>) {
        let sessions: Vec<_> = lock(&self.sessions).values().cloned().collect();
        client.send(ServiceMsg::Sessions { full: true, upsert: sessions, remove: Vec::new() });
        for link in lock(&self.subsessions).values() {
            client.send(link.clone());
        }
        client.send(ServiceMsg::Bus { json: passive::snapshot(self) });
    }

    fn client_disconnected(&self, client: u64) {
        lock(&self.tracked).remove(&client);
        let _ = self.index_wake.try_send(());
    }

    fn message(&self, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
        match message {
            ClientMsg::Track { ids } => {
                lock(&self.tracked).insert(client.id, ids.into_iter().filter(|id| crate::agents::ALL.iter().any(|k| k.provider().is_session_id(id))).collect());
                let _ = self.index_wake.try_send(());
            }
            ClientMsg::RefreshSessions => {
                let _ = self.index_wake.try_send(());
            }
            ClientMsg::Routing { json } => {
                let routing: crate::bus::Routing = serde_json::from_str(&json).context("invalid routing")?;
                let mut current = lock(&self.routing);
                if *current != routing {
                    passive::save_routing(&self.instance.home, &routing);
                    *current = routing;
                }
            }
            ClientMsg::Bus { json } => passive::action(self, &json, &self.bus_tx),
            other => anyhow::bail!("unexpected message {other:?}"),
        }
        Ok(())
    }

    fn input_noted(&self, id: &str) {
        passive::input_noted(self, id);
    }

    fn terminals_changed(&self) {
        let _ = self.index_wake.try_send(());
    }

    fn has_work(&self) -> bool {
        passive::has_work(self)
    }
}

/// True when a live process (not a recycled PID) holds the session's in-use lock.
pub fn session_live(root: &Path, id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join(id)) else { return false };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        name.to_string_lossy()
            .strip_prefix("inuse.")
            .and_then(|rest| rest.strip_suffix(".lock"))
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(|pid| crate::win::lock_owner_alive(pid, &e.path()))
    })
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

fn copilot_program() -> Result<String> {
    if let Some(custom) = std::env::var_os("COPILOT_CANVAS_COPILOT") {
        return Ok(custom.to_string_lossy().into_owned());
    }
    find_on_path("copilot.exe")
        .map(|p| p.to_string_lossy().into_owned())
        .context("copilot.exe was not found on PATH (set COPILOT_CANVAS_COPILOT to override)")
}

/// Copilot processes inside a terminal's process tree that are a session's
/// own CLI process: `copilot.exe` started by something else (the shim) or by
/// that shim (the real CLI). Deeper `copilot.exe` are the CLI's workers.
pub fn nested_copilots(tree: &[u32], procs: &HashMap<u32, (u32, String)>) -> Vec<u32> {
    let is_copilot = |pid: u32| procs.get(&pid).is_some_and(|(_, name)| name == "copilot.exe");
    let parent = |pid: u32| procs.get(&pid).map(|(p, _)| *p).unwrap_or(0);
    tree.iter()
        .copied()
        .filter(|&pid| is_copilot(pid) && (!is_copilot(parent(pid)) || !is_copilot(parent(parent(pid)))))
        .collect()
}

/// The session whose in-use lock this process holds.
pub fn session_for_pid(root: &Path, pid: u32) -> Option<String> {
    let lock_name = format!("inuse.{pid}.lock");
    std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| is_session_id(name))
        .find(|name| root.join(name).join(&lock_name).exists())
}

/// Watches hosted terminals for Copilot sessions started inside them (an agent
/// running `copilot -p …`, `copilot --resume …`) and reports each as a
/// subsession of the terminal's session. Copilot records no parent itself.
fn subsession_loop(shared: Arc<Agents>) {
    // pid → (attempts, resolved session); the lock file appears shortly after start.
    let mut checked: HashMap<u32, (u32, Option<String>)> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let terms: Vec<(String, u32)> =
            lock(&shared.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        if terms.is_empty() {
            checked.clear();
            continue;
        }
        let entries = crate::win::process_entries();
        let procs: HashMap<u32, (u32, String)> = entries.iter().map(|(pid, parent, name)| (*pid, (*parent, name.clone()))).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, parent, _) in &entries {
            children.entry(*parent).or_default().push(*pid);
        }
        let mut alive = HashSet::new();
        for (parent_id, root) in terms {
            let tree = crate::win::process_tree(root, &children);
            for pid in nested_copilots(&tree, &procs) {
                alive.insert(pid);
                let state = checked.entry(pid).or_insert((0, None));
                if state.1.is_some() || state.0 >= 15 {
                    continue;
                }
                state.0 += 1;
                let Some(child) = session_for_pid(&shared.instance.sessions_dir, pid) else { continue };
                state.1 = Some(child.clone());
                if child == parent_id || lock(&shared.subsessions).contains_key(&child) {
                    continue;
                }
                let yaml = std::fs::read_to_string(shared.instance.sessions_dir.join(&child).join("workspace.yaml")).unwrap_or_default();
                let field = |key: &str| {
                    yaml.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim().trim_matches(['"', '\'']).to_owned()).unwrap_or_default()
                };
                let message = ServiceMsg::Subsession { parent: parent_id.clone(), child: child.clone(), name: field("name:"), cwd: field("cwd:") };
                log(&shared.instance, &format!("session {child} started inside {parent_id}"));
                lock(&shared.subsessions).insert(child, message.clone());
                shared.broadcast(message);
                let _ = shared.index_wake.try_send(());
            }
        }
        checked.retain(|pid, _| alive.contains(pid));
    }
}

fn index_loop(shared: Arc<Agents>, wake: Receiver<()>) {
    let mut histories: Vec<Box<dyn crate::agents::History>> = crate::agents::ALL.iter().map(|kind| kind.provider().history()).collect();
    loop {
        let tracked: HashSet<String> = lock(&shared.tracked)
            .values()
            .flat_map(|ids| ids.iter().cloned())
            .chain(lock(&shared.terms).values().filter(|t| t.copilot).map(|t| t.id.clone()))
            .collect();
        let (mut changed, mut removed) = (Vec::new(), Vec::new());
        for history in histories.iter_mut() {
            let (c, r) = history.poll(&tracked, Duration::from_secs(15));
            changed.extend(c);
            removed.extend(r);
            for (session, kind, data) in history.take_detected() {
                log(&shared.instance, &format!("detected {kind} in {session}"));
                let _ = shared.bus_tx.send(crate::bus::Event { kind, source: session, data, ..Default::default() });
            }
            for warning in history.take_warnings() {
                log(&shared.instance, &warning);
            }
        }
        if !changed.is_empty() || !removed.is_empty() {
            {
                let mut sessions = lock(&shared.sessions);
                for summary in &changed {
                    sessions.insert(summary.id.clone(), summary.clone());
                }
                for id in &removed {
                    sessions.remove(id);
                }
            }
            shared.broadcast(ServiceMsg::Sessions { full: false, upsert: changed, remove: removed });
        }
        let _ = wake.recv_timeout(Duration::from_secs(1));
        // Coalesce bursts of wake-ups.
        std::thread::sleep(Duration::from_millis(30));
        while wake.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_copilots_are_session_processes_not_workers() {
        let procs: HashMap<u32, (u32, String)> = [
            (1, (0, "copilot-canvas-service.exe")),
            (10, (1, "copilot.exe")),  // the terminal's shim
            (11, (10, "copilot.exe")), // its real CLI
            (12, (11, "copilot.exe")), // a worker
            (20, (11, "pwsh.exe")),
            (21, (20, "copilot.exe")), // `copilot -p` run by the agent (shim)
            (22, (21, "copilot.exe")), // its real CLI (holds the lock)
            (23, (22, "copilot.exe")), // its worker
            (24, (20, "node.exe")),
        ]
        .into_iter()
        .map(|(pid, (parent, name))| (pid, (parent, name.to_owned())))
        .collect();
        let tree = [10, 11, 12, 20, 21, 22, 23, 24];
        assert_eq!(nested_copilots(&tree, &procs), vec![10, 11, 21, 22]);
    }

    #[test]
    fn session_for_pid_finds_the_lock_holder() {
        let root = std::env::temp_dir().join(format!("cc-lock-{}", uuid::Uuid::new_v4()));
        let id = "12345678-1234-1234-1234-123456789abc";
        std::fs::create_dir_all(root.join(id)).unwrap();
        std::fs::write(root.join(id).join("inuse.4242.lock"), "").unwrap();
        assert_eq!(session_for_pid(&root, 4242).as_deref(), Some(id));
        assert_eq!(session_for_pid(&root, 4243), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
