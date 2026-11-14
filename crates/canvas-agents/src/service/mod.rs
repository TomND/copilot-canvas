//! The L3 canvas backend inside the service process ("agent supervisor"):
//! the session-history index, sessions started inside terminals, the event
//! bus and passive sessions. The terminals themselves belong to the L1 host
//! (`canvas_host::server`); this module plugs into it as its [`HostApp`].

mod docs;
mod factory_watch;
mod inbox;
mod passive;
mod prs;
mod reviews;
mod push;
pub mod restore;
mod todos;
pub use canvas_host::{keys, term};

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use canvas_host::server::{self, Client, Host, HostApp, Resolved};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::agents::{AgentKind, Integration, Provide};
use crate::backend::Ctx;
use crate::index::is_session_id;
use crate::paths::{Instance, now_ms};
use crate::protocol::{AgentLaunch, CAP_TUNNEL_ACCESS, ClientMsg, LaunchKind, ServiceMsg, SessionSummary};
pub use canvas_sys::token::{read_token, write_token};
pub use server::log;
use term::lock;

/// Threads answering backend requests (`ClientMsg::Rpc`): slow ones (CLI
/// runs, scans), bulky reads (transcripts, files, note folders) and quick
/// ones (file checks, listings) each on their own.
const RPC_WORKERS: usize = 8;
const BULKY_RPC_WORKERS: usize = 3;
const QUICK_RPC_WORKERS: usize = 4;
/// A client whose edits keep being refused gets the whole canvas back at most this often.
const DOC_REFUSAL_GAP_MS: u64 = 5_000;

/// A client's refused edits of one canvas: when it last got the whole
/// canvas back, and the refusals since (owed: sent by `docs_loop` once the
/// gap is over, never dropped, R2-C-05).
#[derive(Default)]
struct Refusals {
    last_ms: u64,
    owed: u32,
    /// The latest refusal's reason.
    error: String,
}

impl Refusals {
    /// A refusal now: `Some(refused since the last answer)` when the whole
    /// canvas goes back now; else it is owed.
    fn refused(&mut self, now: u64, error: String) -> Option<u32> {
        if now.saturating_sub(self.last_ms) >= DOC_REFUSAL_GAP_MS {
            let since = std::mem::take(&mut self.owed);
            self.last_ms = now;
            return Some(since);
        }
        self.owed += 1;
        self.error = error;
        None
    }

    /// An answer owed and due now: (the refusals it covers, the latest reason).
    fn due(&mut self, now: u64) -> Option<(u32, String)> {
        if self.owed == 0 || now.saturating_sub(self.last_ms) < DOC_REFUSAL_GAP_MS {
            return None;
        }
        self.last_ms = now;
        Some((std::mem::take(&mut self.owed), std::mem::take(&mut self.error)))
    }
}
/// What a client that went away tracked stays tracked this long.
const TRACK_GRACE_MS: u64 = 120_000;

/// A backend request waiting for a worker.
struct RpcJob {
    client: Arc<Client>,
    id: u64,
    /// The request, or why its JSON is not one this build knows.
    request: Result<canvas_model::rpc::Request, String>,
}

/// A terminal whose agent session is not known yet: OpenCode creates its
/// session on the first prompt (a fork at start).
#[derive(Clone, Debug, PartialEq)]
struct PendingSession {
    launched_ms: u64,
    /// The agent's folder (asked from the TUI's server; known up front for passive runs).
    cwd: Option<String>,
    /// A fork: its session exists from the start.
    fork: bool,
    /// Sessions the TUI's own server reported busy or waiting (proof they are its own).
    busy: HashSet<String>,
}

/// What the service knows about a launch's agent session up front.
#[derive(Clone, Debug, PartialEq)]
enum SessionPlan {
    /// The agent names it later (OpenCode's first prompt, or its fork at start).
    Expect { fork: bool },
    /// A resumed session whose id differs from the terminal's.
    Known(String),
    /// The terminal id is the session id (Copilot, Claude Code), or the caller
    /// learns it (a passive OpenCode run, from its folder).
    Terminal,
}

fn session_plan(kind: AgentKind, launch: &AgentLaunch, id: &str, session: &str) -> SessionPlan {
    let fork = launch.fork_from.is_some();
    if kind == AgentKind::OpenCode && launch.headless_prompt.is_none() && (!launch.resume || fork) {
        return SessionPlan::Expect { fork };
    }
    if launch.resume && !fork && session != id {
        return SessionPlan::Known(session.to_owned());
    }
    SessionPlan::Terminal
}

/// The plugin with a launch's skill folders (`None`: nothing to give), made
/// under `root` as `launch`, and the service log's line about it: what it got
/// and what it left out (names the agent already loads from the project of
/// `cwd`, repeated names, links).
#[cfg(test)]
fn group_skill_plugin(root: &Path, launch: &str, id: &str, kind: AgentKind, cwd: &Path, skills: &canvas_model::launch::GroupSkills) -> (Option<PathBuf>, String) {
    let native = canvas_model::skills::native_skill_names(kind, cwd);
    match canvas_model::skills::prepare_group_plugin(root, &skills.group, launch, &skills.dirs, &native) {
        Ok(made) => (made.dir.clone(), group_skill_line(id, skills, &made, "")),
        Err(error) => (None, format!("terminal {id}: cannot prepare the skill folders of {}: {error}", group_label(skills))),
    }
}

fn group_label(skills: &canvas_model::launch::GroupSkills) -> String {
    if skills.group.is_empty() { "the canvas".to_owned() } else { format!("group {}", skills.group) }
}

/// The service log's line about a launch's group skill plugin.
fn group_skill_line(id: &str, skills: &canvas_model::launch::GroupSkills, made: &canvas_model::skills::GroupPlugin, how: &str) -> String {
    let given = if made.given.is_empty() { "none".to_owned() } else { made.given.join(", ") };
    let skipped = if made.skipped.is_empty() { String::new() } else { format!("; skipped {}", made.skipped.join(", ")) };
    format!("terminal {id}: skill folders of {} ({} folder(s)){how}: {given}{skipped}", group_label(skills), skills.dirs.len())
}

