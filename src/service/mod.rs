//! Persistent terminal service. Owns every hosted terminal so sessions survive
//! client restarts; streams only visible terminals to clients as row diffs.

mod factory_watch;
mod passive;
pub mod keys;
pub mod term;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

use crate::index::{Index, is_session_id};
use crate::paths::{Instance, build_id, hex, now_ms};
use crate::pipe::{Pipe, PipeServer};
use crate::protocol::{
    self, ClientMsg, Frame, LaunchKind, PROTOCOL, ServiceMsg, ServiceStats, SessionSummary, read_frame, write_frame,
};
use term::{Hooks, Spawn, Terminal, lock, row_hash, snapshot};

const FRAME_INTERVAL: Duration = Duration::from_millis(4);
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
const CLIENT_QUEUE: usize = 1024;

struct Sub {
    generation: u64,
    hashes: Vec<u64>,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16)>,
    offset: u32,
}

struct Client {
    id: u64,
    out: Sender<ServiceMsg>,
    subs: Mutex<HashMap<String, Sub>>,
    tracked: Mutex<HashSet<String>>,
}

impl Client {
    fn send(&self, message: ServiceMsg) -> bool {
        !matches!(self.out.try_send(message), Err(TrySendError::Disconnected(_)))
    }
}

struct Shared {
    instance: Instance,
    token: Vec<u8>,
    terms: Mutex<HashMap<String, Arc<Terminal>>>,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    sessions: Mutex<HashMap<String, SessionSummary>>,
    next_client: AtomicU64,
    pump: Sender<()>,
    index_wake: Sender<()>,
    exited: Sender<String>,
    clipboard: Sender<String>,
    /// Terminal ids whose title, progress or bell count changed.
    changed: Sender<String>,
    /// Sessions started inside a hosted terminal: child id → message.
    subsessions: Mutex<HashMap<String, ServiceMsg>>,
    /// Groups and passive sessions from the canvas, and the bus state.
    routing: Mutex<crate::bus::Routing>,
    bus: Mutex<crate::bus::BusState>,
    bus_tx: Sender<crate::bus::Event>,
    /// When each terminal last got keyboard input (messages wait while you type).
    input_at: Mutex<HashMap<String, u64>>,
    last_input_client: AtomicU64,
    /// Terminal that most recently received keyboard input (gets CPU priority).
    active_input: Mutex<Option<String>>,
    priority_wake: Sender<()>,
    bytes: Arc<AtomicU64>,
    frames: AtomicU64,
    last_activity: Mutex<Instant>,
    started: Instant,
    shutting_down: AtomicBool,
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

fn write_token(path: &Path, token: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, hex(token))?;
    std::fs::rename(&temp, path).context("cannot publish service token")?;
    Ok(())
}

pub fn read_token(path: &Path) -> Result<Vec<u8>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = text.trim();
    anyhow::ensure!(text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()), "malformed service token");
    Ok((0..32).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("validated hex")).collect())
}

