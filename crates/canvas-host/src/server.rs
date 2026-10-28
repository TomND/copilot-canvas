//! The terminal host server (L1). Owns every hosted terminal so sessions
//! survive client restarts; streams only visible terminals to clients as row
//! diffs; keeps the session you type in responsive (priority manager).
//!
//! Everything that is not about terminals (session history, the canvas
//! document, the event bus, …) is delegated to a [`HostApp`], the L3 backend
//! that shares this process.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

use crate::paths::{Instance, build_id, now_ms};
use crate::pipe::{Pipe, PipeServer};
use crate::protocol::{self, ClientMsg, Frame, Input, LaunchKind, PROTOCOL, ServiceMsg, ServiceStats, read_frame, write_frame};
use crate::emulator::Engine;
use crate::term::{Hooks, Spawn, Terminal, lock, row_hash};

const FRAME_INTERVAL: Duration = Duration::from_millis(4);
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
pub const CLIENT_QUEUE: usize = 1024;

/// A launch the backend resolved into a concrete program.
pub struct Resolved {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// An agent CLI: graceful stop (Ctrl+D), tracked history, idle auto-stop.
    pub agent: bool,
}

/// The L3 backend running in the same process (the "agent supervisor").
pub trait HostApp: Send + Sync {
    /// Turns an agent launch (`LaunchKind::Copilot…`) into a program to run.
    fn resolve_launch(&self, id: &str, kind: LaunchKind) -> Result<Resolved>;
    /// A client authenticated: send it the backend's snapshots.
    fn client_connected(&self, client: &Arc<Client>);
    fn client_disconnected(&self, client: u64);
    /// A message of the canvas channel (everything the host does not handle).
    fn message(&self, client: &Arc<Client>, message: ClientMsg) -> Result<()>;
    /// Keyboard input reached a terminal (queued messages wait while you type).
    fn input_noted(&self, id: &str);
    /// A terminal was launched or exited.
    fn terminals_changed(&self);
    /// Pending work that keeps the service running without clients or terminals.
    fn has_work(&self) -> bool;
}

struct Sub {
    generation: u64,
    hashes: Vec<u64>,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16)>,
    offset: u32,
}

/// One connected client.
pub struct Client {
    pub id: u64,
    out: Sender<ServiceMsg>,
    subs: Mutex<HashMap<String, Sub>>,
}

impl Client {
    /// Queues a message; false when the client is gone.
    pub fn send(&self, message: ServiceMsg) -> bool {
        !matches!(self.out.try_send(message), Err(TrySendError::Disconnected(_)))
    }
}

pub struct Host {
    pub instance: Instance,
    token: Vec<u8>,
    pub terms: Mutex<HashMap<String, Arc<Terminal>>>,
    pub clients: Mutex<HashMap<u64, Arc<Client>>>,
    next_client: AtomicU64,
    /// Wakes the frame pump.
    pub pump: Sender<()>,
    exited: Sender<String>,
    clipboard: Sender<String>,
    /// Terminal ids whose title, progress or bell count changed.
    changed: Sender<String>,
    last_input_client: AtomicU64,
    /// Terminal that most recently received keyboard input (gets CPU priority).
    active_input: Mutex<Option<String>>,
    priority_wake: Sender<()>,
    bytes: Arc<AtomicU64>,
    frames: AtomicU64,
    last_activity: Mutex<Instant>,
    started: Instant,
    pub shutting_down: AtomicBool,
    /// Terminal engine for new terminals (`COPILOT_CANVAS_ENGINE`).
    pub engine: Engine,
    app: OnceLock<Arc<dyn HostApp>>,
}

pub fn log(instance: &Instance, message: &str) {
    let path = instance.log_path();
    if std::fs::metadata(&path).map(|m| m.len() > 4 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} [{}] {message}", now_ms(), std::process::id());
    }
}

/// The engine new terminals use: `COPILOT_CANVAS_ENGINE=alacritty|ghostty`,
/// else the build's default.
pub fn configured_engine() -> Engine {
    std::env::var("COPILOT_CANVAS_ENGINE")
        .ok()
        .and_then(|name| Engine::parse(&name))
        .filter(|engine| engine.available())
        .unwrap_or(Engine::Alacritty)
}

pub fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    std::thread::Builder::new().name(name.into()).spawn(body).expect("spawn service thread");
}