/// What tells whether skill folders changed: the names in them and the size
/// and time of their files (a bounded walk, no file read). A folder's own
/// time is left out: what changes it (a name added or removed) is in the
/// names, and on Windows the time a listing gives for a folder is brought up
/// to date lazily (listing the folder itself does it), so it moved between
/// two walks with nothing changed and the plugin was built again each time.
/// For the same reason a file's size and time are asked of the file itself
/// there, not taken from the listing.
fn skills_stamp(dirs: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut seen = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (PathBuf::from(d), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            dir.hash(&mut hasher);
            continue;
        };
        let mut items: Vec<(PathBuf, bool)> = entries.flatten().filter_map(|e| e.file_type().ok().map(|t| (e.path(), t.is_dir()))).collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, is_dir) in items {
            seen += 1;
            if seen > canvas_model::skills::MAX_VISITS * 4 {
                return hasher.finish();
            }
            if is_dir {
                (&path, true).hash(&mut hasher);
                if depth < 6 {
                    stack.push((path, depth + 1));
                }
            } else {
                // Links are not followed (the plugin builder skips them too).
                let meta = std::fs::symlink_metadata(&path).ok();
                (&path, meta.as_ref().map(|m| m.len()), meta.and_then(|m| m.modified().ok())).hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// A group skill plugin built for an earlier launch, reused while its
/// inputs are the same and its folders unchanged (S8-03: every launch and
/// resume scanned and copied every skill again).
struct CachedPlugin {
    stamp: u64,
    made: canvas_model::skills::GroupPlugin,
    /// When a launch last took it (the least recently used go beyond `MAX_CACHED_PLUGINS`).
    used_ms: u64,
}

/// Group skill plugins kept for later launches, at most.
const MAX_CACHED_PLUGINS: usize = 16;

/// Group skill plugins by terminal (and when each was registered).
type GroupPlugins = Mutex<HashMap<String, (PathBuf, u64)>>;

/// The plugin kept for `key` while its folders' `stamp` is the same,
/// registered for terminal `id` before the cache's lock is released.
fn take_cached_plugin(plugins: &GroupPlugins, cache: &Mutex<HashMap<String, CachedPlugin>>, key: &str, stamp: u64, id: &str) -> Option<canvas_model::skills::GroupPlugin> {
    let mut plugins = lock(plugins);
    let mut cache = lock(cache);
    let cached = cache.get_mut(key).filter(|c| c.stamp == stamp && c.made.dir.as_ref().is_none_or(|d| d.is_dir()))?;
    cached.used_ms = now_ms();
    if let Some(dir) = &cached.made.dir {
        plugins.insert(id.to_owned(), (dir.clone(), now_ms()));
    }
    Some(cached.made.clone())
}

/// Keeps a plugin just built for `key` (registered for terminal `id` under
/// the same locks); answers the plugins it replaced or pushed out, for the
/// caller to release.
fn keep_built_plugin(plugins: &GroupPlugins, cache: &Mutex<HashMap<String, CachedPlugin>>, key: String, stamp: u64, id: &str, made: &canvas_model::skills::GroupPlugin) -> Vec<PathBuf> {
    let mut plugins = lock(plugins);
    let mut cache = lock(cache);
    if let Some(dir) = &made.dir {
        plugins.insert(id.to_owned(), (dir.clone(), now_ms()));
    }
    let mut gone: Vec<PathBuf> = cache.insert(key, CachedPlugin { stamp, made: made.clone(), used_ms: now_ms() }).and_then(|o| o.made.dir).into_iter().collect();
    while cache.len() > MAX_CACHED_PLUGINS {
        let Some(oldest) = cache.iter().min_by_key(|(_, c)| c.used_ms).map(|(k, _)| k.clone()) else { break };
        gone.extend(cache.remove(&oldest).and_then(|o| o.made.dir));
    }
    gone
}

/// A terminal has the plugin, or it is the one kept for the next launch.
fn plugin_in_use(plugins: &GroupPlugins, cache: &Mutex<HashMap<String, CachedPlugin>>, plugin: &Path) -> bool {
    let plugins = lock(plugins);
    plugins.values().any(|(p, _)| p == plugin) || lock(cache).values().any(|c| c.made.dir.as_deref() == Some(plugin))
}

/// The group skill plugin to remove when terminal `id` (started at
/// `started`, when known) exits: the one registered for it, unless it was
/// made after that terminal started (a relaunch of the id owns it then).
fn take_exited_plugin(plugins: &mut HashMap<String, (PathBuf, u64)>, id: &str, started: Option<u64>) -> Option<PathBuf> {
    let (_, made) = plugins.get(id)?;
    if started.is_some_and(|started| *made > started) {
        return None;
    }
    plugins.remove(id).map(|(plugin, _)| plugin)
}

/// The environment entry that removes the skill folders' variable from
/// what an agent inherits ("-NAME" with an empty value: `Terminal::spawn`).
fn no_group_skills_env() -> Vec<(String, String)> {
    canvas_model::launch::GROUP_SKILLS_NAMES.iter().map(|name| (format!("-{name}"), String::new())).collect()
}

/// A plain Copilot launch's environment without the skill folders'
/// variable (in any case): it travels only in agent launches.
fn without_group_skills(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.retain(|(k, _)| !canvas_model::launch::is_group_skills(k));
    env
}

/// The backend's state; dereferences to the terminal host it runs in.
pub struct Agents {
    host: Arc<Host>,
    sessions: Mutex<HashMap<String, SessionSummary>>,
    index_wake: Sender<()>,
    /// Sessions started inside a hosted terminal: child id → its link.
    subsessions: Mutex<HashMap<String, SubsessionLink>>,
    /// Groups and passive sessions from the canvas, and the bus state.
    routing: Mutex<crate::bus::Routing>,
    /// Numbers routing changes; the version last written to routing.json
    /// (written after the routing lock is released, a newer one wins).
    routing_version: std::sync::atomic::AtomicU64,
    routing_written: Mutex<u64>,
    bus: Mutex<crate::bus::BusState>,
    /// Windows' changes to the bus state since the bus loop took its copy
    /// (locked after `bus`, never before).
    bus_ops: Mutex<Vec<passive::BusOp>>,
    bus_tx: Sender<crate::bus::Event>,
    /// When each terminal last got keyboard input (messages wait while you type).
    input_at: Mutex<HashMap<String, u64>>,
    /// Sessions each client wants indexed (its canvas members).
    tracked: Mutex<HashMap<u64, HashSet<String>>>,
    /// What clients that went away tracked, until then (ms): a reconnect
    /// (a tunnel blip) finds their tails still read (S4-15, S5-12).
    lingering: Mutex<Vec<(u64, HashSet<String>)>>,
    /// The agent each terminal runs (for its exit hook).
    agent_terms: Mutex<HashMap<String, AgentKind>>,
    /// Terminals whose agent session is not known yet.
    pending_sessions: Mutex<HashMap<String, PendingSession>>,
    /// Terminal id → the agent's own session id, once learned (told to every client).
    agent_sessions: Mutex<HashMap<String, String>>,
    /// Terminal id → the skills plugin its next launch gets (a canvas-wide
    /// agent's run, set right before the launch).
    skill_plugins: Mutex<HashMap<String, PathBuf>>,
    /// Terminal id → the group skill plugin its latest launch got and when
    /// it was made (a plugin newer than an exiting terminal is a relaunch's).
    group_plugins: Mutex<HashMap<String, (PathBuf, u64)>>,
    /// Numbers each launch's group skill plugin folder.
    group_plugin_seq: std::sync::atomic::AtomicU64,
    /// Group skill plugins by what they were made of (group, folders, the
    /// project's own skill names): launches share one while it holds.
    skill_cache: Mutex<HashMap<String, CachedPlugin>>,
    /// Canvas documents clients opened.
    docs: docs::Docs,
    /// (client, document) → when a refused edit last got the whole canvas, and how many were refused since.
    doc_refusals: Mutex<HashMap<(u64, String), Refusals>>,
    /// Backend requests for the worker threads (slow, bulky, quick).
    rpc: (Sender<RpcJob>, Sender<RpcJob>, Sender<RpcJob>),
    /// Pairing codes and paired remote clients.
    remote: crate::remote::Registry,
    /// The reports inbox (`inbox.rs`).
    reports: Mutex<crate::reports::Store>,
    /// The routed canvas's tracked pull requests (`prs.rs`).
    prs: Mutex<prs::Hub>,
    /// The routed canvas's review follow-ups (`reviews.rs`).
    reviews: Mutex<reviews::Hub>,
    /// The routed canvas's todo boards (`todos.rs`).
    todos: Mutex<todos::Hub>,
    /// Held while the book's files are written (after the book's lock is
    /// released): one writer at a time, in the order of the changes.
    todos_writer: Mutex<()>,
    /// The last bus snapshot every client got (`ServiceMsg::Bus`), held
    /// while it is broadcast: a connecting client gets it without waiting
    /// for a pass or any store's lock, and never after a newer one (R2-C-02, R2-C-07).
    last_bus: Mutex<Option<String>>,
    /// Held while a bus snapshot is made and sent (the bus loop, windows'
    /// todo changes, a connecting client), so a snapshot never overtakes a
    /// newer one; what the last one every client got was made of.
    snapshots: Mutex<passive::Sent>,
    /// The Dev Tunnels access token phone clients get (`CAP_TUNNEL_ACCESS`).
    tunnel_access: Mutex<Option<crate::tunnel_access::Access>>,
    /// Asks the tunnel access thread to issue a token now.
    tunnel_wake: Sender<()>,
    /// Phones registered for notifications, the APNs key, what was notified.
    push: push::Push,
    /// The satellites' links and the VM boxes' terminals.
    vm_links: Arc<crate::vm::link::Links>,
    /// The running set, resumed after a restart (`restore.rs`).
    restore: restore::Restore,
    /// Sessions a typed message is being submitted to (its Enter follows the
    /// paste and is pressed again until the agent takes it): nothing else is
    /// typed into them meanwhile.
    submitting: Mutex<HashSet<String>>,
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
    // Before any thread starts: the user's login environment (agents, node,
    // gh on PATH; MAC-LINUX-PLAN §5.2) and room for many terminals.
    #[cfg(unix)]
    let resolved = canvas_sys::env::adopt_login_environment();
    let files = canvas_sys::process::raise_fd_limit();
    #[cfg(unix)]
    {
        let what = match &resolved.error {
            Some(error) => format!("login environment not read ({error})"),
            None if resolved.added.is_empty() => "login environment: nothing to add".to_owned(),
            None => format!("login environment: {} variable(s) added ({})", resolved.added.len(), resolved.added.join(", ")),
        };
        server::log(&instance, &format!("{what}; PATH {}; open files {files}", resolved.path));
    }
    #[cfg(windows)]
    let _ = files;
    // Copilot keeps its sessions under `COPILOT_HOME` when that is set (after
    // the login environment, which may set it): the history, statuses and
    // the live guard look there too (S5-10).
    let mut instance = instance;
    if let Some(dir) = crate::agents::copilot::sessions_dir_for(&instance.sessions_dir) {
        server::log(&instance, &format!("Copilot sessions in {} (COPILOT_HOME)", dir.display()));
        instance.sessions_dir = dir;
    }
    server::run(instance, |host| Agents::start(host.clone()))
}

impl Agents {
    fn start(host: Arc<Host>) -> Arc<dyn HostApp> {
        let instance = host.instance.clone();
        crate::agents::copilot::set_sessions_dir(instance.sessions_dir.clone());
        let (index_tx, index_rx) = bounded::<()>(1);
        let (bus_tx, bus_rx) = unbounded::<crate::bus::Event>();
        let (rpc_tx, rpc_rx) = unbounded::<RpcJob>();
        let (bulky_tx, bulky_rx) = unbounded::<RpcJob>();
        let (quick_tx, quick_rx) = unbounded::<RpcJob>();
        let (tunnel_tx, tunnel_rx) = bounded::<()>(1);
        let agents = Arc::new(Agents {
            host,
            sessions: Mutex::new(HashMap::new()),
            index_wake: index_tx,
            subsessions: Mutex::new(HashMap::new()),
            routing: Mutex::new(passive::load_routing(&instance.home)),
            routing_version: std::sync::atomic::AtomicU64::new(0),
            routing_written: Mutex::new(0),
            bus: Mutex::new(passive::load_state(&instance.home)),
            bus_ops: Mutex::new(Vec::new()),
            bus_tx,
            input_at: Mutex::new(HashMap::new()),
            tracked: Mutex::new(HashMap::new()),
            lingering: Mutex::new(Vec::new()),
            agent_terms: Mutex::new(HashMap::new()),
            pending_sessions: Mutex::new(HashMap::new()),
            agent_sessions: Mutex::new(HashMap::new()),
            skill_plugins: Mutex::new(HashMap::new()),
            group_plugins: Mutex::new(HashMap::new()),
            group_plugin_seq: std::sync::atomic::AtomicU64::new(0),
            skill_cache: Mutex::new(HashMap::new()),
            docs: docs::Docs::default(),
            doc_refusals: Mutex::new(HashMap::new()),
            rpc: (rpc_tx, bulky_tx, quick_tx),
            remote: crate::remote::Registry::new(&instance.home),
            reports: Mutex::new(crate::reports::Store::open(&instance.home, now_ms())),
            prs: Mutex::new(prs::Hub::default()),
            reviews: Mutex::new(reviews::Hub::default()),
            todos: Mutex::new(todos::Hub::default()),
            todos_writer: Mutex::new(()),
            last_bus: Mutex::new(None),
            snapshots: Mutex::new(passive::Sent::default()),
            tunnel_access: Mutex::new(None),
            tunnel_wake: tunnel_tx,
            push: push::Push::new(&instance.home),
            vm_links: crate::vm::link::Links::new(&instance.home, {
                let instance = instance.clone();
                Arc::new(move |line: String| log(&instance, &line))
            }),
            restore: restore::Restore::new(&instance.home, &instance.name),
            submitting: Mutex::new(HashSet::new()),
        });
        // Sessions found inside VM boxes' terminals in their guests: subsessions, as local ones (`subsession_loop`).
        agents.vm_links.on_subsession({
            let weak = Arc::downgrade(&agents);
            Arc::new(move |parent: &str, child: &str, name: &str, cwd: &str| {
                let Some(shared) = weak.upgrade() else { return };
                if lock(&shared.subsessions).contains_key(child) {
                    return;
                }
                let message = ServiceMsg::Subsession { parent: parent.to_owned(), child: child.to_owned(), name: name.to_owned(), cwd: cwd.to_owned() };
                log(&shared.instance, &format!("session {child} started inside {parent} (in its VM)"));
                shared.new_subsession(child, message);
                let _ = shared.index_wake.try_send(());
            })
        });
        server::spawn_named("vm-idle", { let a = agents.clone(); move || vm_idle_loop(a) });
        server::spawn_named("push", { let a = agents.clone(); move || push::push_loop(a) });
        server::spawn_named("tunnel-access", { let a = agents.clone(); move || tunnel_access_loop(a, tunnel_rx) });
        server::spawn_named("index", { let a = agents.clone(); move || index_loop(a, index_rx) });
        server::spawn_named("subsessions", { let a = agents.clone(); move || subsession_loop(a) });
        server::spawn_named("agent-sessions", { let a = agents.clone(); move || agent_session_loop(a) });
        server::spawn_named("docs", { let a = agents.clone(); move || docs_loop(a) });
        for (count, jobs) in [(RPC_WORKERS, &rpc_rx), (BULKY_RPC_WORKERS, &bulky_rx), (QUICK_RPC_WORKERS, &quick_rx)] {
            for _ in 0..count {
                let (ctx, jobs, owner) = (Ctx::new(&instance), jobs.clone(), instance.clone());
                server::spawn_named("rpc", move || rpc_worker(ctx, jobs, owner));
            }
        }
        // The skills and the `atelier` command that canvas sessions use for events.
        if let Err(error) = crate::cli::write_plugin(&instance.home) {
            log(&instance, &format!("cannot write the canvas plugin: {error:#}"));
        }
        if let Err(error) = crate::cli::install_cli(&instance.home) {
            log(&instance, &format!("cannot install the atelier command: {error:#}"));
        }
        // This service's copies of group skill folders: none of its terminals runs yet
        // (other instances sharing the home keep theirs).
        let _ = std::fs::remove_dir_all(canvas_model::skills::group_plugins_dir(&instance.home, &instance.name));
        server::spawn_named("bus", { let a = agents.clone(); move || passive::bus_loop(a, bus_rx) });
        server::spawn_named("reports", { let a = agents.clone(); move || inbox::run(a) });
        server::spawn_named("prs", { let a = agents.clone(); move || prs::run(a) });
        // What ran before a restart starts again (no client needed); then the running set is kept.
        server::spawn_named("restore", { let a = agents.clone(); move || restore::run(a) });
        // VM sessions: launches, the manager, the satellites' terminals and the thin lane (idle without a VM pack).
        server::spawn_named("vm", {
            let (home, log_instance, wake) = (instance.home.clone(), instance.clone(), agents.index_wake.clone());
            let (links, host) = (agents.vm_links.clone(), agents.host.clone());
            move || {
                crate::vm::run(
                    home,
                    move |line| log(&log_instance, &line),
                    move || {
                        let _ = wake.try_send(());
                    },
                    links,
                    move |id, remote| host.adopt_remote(id, 100, 30, true, remote),
                )
            }
        });
        agents
    }
}

/// `PATH` with `dir` first (`;` on Windows, `:` elsewhere).
fn path_with(dir: &Path) -> String {
    let rest = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(rest)).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| dir.display().to_string())
}

impl Agents {
    /// The canvas integration every session gets: the skills plugin, its
    /// identity and the `atelier` command on PATH.
    pub fn integration(&self, id: &str) -> Integration {
        let home = &self.instance.home;
        let plugin = home.join("plugin");
        Integration {
            plugin_dir: plugin.is_dir().then_some(plugin),
            // Under the names before the rename too (deprecated): the old
            // CLI name's alias and instructions written before the rename read them.
            env: [canvas_common::env::both("ATELIER_HOME", &home.display().to_string()), canvas_common::env::both("ATELIER_SESSION", id), vec![("PATH".to_owned(), path_with(&home.join("cli")))]].concat(),
            skill_plugins: Vec::new(),
        }
    }

    /// The skills plugin of a launch: the one a canvas-wide agent's run set
    /// for it, else (its chat opened from the canvas) the plugin of the
    /// canvas-wide agent whose box this is, built from the canvas library now.
    fn skill_plugin(&self, id: &str) -> Option<PathBuf> {
        if let Some(plugin) = lock(&self.skill_plugins).remove(id) {
            return Some(plugin);
        }
        let routing = lock(&self.routing).clone();
        let spec = routing.passive.iter().find(|p| p.canvas_wide && p.session_id == id)?;
        if !crate::doc::valid_canvas_id(&routing.canvas) {
            return None;
        }
        let dir = crate::canvas_wide::canvas_dir(&self.instance.home, &routing.canvas);
        let paths = crate::canvas_wide::agent_paths(&dir, &spec.slug);
        match crate::canvas_wide::prepare_plugin(&dir, &paths, &spec.slug, &spec.skills) {
            Ok((plugin, missing)) => {
                if !missing.is_empty() {
                    log(&self.instance, &format!("canvas agent {}: skills not in the canvas library: {}", spec.name, missing.join(", ")));
                }
                plugin
            }
            Err(error) => {
                log(&self.instance, &format!("canvas agent {}: cannot prepare its skills: {error}", spec.name));
                None
            }
        }
    }