pub fn run(instance: Instance) -> Result<()> {
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
    write_token(&instance.token_path(), &token)?;
    log(&instance, &format!("service started, build {}", build_id()));

    let (pump_tx, pump_rx) = bounded::<()>(1);
    let (index_tx, index_rx) = bounded::<()>(1);
    let (exited_tx, exited_rx) = unbounded::<String>();
    let (clip_tx, clip_rx) = bounded::<String>(16);
    let (changed_tx, changed_rx) = bounded::<String>(256);
    let (bus_tx, bus_rx) = unbounded::<crate::bus::Event>();
    let (prio_tx, prio_rx) = bounded::<()>(1);
    let shared = Arc::new(Shared {
        instance: instance.clone(),
        token,
        terms: Mutex::new(HashMap::new()),
        clients: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        next_client: AtomicU64::new(1),
        pump: pump_tx,
        index_wake: index_tx,
        exited: exited_tx,
        clipboard: clip_tx,
        changed: changed_tx,
        subsessions: Mutex::new(HashMap::new()),
        routing: Mutex::new(passive::load_routing(&instance.home)),
        bus: Mutex::new(passive::load_state(&instance.home)),
        bus_tx,
        input_at: Mutex::new(HashMap::new()),
        last_input_client: AtomicU64::new(0),
        active_input: Mutex::new(None),
        priority_wake: prio_tx,
        bytes: Arc::new(AtomicU64::new(0)),
        frames: AtomicU64::new(0),
        last_activity: Mutex::new(Instant::now()),
        started: Instant::now(),
        shutting_down: AtomicBool::new(false),
    });

    spawn_named("pump", { let s = shared.clone(); move || pump(s, pump_rx) });
    spawn_named("index", { let s = shared.clone(); move || index_loop(s, index_rx) });
    spawn_named("exits", { let s = shared.clone(); move || exit_loop(s, exited_rx) });
    spawn_named("clipboard", { let s = shared.clone(); move || clipboard_loop(s, clip_rx) });
    spawn_named("changes", { let s = shared.clone(); move || change_loop(s, changed_rx) });
    spawn_named("subsessions", { let s = shared.clone(); move || subsession_loop(s) });
    // The skills and the `copilot-canvas` command that canvas sessions use for events.
    if let Err(error) = crate::cli::write_plugin(&instance.home) {
        log(&instance, &format!("cannot write the canvas plugin: {error:#}"));
    }
    if let Err(error) = crate::cli::install_cli(&instance.home) {
        log(&instance, &format!("cannot install the copilot-canvas command: {error:#}"));
    }
    spawn_named("bus", { let s = shared.clone(); move || passive::bus_loop(s, bus_rx) });
    spawn_named("idle", { let s = shared.clone(); move || idle_loop(s) });
    spawn_named("priority", { let s = shared.clone(); move || priority_loop(s, prio_rx) });

    loop {
        match server.accept() {
            Ok(pipe) => {
                let shared = shared.clone();
                spawn_named("client", move || {
                    if let Err(error) = serve_client(&shared, pipe) {
                        log(&shared.instance, &format!("client ended: {error:#}"));
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

fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    std::thread::Builder::new().name(name.into()).spawn(body).expect("spawn service thread");
}

impl Shared {
    fn broadcast(&self, message: ServiceMsg) {
        let clients: Vec<Arc<Client>> = lock(&self.clients).values().cloned().collect();
        for client in clients {
            client.send(message.clone());
        }
    }

    fn touch(&self) {
        *lock(&self.last_activity) = Instant::now();
    }

    fn terminal(&self, id: &str) -> Result<Arc<Terminal>> {
        lock(&self.terms).get(id).cloned().with_context(|| format!("no terminal {id}"))
    }

    fn launch(&self, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16) -> Result<()> {
        anyhow::ensure!(!id.is_empty() && id.len() <= 100, "invalid terminal id");
        if let Some(existing) = lock(&self.terms).get(id)
            && existing.alive.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let kind = match kind {
            LaunchKind::Copilot { new_name, prompt, resume } => {
                LaunchKind::CopilotWith { new_name, prompt, resume, args: Vec::new(), env: Vec::new() }
            }
            other => other,
        };
        let (program, args, env, copilot) = match kind {
            LaunchKind::CopilotWith { new_name, prompt, resume, args: extra, env } => {
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
                (program, args, env, true)
            }
            LaunchKind::Command { program, args } => (program, args, Vec::new(), false),
            LaunchKind::Copilot { .. } => unreachable!("normalized above"),
        };
        let terminal = Terminal::spawn(
            Spawn { id, program: &program, args: &args, env: &env, cwd, cols, rows, copilot },
            Hooks {
                pump: self.pump.clone(),
                exited: self.exited.clone(),
                clipboard: self.clipboard.clone(),
                bytes: self.bytes.clone(),
                changed: self.changed.clone(),
            },
        )?;
        log(&self.instance, &format!("launched {id} pid {}", terminal.pid));
        let info = terminal.info();
        lock(&self.terms).insert(id.to_owned(), terminal);
        self.broadcast(ServiceMsg::Terminals(vec![info]));
        let _ = self.index_wake.try_send(());
        Ok(())
    }

    fn stats(&self) -> ServiceStats {
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

    fn shutdown(self: &Arc<Self>) {
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

fn serve_client(shared: &Arc<Shared>, pipe: Pipe) -> Result<()> {
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
            proof: protocol::prove(&shared.token, "server", &client_nonce),
        },
    )?;
    let Some(ClientMsg::Auth { proof }) = read_frame(&mut reader)? else { bail!("expected auth") };
    if !protocol::verify(&shared.token, "client", &server_nonce, &proof) {
        bail!("client failed authentication");
    }

    let (out_tx, out_rx) = bounded::<ServiceMsg>(CLIENT_QUEUE);
    let id = shared.next_client.fetch_add(1, Ordering::Relaxed);
    let client = Arc::new(Client { id, out: out_tx, subs: Mutex::new(HashMap::new()), tracked: Mutex::new(HashSet::new()) });
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
    let infos: Vec<_> = lock(&shared.terms).values().map(|t| t.info()).collect();
    client.send(ServiceMsg::Terminals(infos));
    let sessions: Vec<_> = lock(&shared.sessions).values().cloned().collect();
    client.send(ServiceMsg::Sessions { full: true, upsert: sessions, remove: Vec::new() });
    for link in lock(&shared.subsessions).values() {
        client.send(link.clone());
    }
    client.send(ServiceMsg::Bus { json: passive::snapshot(shared) });
    lock(&shared.clients).insert(id, client.clone());
    shared.touch();

    let result = (|| -> Result<()> {
        while let Some(message) = read_frame::<_, ClientMsg>(&mut reader)? {
            shared.touch();
            if let Err(error) = dispatch(shared, &client, message) {
                client.send(ServiceMsg::Error { id: None, message: format!("{error:#}") });
            }
        }
        Ok(())
    })();
    lock(&shared.clients).remove(&id);
    closer.close();
    let _ = shared.index_wake.try_send(());
    result
}

fn dispatch(shared: &Arc<Shared>, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
    match message {
        ClientMsg::Track { ids } => {
            *lock(&client.tracked) = ids.into_iter().filter(|id| is_session_id(id)).collect();
            let _ = shared.index_wake.try_send(());
        }
        ClientMsg::RefreshSessions => {
            let _ = shared.index_wake.try_send(());
        }
        ClientMsg::Launch { id, kind, cwd, cols, rows } => {
            if let Err(error) = shared.launch(&id, kind, &cwd, cols, rows) {
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
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Input { id, input } => {
            shared.last_input_client.store(client.id, Ordering::Relaxed);
            if !matches!(input, crate::protocol::Input::Focus(_)) {
                shared.note_active(&id);
                passive::input_noted(shared, &id);
            }
            shared.terminal(&id)?.send_input(&input)?;
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Resize { id, cols, rows } => {
            shared.terminal(&id)?.resize(cols, rows)?;
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Scroll { id, delta } => {
            shared.terminal(&id)?.scroll(delta);
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Wheel { id, up, col, row, mods } => {
            shared.last_input_client.store(client.id, Ordering::Relaxed);
            shared.terminal(&id)?.wheel(up, col, row, mods)?;
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Click { id, col, row, button, mods } => {
            shared.last_input_client.store(client.id, Ordering::Relaxed);
            shared.terminal(&id)?.click(button, col, row, mods)?;
            let _ = shared.pump.try_send(());
        }
        ClientMsg::Routing { json } => {
            let routing: crate::bus::Routing = serde_json::from_str(&json).context("invalid routing")?;
            let mut current = lock(&shared.routing);
            if *current != routing {
                passive::save_routing(&shared.instance.home, &routing);
                *current = routing;
            }
        }
        ClientMsg::Bus { json } => passive::action(shared, &json, &shared.bus_tx),
        ClientMsg::Stop { id, force } => shared.terminal(&id)?.stop(force),
        ClientMsg::Forget { id } => {
            let mut terms = lock(&shared.terms);
            if terms.get(&id).is_some_and(|t| !t.alive.load(Ordering::Acquire)) {
                terms.remove(&id);
                drop(terms);
                shared.broadcast(ServiceMsg::TerminalGone(id));
            }
        }
        ClientMsg::Shutdown => {
            let shared = shared.clone();
            spawn_named("shutdown", move || shared.shutdown());
        }
        ClientMsg::Stats => {
            client.send(ServiceMsg::Stats(shared.stats()));
        }
        ClientMsg::Hello { .. } | ClientMsg::Auth { .. } => bail!("unexpected handshake message"),
    }
    Ok(())
}

fn pump(shared: Arc<Shared>, wake: Receiver<()>) {
    let mut next_allowed = Instant::now();
    loop {
        let _ = wake.recv_timeout(Duration::from_millis(50));
        let now = Instant::now();
        if now < next_allowed {
            std::thread::sleep(next_allowed - now);
        }
        // Complete synchronized updates whose end marker never arrived.
        let terms: Vec<Arc<Terminal>> = lock(&shared.terms).values().cloned().collect();
        for terminal in &terms {
            let mut inner = lock(&terminal.inner);
            if inner.parser.sync_timeout().sync_timeout().is_some_and(|deadline| Instant::now() >= deadline) {
                let term::Inner { term, parser, generation } = &mut *inner;
                parser.stop_sync(term);
                *generation += 1;
            }
        }
        let clients: Vec<Arc<Client>> = lock(&shared.clients).values().cloned().collect();
        for client in clients {
            // Backpressure: a slow client gets fewer, newer frames instead of a growing queue.
            if client.out.len() > CLIENT_QUEUE / 2 {
                continue;
            }
            let mut subs = lock(&client.subs);
            for (id, sub) in subs.iter_mut() {
                let Some(terminal) = lock(&shared.terms).get(id).cloned() else { continue };
                let (generation, snap) = {
                    let inner = lock(&terminal.inner);
                    if inner.generation == sub.generation {
                        continue;
                    }
                    (inner.generation, snapshot(&inner))
                };
                let hashes: Vec<u64> = snap.lines.iter().map(|runs| row_hash(runs)).collect();
                let full = sub.cols != snap.cols || sub.rows != snap.rows || sub.hashes.len() != hashes.len();
                let changed: Vec<(u16, Vec<crate::protocol::Run>)> = snap
                    .lines
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
                        shared.frames.fetch_add(1, Ordering::Relaxed);
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

impl Shared {
    fn note_active(&self, id: &str) {
        let mut active = lock(&self.active_input);
        if active.as_deref() != Some(id) {
            *active = Some(id.to_owned());
            drop(active);
            let _ = self.priority_wake.try_send(());
        }
    }
}

/// Keeps the session you type in responsive on a busy machine: its whole
/// process tree (Copilot, tools, MCP servers, builds) runs above normal,
/// sessions shown on screen at normal, all others below normal. Background
/// sessions still use all spare CPU; they just yield to you. Only processes
/// inside our own sessions are touched. `COPILOT_CANVAS_NO_PRIORITY=1` turns
/// it off.
fn priority_loop(shared: Arc<Shared>, wake: Receiver<()>) {
    use windows_sys::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};
    if std::env::var_os("COPILOT_CANVAS_NO_PRIORITY").is_some() {
        return;
    }
    let mut applied: HashMap<u32, u32> = HashMap::new();
    loop {
        // Re-check every 2 s so new child processes (tools, builds) follow their session.
        let _ = wake.recv_timeout(Duration::from_secs(2));
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let terms: Vec<(String, u32)> =
            lock(&shared.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).map(|t| (t.id.clone(), t.pid)).collect();
        if terms.is_empty() {
            applied.clear();
            continue;
        }
        let active = lock(&shared.active_input).clone();
        let visible: HashSet<String> = lock(&shared.clients).values().flat_map(|c| lock(&c.subs).keys().cloned().collect::<Vec<_>>()).collect();
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
fn subsession_loop(shared: Arc<Shared>) {
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

fn index_loop(shared: Arc<Shared>, wake: Receiver<()>) {
    let mut index = Index::new(shared.instance.sessions_dir.clone());
    loop {
        let tracked: HashSet<String> = lock(&shared.clients)
            .values()
            .flat_map(|c| lock(&c.tracked).iter().cloned().collect::<Vec<_>>())
            .chain(lock(&shared.terms).values().filter(|t| t.copilot).map(|t| t.id.clone()))
            .collect();
        let (changed, removed) = index.poll(&tracked, Duration::from_secs(15));
        for (session, kind, data) in index.take_detected() {
            log(&shared.instance, &format!("detected {kind} in {session}"));
            let _ = shared.bus_tx.send(crate::bus::Event { kind, source: session, data, ..Default::default() });
        }
        for warning in index.warnings.drain(..) {
            log(&shared.instance, &warning);
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

fn exit_loop(shared: Arc<Shared>, exited: Receiver<String>) {
    for id in exited {
        if let Some(terminal) = lock(&shared.terms).get(&id).cloned() {
            log(&shared.instance, &format!("terminal {id} exited with {:?}", *lock(&terminal.exit_code)));
            shared.broadcast(ServiceMsg::Terminals(vec![terminal.info()]));
        }
        let _ = shared.index_wake.try_send(());
    }
}

/// Resends terminal info when a title, progress state or bell changes;
/// bursts (a title per keystroke, progress toggles) are coalesced.
fn change_loop(shared: Arc<Shared>, changed: Receiver<String>) {
    while let Ok(first) = changed.recv() {
        std::thread::sleep(Duration::from_millis(100));
        let mut ids = HashSet::from([first]);
        ids.extend(changed.try_iter());
        let infos: Vec<crate::protocol::TermInfo> = {
            let terms = lock(&shared.terms);
            ids.iter().filter_map(|id| terms.get(id).map(|t| t.info())).collect()
        };
        if !infos.is_empty() {
            shared.broadcast(ServiceMsg::Terminals(infos));
        }
    }
}

fn clipboard_loop(shared: Arc<Shared>, clipboard: Receiver<String>) {
    for text in clipboard {
        let target = shared.last_input_client.load(Ordering::Relaxed);
        if let Some(client) = lock(&shared.clients).get(&target) {
            client.send(ServiceMsg::Clipboard(text));
        }
    }
}

fn idle_loop(shared: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_secs(20));
        let alive = lock(&shared.terms).values().any(|t| t.alive.load(Ordering::Acquire));
        let clients = !lock(&shared.clients).is_empty();
        if !alive && !clients && lock(&shared.last_activity).elapsed() >= IDLE_EXIT && !passive::has_work(&shared) {
            log(&shared.instance, "idle with no clients or terminals; exiting");
            let _ = std::fs::remove_file(shared.instance.token_path());
            std::process::exit(0);
        }
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
