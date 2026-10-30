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

use crate::agents::{AgentKind, Integration, Provide};
use crate::backend::Ctx;
use crate::index::is_session_id;
use crate::paths::{Instance, now_ms};
use crate::protocol::{AgentLaunch, ClientMsg, LaunchKind, ServiceMsg, SessionSummary};
pub use canvas_common::auth::{read_token, write_token};
pub use server::log;
use term::lock;

/// Threads answering backend requests (`ClientMsg::Rpc`): slow ones (CLI
/// runs, scans) and quick ones (file checks, listings) on their own.
const RPC_WORKERS: usize = 8;
const QUICK_RPC_WORKERS: usize = 4;

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
    /// The agent each terminal runs (for its exit hook).
    agent_terms: Mutex<HashMap<String, AgentKind>>,
    /// Terminals whose agent session is not known yet.
    pending_sessions: Mutex<HashMap<String, PendingSession>>,
    /// Terminal id → the agent's own session id, once learned (told to every client).
    agent_sessions: Mutex<HashMap<String, String>>,
    /// Terminal id → the skills plugin its next launch gets (a canvas-wide
    /// agent's run, set right before the launch).
    skill_plugins: Mutex<HashMap<String, PathBuf>>,
    /// Canvas documents clients opened.
    docs: docs::Docs,
    /// Backend requests for the worker threads (slow, quick).
    rpc: (Sender<RpcJob>, Sender<RpcJob>),
    /// Pairing codes and paired remote clients.
    remote: crate::remote::Registry,
    /// The reports inbox (`inbox.rs`).
    reports: Mutex<crate::reports::Store>,
    /// The routed canvas's tracked pull requests (`prs.rs`).
    prs: Mutex<prs::Hub>,
    /// The routed canvas's review follow-ups (`reviews.rs`).
    reviews: Mutex<reviews::Hub>,
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
        let (rpc_tx, rpc_rx) = unbounded::<RpcJob>();
        let (quick_tx, quick_rx) = unbounded::<RpcJob>();
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
            agent_terms: Mutex::new(HashMap::new()),
            pending_sessions: Mutex::new(HashMap::new()),
            agent_sessions: Mutex::new(HashMap::new()),
            skill_plugins: Mutex::new(HashMap::new()),
            docs: docs::Docs::default(),
            rpc: (rpc_tx, quick_tx),
            remote: crate::remote::Registry::new(&instance.home),
            reports: Mutex::new(crate::reports::Store::open(&instance.home, now_ms())),
            prs: Mutex::new(prs::Hub::default()),
            reviews: Mutex::new(reviews::Hub::default()),
        });
        server::spawn_named("index", { let a = agents.clone(); move || index_loop(a, index_rx) });
        server::spawn_named("subsessions", { let a = agents.clone(); move || subsession_loop(a) });
        server::spawn_named("agent-sessions", { let a = agents.clone(); move || agent_session_loop(a) });
        server::spawn_named("docs", { let a = agents.clone(); move || docs_loop(a) });
        for (count, jobs) in [(RPC_WORKERS, &rpc_rx), (QUICK_RPC_WORKERS, &quick_rx)] {
            for _ in 0..count {
                let (ctx, jobs, owner) = (Ctx::new(&instance), jobs.clone(), instance.clone());
                server::spawn_named("rpc", move || rpc_worker(ctx, jobs, owner));
            }
        }
        // The skills and the `copilot-canvas` command that canvas sessions use for events.
        if let Err(error) = crate::cli::write_plugin(&instance.home) {
            log(&instance, &format!("cannot write the canvas plugin: {error:#}"));
        }
        if let Err(error) = crate::cli::install_cli(&instance.home) {
            log(&instance, &format!("cannot install the copilot-canvas command: {error:#}"));
        }
        server::spawn_named("bus", { let a = agents.clone(); move || passive::bus_loop(a, bus_rx) });
        server::spawn_named("reports", { let a = agents.clone(); move || inbox::run(a) });
        server::spawn_named("prs", { let a = agents.clone(); move || prs::run(a) });
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

    fn resolve_agent(&self, id: &str, mut launch: AgentLaunch) -> Result<Resolved> {
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
        // A conversation that never got its first prompt has nothing to resume:
        // it starts now (Copilot's `--session-id` does either by itself).
        if launch.resume && launch.fork_from.is_none() && kind != AgentKind::Copilot && !provider.has_history(&session) {
            launch.resume = false;
        }
        let mut integration = self.integration(id);
        integration.skill_plugins.extend(self.skill_plugin(id));
        let command = provider.command(id, &launch, &integration)?;
        let how = if launch.headless_prompt.is_some() { "headless run" } else if launch.fork_from.is_some() { "fork" } else if launch.resume { "resume" } else { "new session" };
        // The model as the command line carries it (agents that keep a session's model get none on resume).
        let model = launch.model.as_deref().filter(|m| command.args.iter().any(|a| a == m)).map(|m| format!(", model {m}")).unwrap_or_default();
        let skills = if integration.skill_plugins.is_empty() { "" } else { ", with its canvas skills" };
        log(&self.instance, &format!("terminal {id}: {} {how}{model}{skills}", kind.id()));
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
        Ok(Resolved { program: command.program, args: command.args, env: command.env, agent: true })
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

impl HostApp for Agents {
    fn command_env(&self, id: &str) -> Vec<(String, String)> {
        // Plain shells (the demo's) get the `copilot-canvas` command and their identity too.
        self.integration(id).env
    }

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
        for (id, native) in lock(&self.agent_sessions).iter() {
            client.send(ServiceMsg::AgentSession { id: id.clone(), native: native.clone() });
        }
        client.send(ServiceMsg::Bus { json: passive::snapshot(self) });
        // Only clients that show the inbox get it (`CAP_REPORTS`).
        client.send(inbox::message(self));
    }

    fn client_disconnected(&self, client: u64) {
        lock(&self.tracked).remove(&client);
        self.docs.leave(client);
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
            ClientMsg::Bus { json } => {
                // Inbox requests travel as bus requests (older services ignore them).
                let handled = serde_json::from_str::<serde_json::Value>(&json).is_ok_and(|value| inbox::action(self, &value));
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
                let quick = request.as_ref().is_ok_and(|r| r.is_quick());
                let queue = if quick { &self.rpc.1 } else { &self.rpc.0 };
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
        let kind = lock(&self.agent_terms).get(id).copied();
        if let Some(kind) = kind {
            let started = std::time::Instant::now();
            kind.provider().terminal_stopping(id);
            log(&self.instance, &format!("stopping {id} ({}), prepared in {} ms", kind.id(), started.elapsed().as_millis()));
        }
    }

    fn terminal_exited(&self, id: &str) {
        // Already relaunched: what is registered now belongs to the new run.
        if lock(&self.terms).get(id).is_some_and(|t| t.alive.load(Ordering::Acquire)) {
            return;
        }
        let kind = lock(&self.agent_terms).remove(id);
        if let Some(kind) = kind {
            kind.provider().terminal_exited(id, &self.instance.home);
        }
    }

    fn has_work(&self) -> bool {
        passive::has_work(self)
    }

    fn remote_credential(&self, id: &str) -> canvas_common::handshake::Credential {
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
        for saved in self.docs.save(true) {
            if let Some(error) = saved.error {
                log(&self.instance, &format!("cannot save the canvas {}: {error}", saved.key));
            }
        }
    }
}

impl Agents {
    /// Pairing codes, the client list and revocation (`copilot-canvas remote
    /// pair|clients|revoke`), for clients on the local pipe only.
    fn remote_admin(&self, client: &Arc<Client>, request: canvas_model::rpc::Request) -> canvas_model::rpc::Response {
        use canvas_model::rpc::{PairingCode, RemoteClient, Request, Response};
        if client.remote {
            return Response::Error("remote access is managed on the service's machine (`copilot-canvas remote …` there)".into());
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
                    let open: Vec<String> = lock(&self.clients).values().filter_map(|c| c.remote_id.clone()).collect();
                    Response::RemoteClients(
                        self.remote
                            .list()?
                            .into_iter()
                            .map(|c| RemoteClient {
                                connections: open.iter().filter(|id| **id == c.id).count() as u32,
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
                    let connected: Vec<Arc<Client>> = lock(&self.clients).values().filter(|c| c.remote_id.as_deref() == Some(revoked.id.as_str())).cloned().collect();
                    for connection in &connected {
                        connection.close();
                    }
                    log(&self.instance, &format!("revoked remote client {:?} ({:?}); closed {} connection(s)", revoked.id, revoked.name, connected.len()));
                    let mut text = format!("Revoked {} ({}); {} live connection(s) closed.", revoked.name, revoked.id, connected.len());
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
        let applied = serde_json::from_str::<Vec<canvas_model::sync::Op>>(&ops_json).context("unreadable canvas edit").and_then(|ops| {
            self.docs.edit(&key, client.id, &ops, |version, clients| {
                let targets: Vec<Arc<Client>> = lock(&self.clients).values().filter(|c| clients.contains(&c.id)).cloned().collect();
                for target in targets {
                    target.send(ServiceMsg::DocPatch { key: key.clone(), version, origin: client.id, ops_json: ops_json.clone() });
                }
            })
        });
        if let Err(error) = applied {
            log(&self.instance, &format!("refused a canvas edit from client {}: {error:#}", client.id));
            self.docs.snapshot(&key, client.id, |version, reply| {
                let json = serde_json::to_string(&reply).unwrap_or_default();
                client.send(ServiceMsg::Doc { key: key.clone(), version, json });
            });
            client.send(ServiceMsg::Error { id: None, message: format!("A canvas change was refused: {error:#}") });
        }
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
        let entries = crate::win::process_entries();
        let exe = entries.iter().find(|(p, _, _)| *p == pid).map(|(_, _, name)| name.clone()).unwrap_or_default();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (child, parent, _) in &entries {
            children.entry(*parent).or_default().push(*child);
        }
        let terms: Vec<(String, u32)> = lock(&self.terms).values().filter(|t| t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        match terms.into_iter().find(|(_, root)| crate::win::process_tree(*root, &children).contains(&pid)) {
            Some((terminal, _)) => format!("a command in canvas terminal {terminal:?} ({exe} pid {pid})"),
            None => format!("a local process ({exe} pid {pid})"),
        }
    }

    /// Turns remote listening off (setting and listener) once no paired
    /// client is left and no code is open, unless `COPILOT_CANVAS_LISTEN` asks for it.
    fn listen_only_while_needed(&self) -> Result<Option<String>> {
        if self.remote.active_count() > 0 || self.remote.open_codes(now_ms()) > 0 || std::env::var_os("COPILOT_CANVAS_LISTEN").is_some_and(|v| !v.is_empty()) {
            return Ok(None);
        }
        let home = &self.instance.home;
        let mut settings = canvas_common::settings::Settings::load_checked(home)?;
        if settings.remote.listen.take().is_some() {
            settings.save(home).context("cannot turn off remote listening in settings.json")?;
        }
        let stopped = self.host.stop_remote();
        log(&self.instance, "no paired client is left and no pairing code is open: remote listening is off");
        Ok(stopped.then(|| "No paired client is left: remote listening is off (`copilot-canvas remote pair` turns it on again).".to_owned()))
    }
}

/// Saves canvas documents shortly after they change and tells their clients
/// when saving starts or stops failing.
fn docs_loop(shared: Arc<Agents>) {
    loop {
        std::thread::sleep(Duration::from_millis(100));
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
    loop {
        let natives: Vec<String> = {
            let terms = lock(&shared.terms);
            let sessions = lock(&shared.agent_sessions);
            sessions.iter().filter(|(id, _)| terms.get(*id).is_some_and(|t| t.alive.load(Ordering::Acquire))).map(|(_, native)| native.clone()).collect()
        };
        let tracked: HashSet<String> = lock(&shared.tracked)
            .values()
            .flat_map(|ids| ids.iter().cloned())
            .chain(lock(&shared.terms).values().filter(|t| t.copilot).map(|t| t.id.clone()))
            .chain(natives)
            .collect();
        let (mut changed, mut removed) = (Vec::new(), Vec::new());
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