    fn resolve_agent(&self, id: &str, mut launch: AgentLaunch, cwd: &str) -> Result<Resolved> {
        // Where it runs: a VM launch names its agent `vm:<kind>` and carries
        // its placement; anything else runs here, unless the canvas places
        // the box on a VM (an older client's launch: never started here).
        if let Some(placement) = canvas_model::launch::take_placement(&mut launch).map_err(|e| anyhow::anyhow!(e))? {
            return self.resolve_vm(id, launch, placement);
        }
        anyhow::ensure!(
            !self.docs.placement_of(id).is_some_and(|p| p.is_vm()),
            "this session runs on a VM: update this client to start it (nothing was started on this machine)"
        );
        let kind = AgentKind::parse(&launch.agent).with_context(|| format!("unknown agent {:?}", launch.agent))?;
        let provider = kind.provider();
        let session = launch.native_id.clone().unwrap_or_else(|| id.to_owned());
        // Skill folders travel in the environment; they never reach the agent.
        let group_skills = canvas_model::launch::take_group_skills(&mut launch);
        // Never become a second writer for a session running elsewhere (who has it is named).
        if launch.resume {
            // A passive run (headless, on the bus loop) never waits: it is retried later.
            self.refuse_live(id, kind, &session, launch.headless_prompt.is_none())?;
        }
        // Nor for one another box of this service runs or is starting right
        // now (before the agent wrote its own lock): claimed under one lock.
        if launch.fork_from.is_none()
            && let Err(other) = self.host.claim_launch_key(id, &format!("{}:{session}", kind.id()))
        {
            log(&self.instance, &format!("terminal {id}: {} session {session} already runs in terminal {other}; not started twice", kind.id()));
            let name = lock(&self.routing).names.get(&other).map(|n| format!("“{n}”")).unwrap_or_else(|| format!("terminal {other}"));
            anyhow::bail!("this session is already running in another box of this canvas ({name}): use that box");
        }
        // Structured launches carry no free-form arguments; the environment
        // must not impersonate another session.
        anyhow::ensure!(
            !launch.env.iter().any(|(k, _)| canvas_common::env::is(k.trim_start_matches('-'), "ATELIER_SESSION")),
            "launch options may not select a different session"
        );
        // A conversation that never got its first prompt has nothing to resume:
        // it starts now (Copilot's `--session-id` does either by itself).
        if launch.resume && launch.fork_from.is_none() && kind != AgentKind::Copilot && !provider.has_history(&session) {
            launch.resume = false;
        }
        let mut integration = self.integration(id);
        // Sessions and passive runs know their agent (`ATELIER_AGENT`).
        integration.env.extend(crate::agents::session_agent_env(kind));
        // The skill folders' variable never reaches the agent, also one this service
        // inherited (started inside an older service's session).
        integration.env.extend(no_group_skills_env());
        integration.skill_plugins.extend(self.skill_plugin(id));
        let skills = if integration.skill_plugins.is_empty() { "" } else { ", with its canvas skills" };
        // The previous launch's copy: that terminal has exited (a live one is never launched again).
        let root = canvas_model::skills::group_plugins_dir(&self.instance.home, &self.instance.name);
        let previous = lock(&self.group_plugins).remove(id);
        if let Some((previous, _)) = previous {
            self.release_group_plugin(&root, &previous);
        }
        // Built once and shared while its folders do not change; a late exit of
        // the previous launch never removes one another launch uses.
        // Registered for this terminal as it is taken (R2-C-12).
        let group_plugin = group_skills.and_then(|skills| {
            let (plugin, line) = self.cached_group_plugin(&root, id, kind, Path::new(cwd), &skills);
            log(&self.instance, &line);
            plugin
        });
        let group = if group_plugin.is_some() { ", with its group skills" } else { "" };
        integration.skill_plugins.extend(group_plugin.clone());
        let command = provider.command(id, &launch, &integration).inspect_err(|_| {
            if let Some(plugin) = &group_plugin {
                lock(&self.group_plugins).remove(id);
                self.release_group_plugin(&root, plugin);
            }
        })?;
        let how = if launch.headless_prompt.is_some() { "headless run" } else if launch.fork_from.is_some() { "fork" } else if launch.resume { "resume" } else { "new session" };
        // The model as the command line carries it (agents that keep a session's model get none on resume).
        let model = launch.model.as_deref().filter(|m| command.args.iter().any(|a| a == m)).map(|m| format!(", model {m}")).unwrap_or_default();
        log(&self.instance, &format!("terminal {id}: {} {how}{model}{skills}{group}", kind.id()));
        lock(&self.agent_terms).insert(id.to_owned(), kind);
        match session_plan(kind, &launch, id, &session) {
            SessionPlan::Expect { fork } => self.expect_session(id, None, fork),
            SessionPlan::Known(native) => {
                // A resumed session's id is known up front (events and status follow it).
                lock(&self.pending_sessions).remove(id);
                lock(&self.agent_sessions).insert(id.to_owned(), native);
            }
            SessionPlan::Terminal => {}
        }
        Ok(Resolved { program: command.program, args: command.args, env: command.env, agent: true, remote: None })
    }

    /// A VM box's launch: its intent is recorded and its terminal runs
    /// `atelier vm attach`, which claims a VM (the pack's hooks) and joins
    /// the agent in the guest; nothing is provisioned on the caller's thread.
    fn resolve_vm(&self, id: &str, mut launch: AgentLaunch, placement: canvas_model::vm::VmPlacement) -> Result<Resolved> {
        let home = &self.instance.home;
        let kind = AgentKind::parse(&launch.agent).with_context(|| format!("unknown agent {:?}", launch.agent))?;
        let folder = crate::vm::pack::configured(home).context("VM sessions are off on this machine (no VM pack is configured); nothing was started")?;
        let (pack, status) = crate::vm::pack::load(&folder);
        anyhow::ensure!(status.errors() == 0, "the VM pack has {} error(s) (`atelier vm pack check`); nothing was started", status.errors());
        anyhow::ensure!(pack.repo(&placement.repo).is_some(), "the VM pack has no repository {:?}; nothing was started", placement.repo);
        anyhow::ensure!(
            !launch.env.iter().any(|(k, _)| canvas_common::env::is(k.trim_start_matches('-'), "ATELIER_SESSION")),
            "launch options may not select a different session"
        );
        // This machine's folders are not in the guest: a satellite gets the
        // canvas's plugin, the profile bundle, the group's skills, notes and
        // instructions as synced roots (the hooks lane gets none of them).
        let group_skills = canvas_model::launch::take_group_skills(&mut launch);
        let satellite = pack.provider != crate::vm::pack::ProviderKind::Hooks;
        let group = launch.group_folder.clone().filter(|_| satellite).zip(launch.group_name.clone());
        // Made here for this box (its project's own skills are in the guest: none are left out for them).
        let group_plugin = group_skills.filter(|_| satellite).and_then(|skills| {
            let root = home.join("vm").join("group-skills");
            match canvas_model::skills::prepare_group_plugin(&root, &skills.group, id, &skills.dirs, &HashSet::new()) {
                Ok(made) => {
                    log(&self.instance, &format!("terminal {id}: skill folders for its VM: {}", if made.given.is_empty() { "none".to_owned() } else { made.given.join(", ") }));
                    made.dir
                }
                Err(error) => {
                    log(&self.instance, &format!("terminal {id}: cannot prepare the skill folders for its VM: {error}"));
                    None
                }
            }
        });
        let target = if satellite {
            crate::vm::target::ExecTarget::Vm(crate::vm::target::GuestRoots {
                plugin: home.join("plugin").is_dir(),
                profile: !pack.profile.include.is_empty() || !pack.paths.is_empty(),
                profile_instructions: !pack.paths.is_empty(),
                group: group.as_ref().map(|(_, name)| crate::vm::sync::group_slug(name)),
                group_skills: group_plugin.is_some(),
                mcp: placement.mcp.as_deref().is_some_and(|preset| pack.mcp.presets.get(preset).is_some_and(|servers| !servers.is_empty())),
            })
        } else {
            crate::vm::target::ExecTarget::Local
        };
        let (argv, env) = crate::vm::launch::guest_command(kind, id, &launch, &target)?;
        use canvas_model::vm::VmState;
        let mut intent = crate::vm::launch::load(home, id)?.unwrap_or_default();
        intent.r#box = id.to_owned();
        intent.agent = kind.id().to_owned();
        // A first launch, or one after its VM was deleted, failed or lost: the launcher claims a VM.
        if intent.sandbox.is_empty() || matches!(intent.state, VmState::Failed | VmState::Deleted | VmState::Lost) {
            // Resuming its conversation there: from the hub's mirror of the VM it ran in.
            let from = if launch.resume && !intent.sandbox.is_empty() { intent.sandbox.clone() } else { intent.resume_from.clone() };
            (intent.resume_from, intent.resume_session) = if launch.resume { (from, launch.native_id.clone().unwrap_or_else(|| id.to_owned())) } else { Default::default() };
            intent.placement = placement;
            intent.sandbox.clear();
            intent.satellite.clear();
            intent.state = VmState::Claiming;
            intent.error.clear();
            intent.note.clear();
            intent.attempts = 0;
            intent.pending.clear();
            intent.fence += 1;
            intent.queued_ms = now_ms();
        } else if matches!(intent.state, VmState::Suspended | VmState::Suspending) {
            intent.pending = "resume".into();
        }
        intent.argv = argv;
        (intent.group_folder, intent.group_name) = group.unwrap_or_default();
        intent.group_plugin = group_plugin.map(|p| p.display().to_string()).unwrap_or_default();
        intent.env = crate::vm::launch::guest_env(id, kind).into_iter().chain(env).collect();
        if intent.created_ms == 0 {
            intent.created_ms = now_ms();
        }
        crate::vm::launch::store(home, &mut intent)?;
        let how = if intent.sandbox.is_empty() { "on a new VM" } else { "in its VM" };
        log(&self.instance, &format!("terminal {id}: {} {} {how} (repository {})", kind.id(), if launch.resume { "resume" } else { "new session" }, intent.placement.repo));
        if pack.provider != crate::vm::pack::ProviderKind::Hooks {
            // The satellite's lane: the program runs in the guest; this terminal draws it and shows the launch's steps.
            return Ok(Resolved { program: String::new(), args: Vec::new(), env: Vec::new(), agent: true, remote: Some(self.vm_links.launch(id)) });
        }
        let cli = home.join("cli").join(canvas_sys::host::exe_name("atelier"));
        let program = if cli.is_file() { cli } else { std::env::current_exe()? };
        Ok(Resolved { program: program.display().to_string(), args: vec!["vm".into(), "attach".into(), "--box".into(), id.to_owned()], env: self.integration(id).env, agent: true, remote: None })
    }

    /// The group skill plugin of a launch: the one built before for the same
    /// group, folders and project skill names while the folders are
    /// unchanged, else a new one (which replaces it). It is registered for
    /// terminal `id` under the cache's lock: a concurrent launch that replaces
    /// it in the cache never removes it before this one could register it
    /// (R2-C-12; lock order: group_plugins, then skill_cache).
    fn cached_group_plugin(&self, root: &Path, id: &str, kind: AgentKind, cwd: &Path, skills: &canvas_model::launch::GroupSkills) -> (Option<PathBuf>, String) {
        let native = canvas_model::skills::native_skill_names(kind, cwd);
        let mut names: Vec<&String> = native.iter().collect();
        names.sort();
        let key = format!("{}\u{1f}{}\u{1f}{}", skills.group, skills.dirs.join("\u{1e}"), names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join("\u{1e}"));
        let stamp = skills_stamp(&skills.dirs);
        if let Some(made) = take_cached_plugin(&self.group_plugins, &self.skill_cache, &key, stamp, id) {
            return (made.dir.clone(), group_skill_line(id, skills, &made, ", unchanged since an earlier launch"));
        }
        let launch_key = format!("{}-{id}", self.group_plugin_seq.fetch_add(1, Ordering::Relaxed));
        match canvas_model::skills::prepare_group_plugin(root, &skills.group, &launch_key, &skills.dirs, &native) {
            Ok(made) => {
                let line = group_skill_line(id, skills, &made, "");
                let gone = keep_built_plugin(&self.group_plugins, &self.skill_cache, key, stamp, id, &made);
                for old in gone.iter().filter(|d| Some(*d) != made.dir.as_ref()) {
                    self.release_group_plugin(root, old);
                }
                (made.dir, line)
            }
            Err(error) => (None, format!("terminal {id}: cannot prepare the skill folders of {}: {error}", group_label(skills))),
        }
    }

    /// Removes a group skill plugin no launch shares any more (none of the
    /// terminals has it, and it is not the one kept for the next launch).
    fn release_group_plugin(&self, root: &Path, plugin: &Path) {
        if !plugin_in_use(&self.group_plugins, &self.skill_cache, plugin) {
            canvas_model::skills::remove_group_plugin(root, plugin);
        }
    }

    /// Learns the agent session `id` is about to create (see [`agent_session_loop`]).
    fn expect_session(&self, id: &str, cwd: Option<String>, fork: bool) {
        lock(&self.agent_sessions).remove(id);
        lock(&self.pending_sessions).insert(id.to_owned(), PendingSession { launched_ms: now_ms(), cwd, fork, busy: HashSet::new() });
    }

    /// The agent's own session id of a terminal (the terminal id unless learned otherwise).
    fn native_of(&self, id: &str) -> String {
        lock(&self.agent_sessions).get(id).cloned().unwrap_or_else(|| id.to_owned())
    }

    /// Records a learned agent session and tells every client.
    fn learned_session(&self, id: &str, native: &str) {
        lock(&self.pending_sessions).remove(id);
        lock(&self.agent_sessions).insert(id.to_owned(), native.to_owned());
        // Open in that TUI from now on (never resumed a second time elsewhere).
        crate::agents::opencode::claim_session(id, native);
        log(&self.instance, &format!("terminal {id} runs agent session {native}"));
        self.broadcast(ServiceMsg::AgentSession { id: id.to_owned(), native: native.to_owned() });
        let _ = self.index_wake.try_send(());
    }
}

/// Keeps a Dev Tunnels access token for phone clients: issued when one
/// connects without a fresh token, renewed half a day before it expires while
/// one stays connected, at most every 10 minutes after a failure.
fn tunnel_access_loop(agents: Arc<Agents>, wake: Receiver<()>) {
    const RETRY: Duration = Duration::from_secs(600);
    let mut failed_at: Option<Instant> = None;
    loop {
        let _ = wake.recv_timeout(RETRY);
        // Connected phones, and phones away that get tokens by notification.
        let wanted = lock(&agents.clients).values().any(|c| c.caps() & CAP_TUNNEL_ACCESS != 0) || push::wants_access(&agents);
        if !wanted || lock(&agents.tunnel_access).as_ref().is_some_and(|a| a.fresh(now_ms())) || failed_at.is_some_and(|at| at.elapsed() < RETRY) {
            continue;
        }
        match crate::tunnel_access::issue(&agents.instance.home) {
            Ok(access) => {
                failed_at = None;
                let until = canvas_common::paths::format_rfc3339_ms(access.expires_ms);
                log(&agents.instance, &format!("issued a Dev Tunnels access token for phone clients (until {until}, {})", if access.url.is_empty() { "web address unknown" } else { &access.url }));
                *lock(&agents.tunnel_access) = Some(access.clone());
                push::send_access(&agents, &access);
                agents.broadcast(ServiceMsg::TunnelAccess { url: access.url, token: access.token, expires_ms: access.expires_ms });
            }
            Err(error) => {
                failed_at = Some(Instant::now());
                log(&agents.instance, &format!("cannot issue a Dev Tunnels access token for phone clients: {error:#}"));
            }
        }
    }
}

impl Agents {
    /// Sends `client` every session summary, under the lock the index's
    /// broadcasts take: no change falls between the list and the updates.
    fn send_sessions(&self, client: &Client) {
        let sessions = lock(&self.sessions);
        client.send(ServiceMsg::Sessions { full: true, upsert: sessions.values().cloned().collect(), remove: Vec::new() });
    }
}