/// Runs the service: binds the pipe, starts the host threads, lets `make_app`
/// build the backend, then serves clients until shutdown.
pub fn run(instance: Instance, make_app: impl FnOnce(&Arc<Host>) -> Arc<dyn HostApp>) -> Result<()> {
    crate::win::raise_priority();
    let mut server = match PipeServer::bind(&instance.pipe_name()) {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("a service for this canvas is already running")
        }
        Err(error) => return Err(error).context("cannot create the service pipe"),
    };
    if let Err(error) = crate::win::contain_descendants() {
        log(&instance, &format!("job containment unavailable: {error}"));
    }
    let token = protocol::nonce();
    canvas_common::auth::write_token(&instance.token_path(), &token)?;
    log(&instance, &format!("service started, build {}", build_id()));

    let (pump_tx, pump_rx) = bounded::<()>(1);
    let (exited_tx, exited_rx) = unbounded::<String>();
    let (clip_tx, clip_rx) = bounded::<String>(16);
    let (changed_tx, changed_rx) = bounded::<String>(256);
    let (prio_tx, prio_rx) = bounded::<()>(1);
    let host = Arc::new(Host {
        instance: instance.clone(),
        token,
        terms: Mutex::new(HashMap::new()),
        clients: Mutex::new(HashMap::new()),
        next_client: AtomicU64::new(1),
        pump: pump_tx,
        exited: exited_tx,
        clipboard: clip_tx,
        changed: changed_tx,
        last_input_client: AtomicU64::new(0),
        active_input: Mutex::new(None),
        priority_wake: prio_tx,
        bytes: Arc::new(AtomicU64::new(0)),
        frames: AtomicU64::new(0),
        last_activity: Mutex::new(Instant::now()),
        started: Instant::now(),
        shutting_down: AtomicBool::new(false),
        engine: configured_engine(),
        app: OnceLock::new(),
    });
    let app = make_app(&host);
    let _ = host.app.set(app);

    spawn_named("pump", { let h = host.clone(); move || pump(h, pump_rx) });
    spawn_named("exits", { let h = host.clone(); move || exit_loop(h, exited_rx) });
    spawn_named("clipboard", { let h = host.clone(); move || clipboard_loop(h, clip_rx) });
    spawn_named("changes", { let h = host.clone(); move || change_loop(h, changed_rx) });
    spawn_named("idle", { let h = host.clone(); move || idle_loop(h) });
    spawn_named("priority", { let h = host.clone(); move || priority_loop(h, prio_rx) });

    loop {
        match server.accept() {
            Ok(pipe) => {
                let host = host.clone();
                spawn_named("client", move || {
                    if let Err(error) = serve_client(&host, pipe) {
                        log(&host.instance, &format!("client ended: {error:#}"));
                    }
                });
            }
            Err(error) => {
                log(&instance, &format!("accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

impl Host {
    /// The backend; its threads may call in while `run` is still installing it.
    fn app(&self) -> &Arc<dyn HostApp> {
        loop {
            if let Some(app) = self.app.get() {
                return app;
            }
            std::thread::yield_now();
        }
    }

    pub fn broadcast(&self, message: ServiceMsg) {
        let clients: Vec<Arc<Client>> = lock(&self.clients).values().cloned().collect();
        for client in clients {
            client.send(message.clone());
        }
    }

    pub fn touch(&self) {
        *lock(&self.last_activity) = Instant::now();
    }

    pub fn terminal(&self, id: &str) -> Result<Arc<Terminal>> {
        lock(&self.terms).get(id).cloned().with_context(|| format!("no terminal {id}"))
    }

    /// Terminals subscribed by any client (shown on someone's screen).
    pub fn visible(&self) -> HashSet<String> {
        lock(&self.clients).values().flat_map(|c| lock(&c.subs).keys().cloned().collect::<Vec<_>>()).collect()
    }

    pub fn launch(&self, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16) -> Result<()> {
        anyhow::ensure!(!id.is_empty() && id.len() <= 100, "invalid terminal id");
        if let Some(existing) = lock(&self.terms).get(id)
            && existing.alive.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let resolved = match kind {
            LaunchKind::Command { program, args } => Resolved { program, args, env: Vec::new(), agent: false },
            other => self.app().resolve_launch(id, other)?,
        };
        let terminal = Terminal::spawn(
            Spawn {
                id,
                program: &resolved.program,
                args: &resolved.args,
                env: &resolved.env,
                cwd,
                cols,
                rows,
                copilot: resolved.agent,
                engine: self.engine,
            },
            Hooks {
                pump: self.pump.clone(),
                exited: self.exited.clone(),
                clipboard: self.clipboard.clone(),
                bytes: self.bytes.clone(),
                changed: self.changed.clone(),
            },
        )?;
        // The pid stays last: `selftest copilot` parses the number after " pid ".
        log(&self.instance, &format!("launched {id} engine {} pid {}", terminal.engine.name(), terminal.pid));
        let info = terminal.info();
        lock(&self.terms).insert(id.to_owned(), terminal);
        self.broadcast(ServiceMsg::Terminals(vec![info]));
        self.app().terminals_changed();
        Ok(())
    }

    pub fn stats(&self) -> ServiceStats {
        let terms = lock(&self.terms);
        let usage = crate::win::process_usage(None);
        ServiceStats {
            pid: std::process::id(),
            terminals: terms.len() as u32,
            alive: terms.values().filter(|t| t.alive.load(Ordering::Acquire)).count() as u32,
            working_set_bytes: usage.working_set,
            private_bytes: usage.private,
            cpu_ms: usage.cpu_ms,
            uptime_ms: self.started.elapsed().as_millis() as u64,
            bytes_parsed: self.bytes.load(Ordering::Relaxed),
            frames_sent: self.frames.load(Ordering::Relaxed),
        }
    }

    pub fn shutdown(self: &Arc<Self>) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        log(&self.instance, "shutdown requested");
        let terms: Vec<Arc<Terminal>> = lock(&self.terms).values().cloned().collect();
        for terminal in &terms {
            terminal.stop(false);
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline && terms.iter().any(|t| t.alive.load(Ordering::Acquire)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        for terminal in &terms {
            terminal.force_kill();
        }
        let _ = std::fs::remove_file(self.instance.token_path());
        log(&self.instance, "service exiting");
        std::process::exit(0);
    }

    fn note_active(&self, id: &str) {
        let mut active = lock(&self.active_input);
        if active.as_deref() != Some(id) {
            *active = Some(id.to_owned());
            drop(active);
            let _ = self.priority_wake.try_send(());
        }
    }
}

fn serve_client(host: &Arc<Host>, pipe: Pipe) -> Result<()> {
    let (mut reader, mut writer, closer) = pipe.split()?;
    let Some(ClientMsg::Hello { protocol: _, build: _, nonce: client_nonce }) = read_frame(&mut reader)? else {
        bail!("expected hello");
    };
    anyhow::ensure!(client_nonce.len() == 32, "bad client nonce");
    let server_nonce = protocol::nonce();
    write_frame(
        &mut writer,
        &ServiceMsg::Challenge {
            protocol: PROTOCOL,
            build: build_id().to_owned(),
            nonce: server_nonce.clone(),
            proof: protocol::prove(&host.token, "server", &client_nonce),
        },
    )?;
    let Some(ClientMsg::Auth { proof }) = read_frame(&mut reader)? else { bail!("expected auth") };
    if !protocol::verify(&host.token, "client", &server_nonce, &proof) {
        bail!("client failed authentication");
    }

    let (out_tx, out_rx) = bounded::<ServiceMsg>(CLIENT_QUEUE);
    let id = host.next_client.fetch_add(1, Ordering::Relaxed);
    let client = Arc::new(Client { id, out: out_tx, subs: Mutex::new(HashMap::new()) });
    let writer_closer = closer.clone();
    spawn_named("client-writer", move || {
        for message in out_rx {
            if write_frame(&mut writer, &message).is_err() {
                break;
            }
        }
        writer_closer.close();
    });
    client.send(ServiceMsg::Ready { pid: std::process::id() });
    let infos: Vec<_> = lock(&host.terms).values().map(|t| t.info()).collect();
    client.send(ServiceMsg::Terminals(infos));
    host.app().client_connected(&client);
    lock(&host.clients).insert(id, client.clone());
    host.touch();

    let result = (|| -> Result<()> {
        while let Some(message) = read_frame::<_, ClientMsg>(&mut reader)? {
            host.touch();
            if let Err(error) = dispatch(host, &client, message) {
                client.send(ServiceMsg::Error { id: None, message: format!("{error:#}") });
            }
        }
        Ok(())
    })();
    lock(&host.clients).remove(&id);
    closer.close();
    host.app().client_disconnected(id);
    result
}

fn dispatch(host: &Arc<Host>, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
    match message {
        ClientMsg::Launch { id, kind, cwd, cols, rows } => {
            if let Err(error) = host.launch(&id, kind, &cwd, cols, rows) {
                client.send(ServiceMsg::Error { id: Some(id), message: format!("{error:#}") });
            }
        }
        ClientMsg::Subscribe { ids } => {
            let mut subs = lock(&client.subs);
            subs.retain(|id, _| ids.contains(id));
            for id in ids.into_iter().take(16) {
                subs.entry(id).or_insert(Sub { generation: 0, hashes: Vec::new(), cols: 0, rows: 0, cursor: None, offset: 0 });
            }
            drop(subs);
            let _ = host.pump.try_send(());
        }
        ClientMsg::Input { id, input } => {
            host.last_input_client.store(client.id, Ordering::Relaxed);
            if !matches!(input, Input::Focus(_)) {
                host.note_active(&id);
                host.app().input_noted(&id);
            }
            host.terminal(&id)?.send_input(&input)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Resize { id, cols, rows } => {
            host.terminal(&id)?.resize(cols, rows)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Scroll { id, delta } => {
            host.terminal(&id)?.scroll(delta);
            let _ = host.pump.try_send(());
        }
        ClientMsg::Wheel { id, up, col, row, mods } => {
            host.last_input_client.store(client.id, Ordering::Relaxed);
            host.terminal(&id)?.wheel(up, col, row, mods)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Click { id, col, row, button, mods } => {
            host.last_input_client.store(client.id, Ordering::Relaxed);
            host.terminal(&id)?.click(button, col, row, mods)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Stop { id, force } => host.terminal(&id)?.stop(force),
        ClientMsg::Forget { id } => {
            let mut terms = lock(&host.terms);
            if terms.get(&id).is_some_and(|t| !t.alive.load(Ordering::Acquire)) {
                terms.remove(&id);
                drop(terms);
                host.broadcast(ServiceMsg::TerminalGone(id));
            }
        }
        ClientMsg::Shutdown => {
            let host = host.clone();
            spawn_named("shutdown", move || host.shutdown());
        }
        ClientMsg::Stats => {
            client.send(ServiceMsg::Stats(host.stats()));
        }
        ClientMsg::Hello { .. } | ClientMsg::Auth { .. } => bail!("unexpected handshake message"),
        other => host.app().message(client, other)?,
    }
    Ok(())
}

fn pump(host: Arc<Host>, wake: Receiver<()>) {
    let mut next_allowed = Instant::now();
    loop {
        let _ = wake.recv_timeout(Duration::from_millis(50));
        let now = Instant::now();
        if now < next_allowed {
            std::thread::sleep(next_allowed - now);
        }
        let clients: Vec<Arc<Client>> = lock(&host.clients).values().cloned().collect();
        for client in clients {
            // Backpressure: a slow client gets fewer, newer frames instead of a growing queue.
            if client.out.len() > CLIENT_QUEUE / 2 {
                continue;
            }
            let mut subs = lock(&client.subs);
            for (id, sub) in subs.iter_mut() {
                let Some(terminal) = lock(&host.terms).get(id).cloned() else { continue };
                let generation = terminal.generation();
                if generation == sub.generation {
                    continue;
                }
                let Some(snap) = terminal.snapshot() else { continue };
                let lines = crate::term::rows_v7(&snap);
                let hashes: Vec<u64> = lines.iter().map(|runs| row_hash(runs)).collect();
                let full = sub.cols != snap.cols || sub.rows != snap.rows || sub.hashes.len() != hashes.len();
                let changed: Vec<(u16, Vec<crate::protocol::Run>)> = lines
                    .into_iter()
                    .enumerate()
                    .filter(|(y, _)| full || sub.hashes[*y] != hashes[*y])
                    .map(|(y, runs)| (y as u16, runs))
                    .collect();
                if !full && changed.is_empty() && sub.cursor == snap.cursor && sub.offset == snap.display_offset {
                    sub.generation = generation;
                    continue;
                }
                let frame = Frame {
                    id: id.clone(),
                    cols: snap.cols,
                    rows: snap.rows,
                    full,
                    changed,
                    cursor: snap.cursor,
                    display_offset: snap.display_offset,
                    history: snap.history,
                };
                match client.out.try_send(ServiceMsg::Frame(frame)) {
                    Ok(()) => {
                        host.frames.fetch_add(1, Ordering::Relaxed);
                        *sub = Sub {
                            generation,
                            hashes,
                            cols: snap.cols,
                            rows: snap.rows,
                            cursor: snap.cursor,
                            offset: snap.display_offset,
                        };
                    }
                    Err(_) => break,
                }
            }
        }
        next_allowed = Instant::now() + FRAME_INTERVAL;
    }
}

/// Keeps the session you type in responsive on a busy machine: its whole
/// process tree (the agent, tools, MCP servers, builds) runs above normal,
/// sessions shown on screen at normal, all others below normal. Background
/// sessions still use all spare CPU; they just yield to you. Only processes
/// inside our own sessions are touched. `COPILOT_CANVAS_NO_PRIORITY=1` turns
/// it off.
fn priority_loop(host: Arc<Host>, wake: Receiver<()>) {
    use windows_sys::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};
    if std::env::var_os("COPILOT_CANVAS_NO_PRIORITY").is_some() {
        return;
    }
    let mut applied: HashMap<u32, u32> = HashMap::new();
    loop {
        // Re-check every 2 s so new child processes (tools, builds) follow their session.
        let _ = wake.recv_timeout(Duration::from_secs(2));
        if host.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let terms: Vec<(String, u32)> =
            lock(&host.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        if terms.is_empty() {
            applied.clear();
            continue;
        }
        let active = lock(&host.active_input).clone();
        let visible = host.visible();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, parent) in crate::win::process_pairs() {
            children.entry(parent).or_default().push(pid);
        }
        let mut seen = HashSet::new();
        for (id, root) in terms {
            let class = if active.as_deref() == Some(id.as_str()) {
                ABOVE_NORMAL_PRIORITY_CLASS
            } else if visible.contains(&id) {
                NORMAL_PRIORITY_CLASS
            } else {
                BELOW_NORMAL_PRIORITY_CLASS
            };
            for pid in crate::win::process_tree(root, &children) {
                seen.insert(pid);
                if applied.get(&pid) != Some(&class) && crate::win::set_priority_class(pid, class) {
                    applied.insert(pid, class);
                }
            }
        }
        applied.retain(|pid, _| seen.contains(pid));
    }
}

fn exit_loop(host: Arc<Host>, exited: Receiver<String>) {
    for id in exited {
        if let Some(terminal) = lock(&host.terms).get(&id).cloned() {
            log(&host.instance, &format!("terminal {id} exited with {:?}", *lock(&terminal.exit_code)));
            host.broadcast(ServiceMsg::Terminals(vec![terminal.info()]));
        }
        host.app().terminals_changed();
    }
}

/// Resends terminal info when a title, progress state or bell changes;
/// bursts (a title per keystroke, progress toggles) are coalesced.
fn change_loop(host: Arc<Host>, changed: Receiver<String>) {
    while let Ok(first) = changed.recv() {
        std::thread::sleep(Duration::from_millis(100));
        let mut ids = HashSet::from([first]);
        ids.extend(changed.try_iter());
        let infos: Vec<crate::protocol::TermInfo> = {
            let terms = lock(&host.terms);
            ids.iter().filter_map(|id| terms.get(id).map(|t| t.info())).collect()
        };
        if !infos.is_empty() {
            host.broadcast(ServiceMsg::Terminals(infos));
        }
    }
}

fn clipboard_loop(host: Arc<Host>, clipboard: Receiver<String>) {
    for text in clipboard {
        let target = host.last_input_client.load(Ordering::Relaxed);
        if let Some(client) = lock(&host.clients).get(&target) {
            client.send(ServiceMsg::Clipboard(text));
        }
    }
}

fn idle_loop(host: Arc<Host>) {
    loop {
        std::thread::sleep(Duration::from_secs(20));
        let alive = lock(&host.terms).values().any(|t| t.alive.load(Ordering::Acquire));
        let clients = !lock(&host.clients).is_empty();
        if !alive && !clients && lock(&host.last_activity).elapsed() >= IDLE_EXIT && !host.app().has_work() {
            log(&host.instance, "idle with no clients or terminals; exiting");
            let _ = std::fs::remove_file(host.instance.token_path());
            std::process::exit(0);
        }
    }
}