/// Writes routing.json for routing change `version` (taken under the
/// routing lock, written after it was released): a slow disk never holds up
/// launches or the bus, and an older change never overwrites a newer one.
fn routing_changed(shared: &Agents, version: u64, routing: &crate::bus::Routing) {
    let mut written = lock(&shared.routing_written);
    if *written < version {
        passive::save_routing(&shared.instance.home, routing);
        *written = version;
    }
}

impl HostApp for Agents {
    fn command_env(&self, id: &str) -> Vec<(String, String)> {
        // Plain shells (the demo's) get the `atelier` command and their identity too.
        self.integration(id).env
    }

    fn resolve_launch(&self, id: &str, kind: LaunchKind, cwd: &str) -> Result<Resolved> {
        self.restore.requested(id, &kind, cwd);
        let (new_name, prompt, resume, extra, env) = match kind {
            LaunchKind::Copilot { new_name, prompt, resume } => (new_name, prompt, resume, Vec::new(), Vec::new()),
            LaunchKind::CopilotWith { new_name, prompt, resume, args, env } => (new_name, prompt, resume, args, without_group_skills(env)),
            LaunchKind::Agent(launch) => return self.resolve_agent(id, *launch, cwd),
            LaunchKind::Command { .. } => anyhow::bail!("plain commands are launched by the host"),
        };
        anyhow::ensure!(is_session_id(id), "refusing to launch an invalid session id");
        // Never become a second writer for a session running elsewhere (who has it is named).
        self.refuse_live(id, AgentKind::Copilot, id, true)?;
        anyhow::ensure!(
            !extra.iter().any(|a| a == "--session-id" || a == "--resume" || a == "-r" || a == "--continue"),
            "launch options may not select a different session"
        );
        // Older clients' launches claim the session as agent launches do: two
        // boxes never start it at once, whichever kind of launch each sends (S4-12).
        if let Err(other) = self.host.claim_launch_key(id, &format!("{}:{id}", AgentKind::Copilot.id())) {
            log(&self.instance, &format!("terminal {id}: copilot session {id} already runs in terminal {other}; not started twice"));
            anyhow::bail!("this session is already running in another box of this canvas (terminal {other}): use that box");
        }
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
        Ok(Resolved { program, args, env, agent: true, remote: None })
    }

    fn client_connected(&self, client: &Arc<Client>) {
        // The client is listed already: every change after this list reaches it.
        self.send_sessions(client);
        // Subsession links again (a link sent to a connection that dropped
        // with it still queued, or to a window that had another canvas open,
        // was lost: R2-C-10; clients take a child already on the canvas as
        // it is), except those whose box you removed (S5-06).
        let links: Vec<ServiceMsg> = lock(&self.subsessions).values().filter_map(SubsessionLink::tell).collect();
        for link in links {
            client.send(link);
        }
        for (id, native) in lock(&self.agent_sessions).iter() {
            client.send(ServiceMsg::AgentSession { id: id.clone(), native: native.clone() });
        }
        passive::send_last_snapshot(self, client.as_ref());
        // Only clients that show the inbox get it (`CAP_REPORTS`).
        client.send(inbox::message(self));
        if client.caps() & CAP_TUNNEL_ACCESS != 0 {
            let now = now_ms();
            let access = lock(&self.tunnel_access).clone();
            if let Some(access) = access.as_ref().filter(|a| a.expires_ms == 0 || a.expires_ms > now) {
                client.send(ServiceMsg::TunnelAccess { url: access.url.clone(), token: access.token.clone(), expires_ms: access.expires_ms });
            }
            if !access.is_some_and(|a| a.fresh(now)) {
                let _ = self.tunnel_wake.try_send(());
            }
        }
    }

    fn client_disconnected(&self, client: u64) {
        let gone = lock(&self.tracked).remove(&client);
        if let Some(ids) = gone.filter(|ids| !ids.is_empty()) {
            lock(&self.lingering).push((now_ms() + TRACK_GRACE_MS, ids));
        }
        lock(&self.doc_refusals).retain(|(id, _), _| *id != client);
        self.docs.leave(client);
        let _ = self.index_wake.try_send(());
    }

    fn message(&self, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
        match message {
            ClientMsg::PushRegister { token, environment, bundle, label } => push::register(self, client, &token, &environment, &bundle, &label)?,
            ClientMsg::Track { ids } => {
                lock(&self.tracked).insert(client.id, ids.into_iter().filter(|id| crate::agents::ALL.iter().any(|k| k.provider().is_session_id(id))).collect());
                let _ = self.index_wake.try_send(());
            }
            // A full refresh (the host sent the terminals): what changed since comes as usual.
            ClientMsg::RefreshSessions => {
                self.send_sessions(client);
                let _ = self.index_wake.try_send(());
            }
            ClientMsg::Routing { json } => {
                let routing: crate::bus::Routing = serde_json::from_str(&json).context("invalid routing")?;
                let changed = {
                    let mut current = lock(&self.routing);
                    (*current != routing).then(|| {
                        *current = routing.clone();
                        self.routing_version.fetch_add(1, Ordering::SeqCst) + 1
                    })
                };
                // Written after the lock is released (launches and the bus take it).
                if let Some(version) = changed {
                    routing_changed(self, version, &routing);
                }
            }
            ClientMsg::Bus { json } => {
                // Inbox requests travel as bus requests (older services ignore them);
                // todo operations are applied at once, as you.
                let handled = serde_json::from_str::<serde_json::Value>(&json).is_ok_and(|value| inbox::action(self, &value) || todos::action(self, client, &value) || restore::action(self, &value));
                if !handled {
                    passive::action(self, &json, &self.bus_tx);
                }
            }
            ClientMsg::DocOpen { path, demo, cwd } => self.docs.open(&self.instance.home, path.as_deref(), demo, &cwd, client.id, |key, version, reply| {
                if let Some(error) = &reply.error {
                    log(&self.instance, &format!("cannot open the canvas {}: {error}", reply.path));
                }
                let json = serde_json::to_string(&reply).unwrap_or_default();
                client.send(ServiceMsg::Doc { key, version, json });
            }),
            ClientMsg::DocEdit { key, base: _, ops_json } => self.doc_edit(client, key, ops_json),
            ClientMsg::Rpc { id, json } => {
                // Read once here: the worker gets the request (or why it is not one).
                let request = serde_json::from_str::<canvas_model::rpc::Request>(&json).map_err(|e| e.to_string());
                if let Ok(request) = &request
                    && request.is_remote_admin()
                {
                    let response = self.remote_admin(client, request.clone());
                    client.send(rpc_reply(id, serde_json::to_string(&response).unwrap_or_default()));
                    return Ok(());
                }
                if let Ok(request) = &request
                    && request.is_service_state()
                {
                    // On a thread of its own: a delete checks the VM's repositories through its link.
                    let (home, links, client, request) = (self.instance.home.clone(), self.vm_links.clone(), client.clone(), request.clone());
                    server::spawn_named("vm-request", move || {
                        let git = |sandbox: &str| links.git_check(sandbox).ok();
                        let response = crate::vm::service_request_with(&home, request, &git);
                        client.send(rpc_reply(id, serde_json::to_string(&response).unwrap_or_default()));
                    });
                    return Ok(());
                }
                let queue = match &request {
                    Ok(r) if r.is_quick() && r.is_bulky() => &self.rpc.1,
                    Ok(r) if !r.is_quick() => &self.rpc.0,
                    _ => &self.rpc.2,
                };
                let _ = queue.send(RpcJob { client: client.clone(), id, request });
            }
            other => anyhow::bail!("unexpected message {other:?}"),
        }
        Ok(())
    }

    fn input_noted(&self, id: &str) {
        passive::input_noted(self, id);
    }

    fn terminals_changed(&self) {
        // OpenCode TUIs: their server's listener is checked against the program's process tree.
        let opencode: Vec<String> = lock(&self.agent_terms).iter().filter(|(_, k)| **k == AgentKind::OpenCode).map(|(id, _)| id.clone()).collect();
        for id in opencode {
            if let Some(pid) = lock(&self.terms).get(&id).filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).map(|t| t.pid) {
                crate::agents::opencode::set_terminal_pid(&id, pid);
            }
        }
        let _ = self.index_wake.try_send(());
    }

    fn terminal_stopping(&self, id: &str) {
        self.restore.stopping(id, self.shutting_down.load(Ordering::Acquire));
        let kind = lock(&self.agent_terms).get(id).copied();
        if let Some(kind) = kind {
            let started = std::time::Instant::now();
            kind.provider().terminal_stopping(id);
            log(&self.instance, &format!("stopping {id} ({}), prepared in {} ms", kind.id(), started.elapsed().as_millis()));
        }
    }

    fn terminal_exited(&self, id: &str) {
        // Already relaunched: what is registered now belongs to the new run
        // (a stop you asked of the earlier run is spent with it).
        if lock(&self.terms).get(id).is_some_and(|t| t.alive.load(Ordering::Acquire)) {
            self.restore.relaunched(id);
            return;
        }
        let forced = lock(&self.terms).get(id).is_some_and(|t| t.was_forced());
        let kept = self.restore.exited(id, forced, self.shutting_down.load(Ordering::Acquire), now_ms());
        if kept != "not recorded" {
            log(&self.instance, &format!("terminal {id}: restore entry {kept}"));
        }
        let kind = lock(&self.agent_terms).remove(id);
        if let Some(kind) = kind {
            kind.provider().terminal_exited(id, &self.instance.home);
        }
        // Its copy of the group's skill folders, unless a relaunch (prepared
        // after this terminal started) has made the one registered now.
        let started = lock(&self.terms).get(id).map(|t| t.started_ms);
        let taken = take_exited_plugin(&mut lock(&self.group_plugins), id, started);
        if let Some(plugin) = taken {
            self.release_group_plugin(&canvas_model::skills::group_plugins_dir(&self.instance.home, &self.instance.name), &plugin);
        }
    }

    fn has_work(&self) -> bool {
        passive::has_work(self) || crate::vm::has_work(&self.instance.home)
    }

    fn remote_credential(&self, id: &str) -> canvas_common::handshake::Credential {
        // A VM session's satellite (its own store; never a paired client).
        if crate::vm::store::is_satellite_id(id) {
            let credential = crate::vm::store::Satellites::new(&self.instance.home).credential(id);
            if !matches!(credential, canvas_common::handshake::Credential::Satellite(_)) {
                log(&self.instance, &format!("refused satellite {id:?}: unknown or revoked"));
            }
            return credential;
        }
        let credential = self.remote.credential(id);
        if !matches!(credential, canvas_common::handshake::Credential::Secret(_)) {
            log(&self.instance, &format!("refused remote client {id:?}: {}", self.remote.describe(id)));
        }
        credential
    }

    fn remote_seen(&self, id: &str) {
        self.remote.seen(id, now_ms());
    }

    fn remote_active(&self, id: &str) -> bool {
        self.remote.secret(id).is_some()
    }

    fn pairing(&self) -> Option<&dyn canvas_common::handshake::Pairing> {
        Some(self)
    }

    fn shutdown(&self) {
        self.restore.shutdown(now_ms());
        self.restore.save_now();
        for saved in self.docs.save(true) {
            if let Some(error) = saved.error {
                log(&self.instance, &format!("cannot save the canvas {}: {error}", saved.key));
            }
        }
    }

    fn serve_satellite(&self, link: server::SatelliteLink) -> Result<()> {
        self.vm_links.serve(link)
    }
}

impl Agents {
    /// Pairing codes, the client list and revocation (`atelier remote
    /// pair|clients|revoke`), for clients on the local pipe only.
    fn remote_admin(&self, client: &Arc<Client>, request: canvas_model::rpc::Request) -> canvas_model::rpc::Response {
        use canvas_model::rpc::{PairingCode, RemoteClient, Request, Response};
        if client.remote {
            return Response::Error("remote access is managed on the service's machine (`atelier remote …` there)".into());
        }
        let now = now_ms();
        let result = (|| -> Result<Response> {
            Ok(match request {
                Request::PairCode { name } => {
                    // Remote listening is off until a client is paired.
                    let home = &self.instance.home;
                    let mut settings = canvas_common::settings::Settings::load_checked(home)?;
                    let addr = match settings.listen() {
                        Some(addr) => addr,
                        None => {
                            let addr = format!("127.0.0.1:{}", canvas_common::remote::DEFAULT_PORT);
                            settings.remote.listen = Some(addr.clone());
                            settings.save(home).context("cannot turn on remote listening in settings.json")?;
                            addr
                        }
                    };
                    let port = self.host.listen_remote(&addr)?;
                    let who = self.requester(client);
                    let (code, expires_ms) = self.remote.open_code(name.clone(), &who, now)?;
                    log(&self.instance, &format!(
                        "PAIRING CODE OPENED by {who}{}; whoever enters it within {} minutes is paired",
                        name.as_deref().map(|n| format!(" for a client to be called {n:?}")).unwrap_or_default(),
                        canvas_common::pairing::CODE_TTL_MS / 60_000
                    ));
                    Response::Pairing(PairingCode { code: canvas_common::pairing::format_code(&code), expires_ms, listen: format!("127.0.0.1:{port}"), name })
                }
                Request::RemoteClients => {
                    // Every connection, its requests-only (upload) ones included (R3-A-01).
                    let open: Vec<(String, bool)> = self.host.remote_connections(None).iter().filter_map(|c| Some((c.remote_id.clone()?, c.requests_only))).collect();
                    Response::RemoteClients(
                        self.remote
                            .list()?
                            .into_iter()
                            .map(|c| RemoteClient {
                                connections: open.iter().filter(|(id, _)| *id == c.id).count() as u32,
                                uploads: open.iter().filter(|(id, only)| *id == c.id && *only).count() as u32,
                                id: c.id,
                                name: c.name,
                                created_ms: c.created_ms,
                                last_seen_ms: c.last_seen_ms,
                                revoked_ms: c.revoked_ms,
                                paired_by: c.paired_by,
                                code_opened_ms: c.code_opened_ms,
                            })
                            .collect(),
                    )
                }
                Request::RemoteRevoke { client: key } => {
                    let revoked = self.remote.revoke(&key, now)?;
                    self.push.devices.remove(Some(&revoked.id), None);
                    // Every connection of it, listed or not: its upload connection too (R3-A-01).
                    let closed = self.host.close_remote(Some(&revoked.id), "revoked");
                    log(&self.instance, &format!("revoked remote client {:?} ({:?}); closed {closed} connection(s)", revoked.id, revoked.name));
                    let mut text = format!("Revoked {} ({}); {closed} live connection(s) closed.", revoked.name, revoked.id);
                    match self.listen_only_while_needed() {
                        Ok(Some(off)) => text = format!("{text}\n{off}"),
                        Ok(None) => {}
                        Err(error) => text = format!("{text}\nRemote listening stays on: {error:#}"),
                    }
                    Response::Text(text)
                }
                other => anyhow::bail!("{} is not a remote-access request", other.name()),
            })
        })();
        result.unwrap_or_else(|error| Response::Error(format!("{error:#}")))
    }

    /// Applies a client's document edit and sends the patch to every client
    /// that has the document open; a refused edit resynchronizes its sender.
    fn doc_edit(&self, client: &Arc<Client>, key: String, ops_json: String) {
        let mut removed: Vec<String> = Vec::new();
        let applied = serde_json::from_str::<Vec<canvas_model::sync::Op>>(&ops_json).context("unreadable canvas edit").and_then(|ops| {
            removed = ops.iter().filter_map(|op| if let canvas_model::sync::Op::DelNode(id) = op { Some(id.clone()) } else { None }).collect();
            self.docs.edit(&key, client.id, &ops, |version, clients| {
                let targets: Vec<Arc<Client>> = lock(&self.clients).values().filter(|c| clients.contains(&c.id)).cloned().collect();
                for target in targets {
                    target.send(ServiceMsg::DocPatch { key: key.clone(), version, origin: client.id, ops_json: ops_json.clone() });
                }
            })
        });
        // A box removed while it waits for its turn in the restore is not
        // resumed (R2-C-01); a subsession box removed is never linked again (S5-06).
        if applied.is_ok() {
            for id in removed {
                if let Some(link) = lock(&self.subsessions).get_mut(&id) {
                    link.dismissed = true;
                }
                if self.restore.cancel(&id, "removed from the canvas before its turn") {
                    log(&self.instance, &format!("restore: {id} removed from the canvas before its turn; not resumed"));
                }
            }
        }
        if let Err(error) = applied {
            // A client that keeps sending the refused change (it puts it back
            // on top of the canvas it gets) gets the whole canvas and the
            // notice at most every few seconds, not ten times a second (S8-05);
            // one refused within that time gets them when it is over (R2-C-05).
            let error = format!("{error:#}");
            let due = lock(&self.doc_refusals).entry((client.id, key.clone())).or_default().refused(now_ms(), error.clone());
            if let Some(since) = due {
                self.resync_refused(client, &key, &error, since);
            }
        }
    }

    /// A client whose canvas edit was refused gets the whole canvas back
    /// (its refused change goes) and a notice; `more`: refusals not answered before.
    fn resync_refused(&self, client: &Client, key: &str, error: &str, more: u32) {
        let more = if more > 0 { format!(" ({more} more refused since the last notice)") } else { String::new() };
        log(&self.instance, &format!("refused a canvas edit from client {}: {error}{more}", client.id));
        self.docs.snapshot(key, client.id, |version, reply| {
            let json = serde_json::to_string(&reply).unwrap_or_default();
            client.send(ServiceMsg::Doc { key: key.to_owned(), version, json });
        });
        client.send(ServiceMsg::Error { id: None, message: format!("A canvas change was refused: {error}") });
    }
}

/// Pairing through the registry, with every outcome in the service log.
impl canvas_common::handshake::Pairing for Agents {
    fn wait(&self) -> Duration {
        self.remote.wait()
    }

    fn codes(&self) -> Vec<String> {
        self.remote.codes()
    }

    fn grant(&self, code: &str, name: &str) -> Result<canvas_common::handshake::PairGrant, String> {
        let result = self.remote.grant(code, name);
        match &result {
            Ok(grant) => {
                let record = self.remote.list().ok().and_then(|list| list.into_iter().find(|c| c.id == grant.client_id)).unwrap_or_default();
                log(&self.instance, &format!(
                    "PAIRED a new remote client {:?} named {:?} (it calls itself {name:?}) with the code opened by {} at {}",
                    grant.client_id,
                    record.name,
                    record.paired_by,
                    crate::paths::format_rfc3339_ms(record.code_opened_ms)
                ));
            }
            Err(reason) => log(&self.instance, &format!("pairing failed after the code matched: {reason}")),
        }
        result
    }

    fn failed(&self) {
        self.remote.failed();
        log(&self.instance, &format!("a pairing attempt failed (no open code matched); the next one waits {} ms", self.remote.wait().as_millis()));
    }
}

impl Agents {
    /// Who asked for something over the pipe: the hosted terminal whose
    /// process tree contains the client (a command typed in a canvas
    /// terminal, possibly by a remote client), else the local process.
    fn requester(&self, client: &Client) -> String {
        let Some(pid) = client.peer_pid else { return format!("client {} (process unknown)", client.id) };
        let entries = canvas_sys::process::process_entries();
        let exe = entries.iter().find(|(p, _, _)| *p == pid).map(|(_, _, name)| name.clone()).unwrap_or_default();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (child, parent, _) in &entries {
            children.entry(*parent).or_default().push(*child);
        }
        let terms: Vec<(String, u32)> = lock(&self.terms).values().filter(|t| t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        match terms.into_iter().find(|(_, root)| canvas_sys::process::process_tree(*root, &children).contains(&pid)) {
            Some((terminal, _)) => format!("a command in canvas terminal {terminal:?} ({exe} pid {pid})"),
            None => format!("a local process ({exe} pid {pid})"),
        }
    }

    /// Turns remote listening off (setting and listener) once no paired
    /// client is left and no code is open, unless `ATELIER_LISTEN` asks for it.
    fn listen_only_while_needed(&self) -> Result<Option<String>> {
        if self.remote.active_count() > 0 || self.remote.open_codes(now_ms()) > 0 || canvas_common::env::var_os("ATELIER_LISTEN").is_some_and(|v| !v.is_empty()) {
            return Ok(None);
        }
        let home = &self.instance.home;
        let mut settings = canvas_common::settings::Settings::load_checked(home)?;
        if settings.remote.listen.take().is_some() {
            settings.save(home).context("cannot turn off remote listening in settings.json")?;
        }
        let stopped = self.host.stop_remote();
        // No paired client is active: whatever connection of one is still open goes too.
        let closed = self.host.close_remote(None, "remote listening is off");
        log(&self.instance, &format!("no paired client is left and no pairing code is open: remote listening is off{}", if closed > 0 { format!("; closed {closed} connection(s)") } else { String::new() }));
        Ok(stopped.then(|| "No paired client is left: remote listening is off (`atelier remote pair` turns it on again).".to_owned()))
    }
}

/// Saves canvas documents shortly after they change and tells their clients
/// when saving starts or stops failing.
fn docs_loop(shared: Arc<Agents>) {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        // Refused edits answered once their gap is over (R2-C-05).
        let now = now_ms();
        let owed: Vec<(u64, String, u32, String)> = lock(&shared.doc_refusals).iter_mut().filter_map(|((client, key), r)| r.due(now).map(|(n, error)| (*client, key.clone(), n, error))).collect();
        for (id, key, refused, error) in owed {
            let client = lock(&shared.clients).get(&id).cloned();
            if let Some(client) = client {
                shared.resync_refused(&client, &key, &error, refused.saturating_sub(1));
            }
        }
        for saved in shared.docs.save(false) {
            if let Some(error) = &saved.error {
                log(&shared.instance, &format!("cannot save the canvas {}: {error}", saved.key));
            }
            let targets: Vec<Arc<Client>> = lock(&shared.clients).values().filter(|c| saved.clients.contains(&c.id)).cloned().collect();
            for target in targets {
                target.send(ServiceMsg::DocSaved { key: saved.key.clone(), version: saved.version, error: saved.error.clone() });
            }
        }
    }
}

/// Answers backend requests (the same handler as the in-process backend).
fn rpc_worker(ctx: Ctx, jobs: Receiver<RpcJob>, instance: Instance) {
    use canvas_model::rpc::{Request, Response, unsupported};
    for job in jobs {
        // Its client is gone (a reconnect asks again): nobody waits for the
        // answer, and the new connection's requests wait behind it (S2-07).
        if job.client.is_dropped() {
            continue;
        }
        let response = match job.request {
            Ok(request) => {
                let name = request.name();
                // Remote clients never ask for these (they open the files in the canvas): logged to tell.
                if let Request::OpenFolder { path } | Request::OpenTarget { target: path } = &request {
                    log(&instance, &format!("{name} for a client: {path}"));
                }
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::backend::handle(&ctx, request)))
                    .unwrap_or_else(|_| Response::Error(format!("the service failed answering {name}")))
            }
            Err(error) => unsupported(&error),
        };
        let json = serde_json::to_string(&response).unwrap_or_else(|e| serde_json::to_string(&Response::Error(format!("cannot encode the answer: {e}"))).unwrap_or_default());
        job.client.send(rpc_reply(job.id, json));
    }
}

/// An `RpcReply`; an answer too large for one frame becomes an error answer
/// (the connection stays up and the caller learns why).
fn rpc_reply(id: u64, json: String) -> ServiceMsg {
    const LIMIT: usize = canvas_common::protocol::MAX_FRAME - 4096;
    if json.len() <= LIMIT {
        return ServiceMsg::RpcReply { id, json };
    }
    let error = canvas_model::rpc::Response::Error(format!("the answer is too large to send ({} MB; at most {} MB)", json.len() >> 20, LIMIT >> 20));
    ServiceMsg::RpcReply { id, json: serde_json::to_string(&error).unwrap_or_default() }
}

/// True when a live process (not a recycled PID) holds the session's in-use lock.
pub fn session_live(root: &Path, id: &str) -> bool {
    !session_lock_pids(root, id).is_empty()
}

/// The live processes (not recycled PIDs) holding the session's in-use locks.
pub fn session_lock_pids(root: &Path, id: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(root.join(id)) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            name.to_string_lossy()
                .strip_prefix("inuse.")
                .and_then(|rest| rest.strip_suffix(".lock"))
                .and_then(|pid| pid.parse::<u32>().ok())
                .filter(|pid| canvas_sys::process::lock_owner_alive(*pid, &e.path()))
        })
        .collect()
}

/// Who holds a session open, as a refusal names it.
#[derive(Clone, Debug, PartialEq)]
enum Holder {
    /// A process inside one of this service's terminals: (terminal id, program, pid).
    Terminal(String, String, u32),
    /// VS Code (the process or one of its ancestors): (program, pid).
    VsCode(String, u32),
    /// Any other program: (program, pid).
    Other(String, u32),
}

/// Classifies the process `pid`: inside one of this service's terminals
/// (`terminal_of`: the terminal whose process tree has it), VS Code (it or
/// an ancestor, from `procs`: pid → parent, name), or another program.
fn classify_holder(pid: u32, procs: &HashMap<u32, (u32, String)>, terminal_of: &dyn Fn(u32) -> Option<String>) -> Holder {
    let name = procs.get(&pid).map(|(_, n)| n.clone()).unwrap_or_else(|| "a process".into());
    if let Some(id) = terminal_of(pid) {
        return Holder::Terminal(id, name, pid);
    }
    let vscode = |n: &str| {
        let n = n.to_ascii_lowercase();
        n.starts_with("code") || n.contains("vscode") || n.starts_with("cursor")
    };
    let (mut at, mut hops) = (pid, 0);
    while let Some((parent, n)) = procs.get(&at) {
        if vscode(n) {
            return Holder::VsCode(n.clone(), pid);
        }
        if *parent == 0 || *parent == at || hops > 32 {
            break;
        }
        (at, hops) = (*parent, hops + 1);
    }
    Holder::Other(name, pid)
}

impl Agents {
    /// Who holds agent session `native` open, after "is" (a refusal, the
    /// restore's reason, the log): "already running" in a canvas box (named),
    /// in VS Code, or in another program, with its name and pid, and what to
    /// do. `launching`: the box being started (its own terminal still closing says so).
    pub(super) fn session_holder(&self, kind: AgentKind, native: &str, launching: Option<&str>) -> String {
        const ELSEWHERE: &str = "already running in another terminal: use that terminal";
        let name = |id: &str| lock(&self.routing).names.get(id).cloned().map(|n| format!("“{n}”")).unwrap_or_else(|| format!("terminal {id}"));
        if kind == AgentKind::OpenCode {
            // Only TUIs this service started count as live: one of its own terminals.
            let fresh = now_ms().saturating_sub(5_000);
            return match crate::agents::opencode::servers().into_iter().find(|(_, s)| s.ok_ms >= fresh && s.sessions.contains(native)) {
                Some((server, _)) => format!("already running inside the canvas box {}: use that box", name(&server.terminal)),
                None => ELSEWHERE.into(),
            };
        }
        match self.holder(kind, native) {
            Some(holder) => holder_text(&holder, launching, &name),
            None => ELSEWHERE.into(),
        }
    }

    /// The process that holds agent session `native` open, classified (`None`:
    /// not known, OpenCode's included).
    fn holder(&self, kind: AgentKind, native: &str) -> Option<Holder> {
        if kind == AgentKind::OpenCode {
            return None;
        }
        let pid = kind.provider().holders(native).first().copied()?;
        let entries = canvas_sys::process::process_entries();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (child, parent, _) in &entries {
            children.entry(*parent).or_default().push(*child);
        }
        let procs: HashMap<u32, (u32, String)> = entries.into_iter().map(|(pid, parent, name)| (pid, (parent, name))).collect();
        let terms: Vec<(String, u32)> = lock(&self.terms).values().filter(|t| t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        let terminal_of = |pid: u32| terms.iter().find(|(_, root)| canvas_sys::process::process_tree(*root, &children).contains(&pid)).map(|(id, _)| id.clone());
        Some(classify_holder(pid, &procs, &terminal_of))
    }

    /// Refuses to become a second writer of agent session `session` (live in
    /// another process), naming who has it. Held by this box's own earlier
    /// CLI that is still closing (a Stop, then Resume at once): waited for, up
    /// to [`OWN_CLOSE_WAIT`] (`wait`), then started; refused only if it is
    /// still there, and then not as "already running" (it is open nowhere
    /// else, R2-C-09).
    fn refuse_live(&self, id: &str, kind: AgentKind, session: &str, wait: bool) -> Result<()> {
        let live = || kind.provider().is_live(session);
        if !live() {
            return Ok(());
        }
        let holder = self.holder(kind, session);
        let own = matches!(&holder, Some(Holder::Terminal(terminal, ..)) if terminal == id);
        if own && wait && !still_live(&live, OWN_CLOSE_WAIT) {
            log(&self.instance, &format!("terminal {id}: waited for its own earlier {} to close", kind.id()));
            return Ok(());
        }
        let name = |id: &str| lock(&self.routing).names.get(id).cloned().map(|n| format!("“{n}”")).unwrap_or_else(|| format!("terminal {id}"));
        let text = match &holder {
            Some(holder) => holder_text(holder, Some(id), &name),
            None => self.session_holder(kind, session, Some(id)),
        };
        log(&self.instance, &format!("terminal {id}: {} session {session} is {text}; not started", kind.id()));
        anyhow::bail!("this session is {text}")
    }
}

/// How long a launch waits for its box's own earlier CLI to close (R2-C-09).
const OWN_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Whether `live` still holds after waiting up to `wait` for it to clear.
fn still_live(live: &dyn Fn() -> bool, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while live() {
        if Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// The words for a holder (`name`: a box's name by its terminal id).
fn holder_text(holder: &Holder, launching: Option<&str>, name: &dyn Fn(&str) -> String) -> String {
    match holder {
        // Not "already running": it is open nowhere else, and clients show those words as ↗ open elsewhere (R2-C-09).
        Holder::Terminal(id, exe, pid) if launching == Some(id.as_str()) => format!("still closing in this box's own terminal ({exe} pid {pid}): try again in a few seconds"),
        Holder::Terminal(id, exe, pid) => format!("already running inside the canvas box {} ({exe} pid {pid}): use that box", name(id)),
        Holder::VsCode(exe, pid) => format!("already running in VS Code ({exe} pid {pid}): close it there first"),
        Holder::Other(exe, pid) => format!("already running in another terminal ({exe} pid {pid}, not started by Atelier): use that terminal"),
    }
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

fn copilot_program() -> Result<String> {
    if let Some(custom) = canvas_common::env::var_os("ATELIER_COPILOT") {
        return Ok(custom.to_string_lossy().into_owned());
    }
    let name = canvas_sys::host::exe_name("copilot");
    find_on_path(&name).map(|p| p.to_string_lossy().into_owned()).with_context(|| format!("{name} was not found on PATH (set ATELIER_COPILOT to override)"))
}

/// Copilot processes inside a terminal's process tree that are a session's
/// own CLI process: `copilot.exe` started by something else (the shim) or by
/// that shim (the real CLI). Deeper `copilot.exe` are the CLI's workers.
/// (`copilot` on macOS and Linux, where [`name_node_copilots`] also names
/// the Node processes that run the CLI's script.)
pub fn nested_copilots(tree: &[u32], procs: &HashMap<u32, (u32, String)>) -> Vec<u32> {
    let copilot = canvas_sys::host::exe_name("copilot");
    let is_copilot = |pid: u32| procs.get(&pid).is_some_and(|(_, name)| *name == copilot);
    let parent = |pid: u32| procs.get(&pid).map(|(p, _)| *p).unwrap_or(0);
    tree.iter()
        .copied()
        .filter(|&pid| is_copilot(pid) && (!is_copilot(parent(pid)) || !is_copilot(parent(parent(pid)))))
        .collect()
}

/// The terminal's own Copilot (its root, or the CLI its root shim started):
/// its session is the terminal's, never a subsession, and the shim never
/// holds a lock (S4-10: each was looked for in every session folder 15 times).
fn own_copilot(pid: u32, root: u32, procs: &HashMap<u32, (u32, String)>) -> bool {
    let copilot = canvas_sys::host::exe_name("copilot");
    pid == root || (procs.get(&pid).is_some_and(|(parent, _)| *parent == root) && procs.get(&root).is_some_and(|(_, name)| *name == copilot))
}

/// Names `copilot` the processes of `tree` that run the Copilot CLI as a
/// script (`node …/bin/copilot`, `node …/@github/copilot/index.js`): on
/// macOS and Linux the CLI is a Node program, so its process name is the
/// interpreter's (MAC-LINUX-PLAN §5.10). A process counts when the script it
/// runs (its first argument that is not an option) is the CLI; only those
/// holding a session's in-use lock become subsessions. Command lines are not
/// read on Windows.
pub fn name_node_copilots(tree: &[u32], procs: &mut HashMap<u32, (u32, String)>) {
    let copilot = canvas_sys::host::exe_name("copilot");
    if cfg!(windows) {
        return;
    }
    for &pid in tree {
        let Some((_, name)) = procs.get(&pid) else { continue };
        if *name == copilot {
            continue;
        }
        let Some(argv) = canvas_sys::process::process_argv(pid) else { continue };
        let runs_copilot = argv.iter().skip(1).find(|arg| !arg.starts_with('-')).is_some_and(|arg| {
            let base = arg.rsplit('/').next().unwrap_or(arg);
            base == "copilot" || arg.contains("/@github/copilot/")
        });
        if runs_copilot && let Some(entry) = procs.get_mut(&pid) {
            entry.1 = copilot.clone();
        }
    }
}

/// The session whose in-use lock this process holds (a stale lock whose
/// pid was reused names nothing: S5-05).
pub fn session_for_pid(root: &Path, pid: u32) -> Option<String> {
    let lock_name = format!("inuse.{pid}.lock");
    std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| is_session_id(name))
        .find(|name| {
            let lock = root.join(name).join(&lock_name);
            lock.exists() && canvas_sys::process::lock_owner_alive(pid, &lock)
        })
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
        // Links of terminals long gone are forgotten (the map only grew, S4-08).
        let running: HashSet<String> = lock(&shared.terms).values().filter(|t| t.alive.load(Ordering::Acquire)).map(|t| t.id.clone()).collect();
        let now = now_ms();
        lock(&shared.subsessions).retain(|_, link| link.keep(&running, now));
        let terms: Vec<(String, u32)> =
            lock(&shared.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        if terms.is_empty() {
            checked.clear();
            continue;
        }
        let entries = canvas_sys::process::process_entries();
        let mut procs: HashMap<u32, (u32, String)> = entries.iter().map(|(pid, parent, name)| (*pid, (*parent, name.clone()))).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, parent, _) in &entries {
            children.entry(*parent).or_default().push(*pid);
        }
        let mut alive = HashSet::new();
        for (parent_id, root) in terms {
            let tree = canvas_sys::process::process_tree(root, &children);
            name_node_copilots(&tree, &mut procs);
            for pid in nested_copilots(&tree, &procs).into_iter().filter(|pid| !own_copilot(*pid, root, &procs)) {
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
                shared.new_subsession(&child, message);
                let _ = shared.index_wake.try_send(());
            }
        }
        checked.retain(|pid, _| alive.contains(pid));
    }
}

/// A session started inside a hosted terminal, as clients are told of it.
struct SubsessionLink {
    message: ServiceMsg,
    found_ms: u64,
    /// Its box was removed from a canvas (a `DelNode` edit of the child): never sent again.
    dismissed: bool,
}

/// Links whose parent no longer runs are kept this long (a client away meanwhile still learns of them).
const SUBSESSION_KEEP_MS: u64 = 3_600_000;

impl SubsessionLink {
    /// The link for a connecting client, unless its box was removed.
    fn tell(&self) -> Option<ServiceMsg> {
        (!self.dismissed).then(|| self.message.clone())
    }

    /// Kept while its parent runs, or for a while after.
    fn keep(&self, running: &HashSet<String>, now: u64) -> bool {
        let parent = match &self.message {
            ServiceMsg::Subsession { parent, .. } => parent.as_str(),
            _ => "",
        };
        running.contains(parent) || now.saturating_sub(self.found_ms) < SUBSESSION_KEEP_MS
    }
}

impl Agents {
    /// Records a subsession found now and tells every client.
    fn new_subsession(&self, child: &str, message: ServiceMsg) {
        lock(&self.subsessions).insert(child.to_owned(), SubsessionLink { message: message.clone(), found_ms: now_ms(), dismissed: false });
        self.broadcast(message);
    }
}

/// VM boxes' lifecycle (docs/VM-SESSIONS.md §8): a running VM box nobody
/// shows, whose agent is not working, with nothing queued for it, and quiet
/// for the pack's `suspend_after` is suspended; a suspended one with a
/// message waiting is resumed (typing and starting it resume it too).
fn vm_idle_loop(shared: Arc<Agents>) {
    use canvas_common::protocol::Activity;
    use canvas_model::vm::{VmOp, VmState};
    let home = shared.instance.home.clone();
    loop {
        std::thread::sleep(Duration::from_secs(15));
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(folder) = crate::vm::pack::configured(&home) else { continue };
        let (pack, status) = crate::vm::pack::load(&folder);
        if status.errors() > 0 || pack.provider == crate::vm::pack::ProviderKind::Hooks {
            continue;
        }
        let intents = crate::vm::launch::all(&home);
        if intents.is_empty() {
            continue;
        }
        let now = now_ms();
        let visible = shared.visible();
        let waiting: HashSet<String> = lock(&shared.bus).mailbox.iter().map(|m| m.to.clone()).collect();
        for intent in intents {
            let id = intent.r#box.clone();
            if !matches!(intent.state, VmState::Running | VmState::Suspended | VmState::Suspending) {
                continue;
            }
            let summary = lock(&shared.sessions).get(&shared.native_of(&id)).cloned();
            let working = summary.as_ref().is_some_and(|s| s.activity == Activity::Working || s.pending > 0);
            let output = lock(&shared.terms).get(&id).map(|t| t.last_output_ms.load(Ordering::Relaxed)).unwrap_or(0);
            let typed = lock(&shared.input_at).get(&id).copied().unwrap_or(0);
            let last = output.max(typed).max(summary.map(|s| s.updated_ms.max(s.last_event_ms)).unwrap_or(0)).max(intent.updated_ms);
            let seen = visible.contains(&id);
            let mail = waiting.contains(&id);
            let Some(op) = vm_idle_verdict(intent.state, &intent.pending, pack.suspend_after_ms, seen, mail, working, now.saturating_sub(last)) else { continue };
            let why = if op == VmOp::Suspend { format!("suspended after {} min idle and unseen", pack.suspend_after_ms / 60_000) } else { "resumed for a message waiting for it".into() };
            if crate::vm::action(&home, &id, op).is_ok() {
                log(&shared.instance, &format!("VM box {id}: {why}"));
            }
        }
    }
}

/// The idle loop's decision for one VM box: suspend a running one that is
/// unseen, has no mail, is not working and has been quiet for
/// `suspend_after_ms` (0: never); resume a suspended one with mail.
fn vm_idle_verdict(state: canvas_model::vm::VmState, pending: &str, suspend_after_ms: u64, seen: bool, mail: bool, working: bool, quiet_ms: u64) -> Option<canvas_model::vm::VmOp> {
    use canvas_model::vm::{VmOp, VmState};
    if !pending.is_empty() {
        return None;
    }
    match state {
        VmState::Running if suspend_after_ms > 0 && !seen && !mail && !working && quiet_ms >= suspend_after_ms => Some(VmOp::Suspend),
        VmState::Suspended | VmState::Suspending if mail => Some(VmOp::Resume),
        _ => None,
    }
}

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

/// A new session counts as a pending terminal's without proof once it is this
/// old (a first turn that ended before the TUI's server was asked).
const SETTLED_MS: u64 = 10_000;

/// The session a terminal started, among the new top-level `candidates` in
/// its folder ((id, created ms), oldest first): one its own server reported
/// busy or waiting; else, unless another pending terminal works in the same
/// folder, the oldest one once `settled` (a fork, or the terminal is gone) or
/// old enough.
fn pick_session(candidates: &[(String, u64)], claimed: &HashSet<String>, proof: &HashSet<String>, contested: bool, settled: bool, now: u64) -> Option<String> {
    let open: Vec<&(String, u64)> = candidates.iter().filter(|(id, _)| !claimed.contains(id)).collect();
    if let Some((id, _)) = open.iter().find(|(id, _)| proof.contains(id)) {
        return Some(id.clone());
    }
    if contested {
        return None;
    }
    open.first().filter(|(_, created)| settled || now.saturating_sub(*created) >= SETTLED_MS).map(|(id, _)| id.clone())
}

/// Learns the sessions new OpenCode terminals create (on the first prompt, or
/// at start for a fork) and tells the clients, so their boxes follow them.
fn agent_session_loop(shared: Arc<Agents>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let pending: Vec<(String, PendingSession)> = lock(&shared.pending_sessions).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if pending.is_empty() {
            continue;
        }
        // Normally started by the history poll; that waits while OpenCode's database is missing.
        crate::agents::opencode::watch_servers();
        let servers = crate::agents::opencode::servers();
        let now = now_ms();
        for (id, mut entry) in pending {
            let server = servers.iter().find(|(s, _)| s.terminal == id);
            // The watcher asked the TUI's own (verified) server for its folder.
            if entry.cwd.is_none()
                && let Some(dir) = server.and_then(|(_, state)| state.directory.clone())
            {
                log(&shared.instance, &format!("terminal {id} works in {dir}; waiting for its session"));
                entry.cwd = Some(dir);
            }
            // Sessions busy in this TUI's own process are its own.
            if let Some(live) = server.and_then(|(_, state)| state.live.as_ref()) {
                entry.busy.extend(live.status.keys().cloned().chain(live.waiting().map(str::to_owned)));
            }
            if let Some(stored) = lock(&shared.pending_sessions).get_mut(&id).filter(|p| p.launched_ms == entry.launched_ms) {
                stored.cwd.clone_from(&entry.cwd);
                stored.busy.clone_from(&entry.busy);
            }
            let alive = lock(&shared.terms).get(&id).is_some_and(|t| t.alive.load(Ordering::Acquire));
            let gone = !alive && now.saturating_sub(entry.launched_ms) > SETTLED_MS;
            if let Some(cwd) = entry.cwd.as_deref() {
                let candidates = crate::agents::opencode::discover_new_sessions(cwd, entry.launched_ms.saturating_sub(2_000));
                let claimed: HashSet<String> = lock(&shared.agent_sessions).iter().filter(|(k, _)| **k != id).map(|(_, v)| v.clone()).collect();
                let contested = lock(&shared.pending_sessions).iter().any(|(other, p)| *other != id && p.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)));
                if let Some(native) = pick_session(&candidates, &claimed, &entry.busy, contested, entry.fork || gone, now) {
                    shared.learned_session(&id, &native);
                    continue;
                }
            }
            // Gone without a session (no prompt was sent): stop looking.
            if gone {
                let mut pending = lock(&shared.pending_sessions);
                if pending.get(&id).is_some_and(|p| p.launched_ms == entry.launched_ms) {
                    pending.remove(&id);
                }
            }
        }
    }
}

/// The bus source of an event found in agent session `session`: the terminal
/// (box) that runs it when the agent's id differs, else the session itself.
fn event_source(agent_sessions: &HashMap<String, String>, session: &str) -> String {
    let mut terminals: Vec<&String> = agent_sessions.iter().filter(|(_, native)| *native == session).map(|(id, _)| id).collect();
    terminals.sort();
    terminals.first().map(|id| (*id).clone()).unwrap_or_else(|| session.to_owned())
}

fn index_loop(shared: Arc<Agents>, wake: Receiver<()>) {
    let mut histories: Vec<(AgentKind, Box<dyn crate::agents::History>)> = crate::agents::ALL.iter().map(|kind| (*kind, kind.provider().history())).collect();
    let mut mirrors: HashMap<PathBuf, crate::index::Index> = HashMap::new();
    let mut claude_mirrors: HashMap<PathBuf, crate::agents::claude_history::ClaudeHistory> = HashMap::new();
    loop {
        let natives: Vec<String> = {
            let terms = lock(&shared.terms);
            let sessions = lock(&shared.agent_sessions);
            sessions.iter().filter(|(id, _)| terms.get(*id).is_some_and(|t| t.alive.load(Ordering::Acquire))).map(|(_, native)| native.clone()).collect()
        };
        let lingering: Vec<String> = {
            let mut lingering = lock(&shared.lingering);
            let now = now_ms();
            lingering.retain(|(until, _)| *until > now);
            lingering.iter().flat_map(|(_, ids)| ids.iter().cloned()).collect()
        };
        let tracked: HashSet<String> = lock(&shared.tracked)
            .values()
            .flat_map(|ids| ids.iter().cloned())
            .chain(lingering)
            .chain(lock(&shared.terms).values().filter(|t| t.copilot).map(|t| t.id.clone()))
            .chain(natives)
            .collect();
        let (mut changed, mut removed) = (Vec::new(), Vec::new());
        // VM sessions' history, mirrored from their guests (the thin lane):
        // one more Copilot index per mirror (none without VM sessions).
        for root in crate::vm::thin::copilot_mirrors(&shared.instance.home) {
            // Live as the guest's satellite reports it (`<home>/vm/mirrors/<sandbox>/copilot`).
            let sandbox = root.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let links = shared.vm_links.clone();
            mirrors.entry(root.clone()).or_insert_with(|| crate::index::Index::new(root).with_liveness(Arc::new(move |id| links.session_live(&sandbox, id))));
        }
        for index in mirrors.values_mut() {
            let (c, _) = index.poll(&tracked, Duration::from_secs(15));
            changed.extend(c);
        }
        // Claude Code's mirrored histories: live as the guest's satellite reports them.
        for root in crate::vm::thin::claude_mirrors(&shared.instance.home) {
            let sandbox = root.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (links, dir) = (shared.vm_links.clone(), root.clone());
            let live: crate::agents::claude_history::LiveSessions = Arc::new(move || {
                let mut files = crate::agents::claude_history::mirrored_pid_files(&dir);
                files.retain(|id, _| links.session_live(&sandbox, id));
                files
            });
            claude_mirrors.entry(root.clone()).or_insert_with(|| crate::agents::claude_history::ClaudeHistory::new(root).with_live(live));
        }
        for history in claude_mirrors.values_mut() {
            let (c, _) = history.poll(&tracked, Duration::from_secs(15));
            changed.extend(c);
        }
        for (kind, history) in histories.iter_mut() {
            // One agent's broken store must not stop status updates for the others.
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let result = history.poll(&tracked, Duration::from_secs(15));
                (result, history.take_detected(), history.take_warnings())
            }));
            let ((c, r), detected, warnings) = match polled {
                Ok(polled) => polled,
                Err(panic) => {
                    let message = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
                    log(&shared.instance, &format!("{} history panicked ({message}); starting it over", kind.label()));
                    *history = kind.provider().history();
                    continue;
                }
            };
            changed.extend(c);
            removed.extend(r);
            for (session, kind, data) in detected {
                // Routing knows boxes by their terminal id; OpenCode names its sessions itself.
                let source = event_source(&lock(&shared.agent_sessions), &session);
                let alias = if source == session { String::new() } else { format!(" (terminal {source})") };
                log(&shared.instance, &format!("detected {kind} in {session}{alias}"));
                let _ = shared.bus_tx.send(crate::bus::Event { kind, source, data, ..Default::default() });
            }
            for warning in warnings {
                log(&shared.instance, &warning);
            }
        }
        if !changed.is_empty() || !removed.is_empty() {
            // Broadcast under the lock a full list is sent under (`send_sessions`): never a stale list after an update.
            let mut sessions = lock(&shared.sessions);
            for summary in &changed {
                sessions.insert(summary.id.clone(), summary.clone());
            }
            for id in &removed {
                sessions.remove(id);
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
    fn an_unseen_quiet_vm_box_is_suspended_and_mail_resumes_it() {
        use canvas_model::vm::{VmOp, VmState};
        let min30 = 30 * 60_000;
        assert_eq!(vm_idle_verdict(VmState::Running, "", min30, false, false, false, min30), Some(VmOp::Suspend));
        assert_eq!(vm_idle_verdict(VmState::Running, "", min30, false, false, false, min30 - 1), None, "not quiet long enough");
        assert_eq!(vm_idle_verdict(VmState::Running, "", min30, true, false, false, min30 * 4), None, "shown in a client");
        assert_eq!(vm_idle_verdict(VmState::Running, "", min30, false, true, false, min30 * 4), None, "a message waits for it");
        assert_eq!(vm_idle_verdict(VmState::Running, "", min30, false, false, true, min30 * 4), None, "its agent is working");
        assert_eq!(vm_idle_verdict(VmState::Running, "", 0, false, false, false, u64::MAX), None, "suspend_after: 0 never suspends");
        assert_eq!(vm_idle_verdict(VmState::Running, "delete", min30, false, false, false, min30), None, "an operation is pending");
        assert_eq!(vm_idle_verdict(VmState::IntegrationOffline, "", min30, false, false, false, min30), None);
        assert_eq!(vm_idle_verdict(VmState::Suspended, "", min30, false, true, false, 0), Some(VmOp::Resume));
        assert_eq!(vm_idle_verdict(VmState::Suspended, "", min30, true, false, false, 0), None, "seeing it does not resume it (viewing never starts anything)");
        assert_eq!(vm_idle_verdict(VmState::Suspended, "resume", min30, false, true, false, 0), None);
    }

    #[test]
    fn new_sessions_go_to_the_terminal_that_made_them() {
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let now = 100_000;
        let fresh = vec![("ses_a".to_owned(), now - 1_000), ("ses_b".to_owned(), now - 500)];
        let none = set(&[]);
        assert_eq!(pick_session(&fresh, &none, &set(&["ses_b"]), false, false, now).as_deref(), Some("ses_b"), "its own server reported it busy");
        assert_eq!(pick_session(&fresh, &none, &set(&["ses_b"]), true, false, now).as_deref(), Some("ses_b"), "proof wins even in a shared folder");
        assert_eq!(pick_session(&fresh, &none, &none, false, false, now), None, "a just-created session may be someone else's: wait");
        assert_eq!(pick_session(&fresh, &none, &none, false, true, now).as_deref(), Some("ses_a"), "a fork (or a finished run): the oldest new one");
        assert_eq!(pick_session(&fresh, &set(&["ses_a"]), &none, false, true, now).as_deref(), Some("ses_b"), "never one another terminal runs");
        assert_eq!(pick_session(&fresh, &none, &none, true, true, now), None, "another new terminal in the folder: wait for proof");
        let settled = vec![("ses_old".to_owned(), now - SETTLED_MS)];
        assert_eq!(pick_session(&settled, &none, &none, false, false, now).as_deref(), Some("ses_old"), "a quick first turn is still found");
        assert_eq!(pick_session(&[], &none, &set(&["ses_x"]), false, true, now), None, "busy elsewhere is not new here");
        assert!(same_dir(r"C:\Repos\Demo\", "c:/repos/demo"));
    }

    #[test]
    fn launches_say_when_the_agent_session_is_known() {
        const SES: &str = "ses_e7fbbd911ffeOynBIm3IrWFsUl";
        let oc = |f: &dyn Fn(&mut AgentLaunch)| {
            let mut launch = AgentLaunch { agent: "opencode".into(), ..Default::default() };
            f(&mut launch);
            session_plan(AgentKind::OpenCode, &launch, "box", launch.native_id.as_deref().unwrap_or("box"))
        };
        assert_eq!(oc(&|_| {}), SessionPlan::Expect { fork: false }, "named on the first prompt");
        assert_eq!(oc(&|l| l.fork_from = Some(SES.into())), SessionPlan::Expect { fork: true });
        assert_eq!(oc(&|l| (l.resume, l.native_id) = (true, Some(SES.into()))), SessionPlan::Known(SES.into()), "resumed: its events and status follow it at once");
        let passive = |l: &mut AgentLaunch| (l.resume, l.native_id, l.headless_prompt) = (true, Some(SES.into()), Some("run".into()));
        assert_eq!(oc(&passive), SessionPlan::Known(SES.into()));
        assert_eq!(oc(&|l| l.headless_prompt = Some("run".into())), SessionPlan::Terminal, "a first passive run is learned from its folder");
        let copilot = AgentLaunch { agent: "copilot".into(), resume: true, ..Default::default() };
        assert_eq!(session_plan(AgentKind::Copilot, &copilot, "box", "box"), SessionPlan::Terminal);
    }

    #[test]
    fn events_found_in_an_agents_own_session_come_from_its_box() {
        let sessions: HashMap<String, String> = [("box-1".to_owned(), "ses_a".to_owned()), ("box-2".to_owned(), "ses_b".to_owned())].into_iter().collect();
        assert_eq!(event_source(&sessions, "ses_a"), "box-1", "routing knows the box, not OpenCode's id");
        assert_eq!(event_source(&sessions, "12345678-1234-1234-1234-123456789abc"), "12345678-1234-1234-1234-123456789abc", "Copilot and Claude: the same id");
        assert_eq!(event_source(&HashMap::new(), "ses_c"), "ses_c", "an imported box is keyed by its session id");
    }

    #[test]
    fn nested_copilots_are_session_processes_not_workers() {
        let procs: HashMap<u32, (u32, String)> = [
            (1, (0, "atelier-service.exe")),
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
        // The CLI's name on this platform (`copilot` off Windows).
        .map(|(pid, (parent, name))| (pid, (parent, if name == "copilot.exe" { canvas_sys::host::exe_name("copilot") } else { name.to_owned() })))
        .collect();
        let tree = [10, 11, 12, 20, 21, 22, 23, 24];
        assert_eq!(nested_copilots(&tree, &procs), vec![10, 11, 21, 22]);
        // The terminal's own shim and CLI are its session, never looked up as subsessions.
        assert_eq!(nested_copilots(&tree, &procs).into_iter().filter(|pid| !own_copilot(*pid, 10, &procs)).collect::<Vec<_>>(), vec![21, 22]);
    }

    #[test]
    fn answers_too_large_for_a_frame_become_errors() {
        let small = rpc_reply(3, "\"Done\"".into());
        assert_eq!(small, ServiceMsg::RpcReply { id: 3, json: "\"Done\"".into() });
        let huge = format!("\"{}\"", "x".repeat(canvas_common::protocol::MAX_FRAME));
        let ServiceMsg::RpcReply { id, json } = rpc_reply(4, huge) else { panic!("a reply") };
        assert_eq!(id, 4, "the caller still gets its answer");
        assert!(matches!(serde_json::from_str::<canvas_model::rpc::Response>(&json).unwrap(), canvas_model::rpc::Response::Error(e) if e.contains("too large")));
        // What stays is always sendable.
        let mut writer = canvas_common::wire::FrameWriter::new(Box::new(std::io::sink()));
        assert!(writer.send(&rpc_reply(5, format!("\"{}\"", "y".repeat(canvas_common::protocol::MAX_FRAME - 4096 - 2)))).is_ok());
    }

    #[test]
    fn refusals_name_who_holds_the_session() {
        let procs: HashMap<u32, (u32, String)> = [
            (1, (0, "atelier-service.exe")),
            (10, (1, "copilot.exe")), // box A's terminal
            (11, (10, "pwsh.exe")),
            (12, (11, "copilot.exe")), // `copilot --resume S` an agent ran in box A
            (20, (0, "Code.exe")),
            (21, (20, "Code.exe")),
            (22, (21, "copilot.exe")), // VS Code's agent host
            (30, (0, "explorer.exe")),
            (31, (30, "WindowsTerminal.exe")),
            (32, (31, "copilot.exe")), // a terminal outside Atelier
        ]
        .into_iter()
        .map(|(pid, (parent, name))| (pid, (parent, name.to_owned())))
        .collect();
        // Box A's terminal is the tree of pid 10.
        let terms = |pid: u32| [10, 11, 12].contains(&pid).then(|| "box-a".to_owned());
        let terms: &dyn Fn(u32) -> Option<String> = &terms;
        assert_eq!(classify_holder(12, &procs, terms), Holder::Terminal("box-a".into(), "copilot.exe".into(), 12));
        assert_eq!(classify_holder(22, &procs, terms), Holder::VsCode("Code.exe".into(), 22));
        assert_eq!(classify_holder(32, &procs, terms), Holder::Other("copilot.exe".into(), 32));
        let name = |id: &str| if id == "box-a" { "“Refactor”".to_owned() } else { format!("terminal {id}") };
        let texts = [
            holder_text(&classify_holder(12, &procs, terms), Some("box-b"), &name),
            holder_text(&classify_holder(12, &procs, terms), Some("box-a"), &name),
            holder_text(&classify_holder(22, &procs, terms), None, &name),
            holder_text(&classify_holder(32, &procs, terms), None, &name),
        ];
        assert_eq!(texts[0], "already running inside the canvas box “Refactor” (copilot.exe pid 12): use that box");
        assert!(texts[1].starts_with("still closing in this box's own terminal") && texts[1].contains("try again"), "{}", texts[1]);
        assert_eq!(texts[2], "already running in VS Code (Code.exe pid 22): close it there first");
        assert_eq!(texts[3], "already running in another terminal (copilot.exe pid 32, not started by Atelier): use that terminal");
        // Every holder elsewhere keeps the words clients recognise as "open elsewhere" (↗);
        // the box's own closing CLI is open nowhere else (R2-C-09).
        assert!([&texts[0], &texts[2], &texts[3]].iter().all(|t| t.starts_with("already running")));
        assert!(!texts[1].contains("already running"));
    }

    #[test]
    fn refused_edits_inside_the_gap_are_answered_when_it_ends_never_dropped() {
        let mut r = Refusals::default();
        assert_eq!(r.refused(10_000, "a".into()), Some(0), "the first: the whole canvas at once");
        // The client resyncs and re-sends; refused again twice within the gap: owed, not dropped (R2-C-05).
        assert_eq!(r.refused(10_100, "b".into()), None);
        assert_eq!(r.refused(10_200, "c".into()), None);
        assert_eq!(r.due(14_999), None, "not before the gap ends");
        assert_eq!(r.due(15_000), Some((2, "c".to_owned())), "one answer for both, with the latest reason");
        assert_eq!(r.due(15_100), None, "once");
        // One more within the new gap: owed again; after it, answered at once.
        assert_eq!(r.refused(16_000, "d".into()), None);
        assert_eq!(r.due(20_000), Some((1, "d".to_owned())));
        assert_eq!(r.refused(26_000, "e".into()), Some(0));
    }

    #[test]
    fn a_launch_waits_for_its_own_closing_cli_then_starts() {
        // Live for 300 ms (the box's own CLI closing after a Stop), then gone: waited for.
        let start = Instant::now();
        let closing = || start.elapsed() < Duration::from_millis(300);
        assert!(!still_live(&closing, Duration::from_secs(5)));
        assert!(start.elapsed() >= Duration::from_millis(300) && start.elapsed() < Duration::from_secs(2));
        // Still live after the wait: refused then.
        let start = Instant::now();
        assert!(still_live(&|| true, Duration::from_millis(250)));
        assert!(start.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn a_plugin_taken_from_the_cache_is_never_removed_by_a_launch_that_replaces_it() {
        let root = std::env::temp_dir().join(format!("cc-plugin-race-{}", uuid::Uuid::new_v4().simple()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let made = |dir: &Path| canvas_model::skills::GroupPlugin { dir: Some(dir.to_path_buf()), ..Default::default() };
        let (plugins, cache): (GroupPlugins, Mutex<HashMap<String, CachedPlugin>>) = Default::default();
        keep_built_plugin(&plugins, &cache, "group".into(), 1, "t0", &made(&a));
        lock(&plugins).remove("t0");
        // Launch 1 takes A from the cache: it is registered as it is taken.
        assert_eq!(take_cached_plugin(&plugins, &cache, "group", 1, "t1").and_then(|m| m.dir), Some(a.clone()));
        // Launch 2 (its folders changed meanwhile) builds B, which replaces A in the cache.
        assert!(take_cached_plugin(&plugins, &cache, "group", 2, "t2").is_none());
        assert_eq!(keep_built_plugin(&plugins, &cache, "group".into(), 2, "t2", &made(&b)), std::slice::from_ref(&a));
        assert!(plugin_in_use(&plugins, &cache, &a), "launch 1 hands A to its agent: never removed under it");
        lock(&plugins).remove("t1");
        assert!(!plugin_in_use(&plugins, &cache, &a) && plugin_in_use(&plugins, &cache, &b));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_skill_folders_stamp_moves_only_when_they_change() {
        let root = std::env::temp_dir().join(format!("cc-skills-stamp-{}", uuid::Uuid::new_v4()));
        let skill = root.join("team").join("review");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: review\n---\n").unwrap();
        std::fs::write(skill.join("scripts").join("run.sh"), "echo 1").unwrap();
        let dirs = vec![root.join("team").display().to_string()];
        let first = skills_stamp(&dirs);
        assert_eq!(skills_stamp(&dirs), first, "unchanged: the plugin built before is reused");
        // A folder's own time moving (Windows brings it up to date lazily) changes nothing.
        #[cfg(unix)]
        {
            let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
            std::fs::File::open(&skill).unwrap().set_modified(later).unwrap();
            assert_eq!(skills_stamp(&dirs), first, "only a folder's time moved");
        }
        // A file deep inside changes: built again.
        std::fs::write(skill.join("scripts").join("run.sh"), "echo 22").unwrap();
        let second = skills_stamp(&dirs);
        assert_ne!(second, first);
        // A file added (or a folder): built again.
        std::fs::create_dir_all(skill.join("assets")).unwrap();
        assert_ne!(skills_stamp(&dirs), second);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_reconnecting_client_is_told_of_a_subsession_again_unless_its_box_was_removed() {
        let message = ServiceMsg::Subsession { parent: "box-a".into(), child: "child-1".into(), name: "Nested".into(), cwd: String::new() };
        let mut link = SubsessionLink { message: message.clone(), found_ms: 1_000, dismissed: false };
        // Every connect gets it (one told before may have lost it with its
        // connection, or had another canvas open, R2-C-10); a client that has the box keeps it as it is.
        assert_eq!(link.tell(), Some(message.clone()));
        assert_eq!(link.tell(), Some(message));
        // Its box removed from the canvas (the service applies that edit): never again (S5-06).
        link.dismissed = true;
        assert_eq!(link.tell(), None);
        // Kept while its parent runs, forgotten an hour after it stopped.
        let running: HashSet<String> = ["box-a".to_owned()].into();
        assert!(link.keep(&running, 10 * SUBSESSION_KEEP_MS));
        assert!(link.keep(&HashSet::new(), 1_000 + SUBSESSION_KEEP_MS - 1));
        assert!(!link.keep(&HashSet::new(), 1_000 + SUBSESSION_KEEP_MS));
    }

    #[test]
    fn session_for_pid_finds_the_lock_holder() {
        let root = std::env::temp_dir().join(format!("cc-lock-{}", uuid::Uuid::new_v4()));
        let id = "12345678-1234-1234-1234-123456789abc";
        std::fs::create_dir_all(root.join(id)).unwrap();
        // This process holds it (it started before the lock was written).
        let me = std::process::id();
        std::fs::write(root.join(id).join(format!("inuse.{me}.lock")), "").unwrap();
        assert_eq!(session_for_pid(&root, me).as_deref(), Some(id));
        assert_eq!(session_for_pid(&root, me + 1), None);
        // A lock left by a dead process whose pid is not running: nothing.
        let other = "22345678-1234-1234-1234-123456789abc";
        std::fs::create_dir_all(root.join(other)).unwrap();
        std::fs::write(root.join(other).join("inuse.4000000.lock"), "").unwrap();
        assert_eq!(session_for_pid(&root, 4_000_000), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn group_skill_folders_reach_every_agents_command_line() {
        use canvas_model::launch::{attach_group_skills, take_group_skills};
        let root = std::env::temp_dir().join(format!("cc-group-skills-{}", uuid::Uuid::new_v4()));
        let (home, skills, repo) = (root.join("home"), root.join("team-skills"), root.join("repo"));
        for (dir, name) in [(&skills, "team-review"), (&skills, "shared"), (&repo.join(".claude").join("skills"), "shared")] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: d\n---\n")).unwrap();
        }
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // SAFETY: as in the providers' own tests, these are only read.
        unsafe {
            std::env::set_var("ATELIER_COPILOT", r"C:\bin\copilot.exe");
            std::env::set_var("ATELIER_OPENCODE", r"C:\bin\opencode.exe");
            std::env::set_var("ATELIER_CLAUDE", r"C:\bin\claude.exe");
        }
        let id = "12345678-1234-1234-1234-123456789abc";
        for kind in crate::agents::ALL {
            for headless in [None, Some("run".to_owned())] {
                let mut launch = AgentLaunch { agent: kind.id().into(), headless_prompt: headless.clone(), ..Default::default() };
                attach_group_skills(&mut launch, "g-1", &[skills.display().to_string()]);
                let carried = take_group_skills(&mut launch).expect("carried");
                // Every agent reads the repository's .claude/skills itself: `shared` is not given again.
                let (plugin, line) = group_skill_plugin(&home, &format!("1-{id}"), id, kind, &repo, &carried);
                let plugin = plugin.expect("a plugin");
                assert!(line.contains("skill folders of group g-1 (1 folder(s)): team-review; skipped shared (the session's project already has it)"), "{line}");
                assert!(plugin.join("skills").join("team-review").join("SKILL.md").is_file() && !plugin.join("skills").join("shared").exists());
                let integration = Integration { skill_plugins: vec![plugin.clone()], env: no_group_skills_env(), ..Default::default() };
                let command = match kind {
                    AgentKind::OpenCode => crate::agents::opencode::OpenCode.command_with(id, &launch, &integration, false).unwrap(),
                    _ => kind.provider().command(id, &launch, &integration).unwrap(),
                };
                match kind {
                    AgentKind::OpenCode => {
                        let config = command.env.iter().rev().find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").map(|(_, v)| v.clone()).unwrap_or_default();
                        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
                        assert_eq!(config["skills"]["paths"], serde_json::json!([plugin.join("skills").display().to_string().replace('\\', "/")]), "{config}");
                    }
                    _ => assert!(command.args.windows(2).any(|w| w[0] == "--plugin-dir" && w[1] == plugin.display().to_string()), "{kind:?} {:?}", command.args),
                }
                assert!(!command.env.iter().any(|(k, _)| canvas_model::launch::is_group_skills(k)), "never in the agent's environment");
                // An inherited one is removed when the terminal starts ("-NAME"), under either name.
                assert!(no_group_skills_env().iter().all(|entry| command.env.contains(entry)), "{kind:?} {:?}", command.env);
            }
        }
        // A session outside the repository gets every skill; nothing to give: no plugin.
        let elsewhere = root.join("notes");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let carried = canvas_model::launch::GroupSkills { group: String::new(), dirs: vec![skills.display().to_string()] };
        let (plugin, line) = group_skill_plugin(&home, &format!("2-{id}"), id, AgentKind::Claude, &elsewhere, &carried);
        assert!(plugin.is_some() && line.contains("skill folders of the canvas (1 folder(s)): shared, team-review"), "{line}");
        let empty = canvas_model::launch::GroupSkills { group: "g-1".into(), dirs: vec![root.join("none").display().to_string()] };
        assert_eq!(group_skill_plugin(&home, &format!("3-{id}"), id, AgentKind::Claude, &elsewhere, &empty).0, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_exiting_terminal_never_takes_the_plugin_of_its_relaunch() {
        let plugin = |n: u64| (PathBuf::from(format!(r"C:\h\group-skills\default\g\{n}-t")), n * 1000);
        let mut plugins = HashMap::from([("t".to_owned(), plugin(1))]);
        // The terminal started after its plugin was made: its own, removed on exit.
        assert_eq!(take_exited_plugin(&mut plugins, "t", Some(1500)), Some(plugin(1).0));
        assert!(plugins.is_empty());
        // Relaunched before the exit was handled: the new plugin stays.
        plugins.insert("t".into(), plugin(3));
        assert_eq!(take_exited_plugin(&mut plugins, "t", Some(1500)), None);
        assert_eq!(plugins.get("t"), Some(&plugin(3)));
        // No terminal known any more, or none registered.
        assert_eq!(take_exited_plugin(&mut plugins, "t", None), Some(plugin(3).0));
        assert_eq!(take_exited_plugin(&mut plugins, "other", Some(1)), None);
    }

    #[test]
    fn plain_copilot_launches_never_pass_the_skill_folders_on() {
        let env = vec![("copilot_canvas_group_skills".to_owned(), r#"{"dirs":["C:\\s"]}"#.to_owned()), ("Atelier_Group_Skills".to_owned(), "x".to_owned()), ("KEEP".to_owned(), "1".to_owned())];
        assert_eq!(without_group_skills(env), [("KEEP".to_owned(), "1".to_owned())]);
        assert_eq!(no_group_skills_env(), [("-COPILOT_CANVAS_GROUP_SKILLS".to_owned(), String::new()), ("-ATELIER_GROUP_SKILLS".to_owned(), String::new())]); // rename:keep
    }
}
