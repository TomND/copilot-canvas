//! The terminal host server (L1). Owns every hosted terminal so sessions
//! survive client restarts; streams only visible terminals to clients as row
//! diffs; keeps the session you type in responsive (priority manager).
//!
//! Clients connect over the named pipe (local) or, when `remote.listen` is
//! set, over TCP on loopback (remote clients behind a Dev Tunnel). Both carry
//! the same framed protocol and handshake. Protocol-8 clients send and get
//! heartbeats, number their input (applied at most once per client session)
//! and, over TCP, get frames coalesced per tile and optionally compressed.
//!
//! Everything that is not about terminals (session history, the canvas
//! document, the event bus, …) is delegated to a [`HostApp`], the L3 backend
//! that shares this process.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};

use crate::emulator::{Engine, Snapshot, SnapshotImage};
use crate::paths::{Instance, build_id, now_ms};
use crate::pipe::{Pipe, PipeServer};
use crate::protocol::{self, CAP_COMPRESS, CAP_IMAGES, ClientMsg, Frame8, IMAGE_PNG, Input, LaunchKind, Placement, Run8, ServiceMsg, ServiceStats};
use crate::term::{Hooks, Notice, Spawn, Terminal, lock};
use canvas_common::wire::{Beat, Closer, FrameReader, FrameWriter, Liveness};
use canvas_common::{handshake, tcp};

const FRAME_INTERVAL: Duration = Duration::from_millis(4);
/// Remote clients: the focused tile at most every 16 ms, others every 100 ms.
const FOCUSED_INTERVAL: Duration = Duration::from_millis(16);
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(100);
const PUMP_POLL: Duration = Duration::from_millis(50);
/// TCP clients that have not authenticated by then are dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
/// Client sessions remembered for input deduplication.
const MAX_INPUT_SESSIONS: usize = 256;
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
    /// A terminal's program exited (after `terminals_changed`).
    fn terminal_exited(&self, _id: &str) {}
    /// A terminal is about to be stopped gracefully; may block briefly (it
    /// runs on its own thread).
    fn terminal_stopping(&self, _id: &str) {}
    /// Pending work that keeps the service running without clients or terminals.
    fn has_work(&self) -> bool;
    /// The secret of a paired remote client (`ClientInfo::remote_id`); `None` refuses it.
    fn remote_secret(&self, id: &str) -> Option<Vec<u8>>;
    /// The service is about to exit: persist what must not be lost.
    fn shutdown(&self) {}
}

/// What a client last received of one subscribed terminal.
#[derive(Default)]
struct Sub {
    generation: u64,
    hashes: Vec<u64>,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16)>,
    offset: u32,
    shape: u8,
    blink: bool,
    modes: u32,
    /// When the last frame went out (remote frames are coalesced).
    sent_at: Option<Instant>,
    /// Image placements last sent (CAP_IMAGES).
    placements: Vec<Placement>,
    /// Images of the current placements: id -> (generation, delivered).
    images: HashMap<u32, (u64, bool)>,
}

/// Keyboard input applied for one client session; outlives its connections.
#[derive(Debug)]
struct InputSession {
    /// Highest `Input8::seq` applied.
    applied: u64,
    /// The connection allowed to apply input (the newest one).
    owner: u64,
    seen: Instant,
}

impl InputSession {
    /// Whether `client` may apply input `seq` now (each seq at most once, only
    /// from the session's current connection); records it.
    fn admit(&mut self, client: u64, seq: u64) -> bool {
        if self.owner != client || seq <= self.applied {
            return false;
        }
        self.applied = seq;
        true
    }
}

/// One connected client.
pub struct Client {
    pub id: u64,
    /// Negotiated protocol: 7 (`Hello`) or 8 (`Hello8`).
    pub protocol: u32,
    /// Connected over TCP: frames are coalesced per tile.
    pub remote: bool,
    /// `CAP_*` bits from the handshake.
    caps: u32,
    out: Sender<ServiceMsg>,
    subs: Mutex<HashMap<String, Sub>>,
    /// The focused tile (`Subscribe8`).
    focused: Mutex<Option<String>>,
    session: Option<Arc<Mutex<InputSession>>>,
    liveness: Option<Arc<Liveness>>,
    closer: Closer,
}

impl Client {
    fn new(id: u64, protocol: u32, remote: bool, caps: u32, out: Sender<ServiceMsg>, closer: Closer) -> Self {
        Self {
            id,
            protocol,
            remote,
            caps,
            out,
            subs: Mutex::new(HashMap::new()),
            focused: Mutex::new(None),
            session: None,
            liveness: (protocol >= 8).then(|| Arc::new(Liveness::standard())),
            closer,
        }
    }

    /// Queues a message in the shape this client reads; false when the client is gone.
    pub fn send(&self, message: ServiceMsg) -> bool {
        !matches!(self.try_send(message), Err(TrySendError::Disconnected(_)))
    }

    fn try_send(&self, message: ServiceMsg) -> Result<(), TrySendError<ServiceMsg>> {
        match message.shaped_for(self.protocol, self.caps) {
            Some(message) => self.out.try_send(message),
            None => Ok(()),
        }
    }

    /// Ends the connection.
    pub fn close(&self) {
        self.closer.close();
    }

    /// Highest `Input8::seq` applied for this client's session.
    pub fn input_ack(&self) -> u64 {
        self.session.as_ref().map(|s| lock(s).applied).unwrap_or(0)
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
    /// Desktop notifications with their text.
    notify: Sender<Notice>,
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
    fallback_logged: AtomicBool,
    /// Compress frames for remote clients that accept it (`remote.compress`).
    compress: bool,
    /// Input deduplication per client session id.
    input_sessions: Mutex<HashMap<Vec<u8>, Arc<Mutex<InputSession>>>>,
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

/// The engine new terminals use: `COPILOT_CANVAS_ENGINE`, else
/// `terminal.engine` in settings.json, else the build's default (Ghostty when
/// it is compiled in).
pub fn configured_engine(instance: &Instance) -> Engine {
    canvas_common::settings::Settings::load(&instance.home)
        .engine()
        .and_then(|name| Engine::parse(&name))
        .filter(|engine| engine.available())
        .unwrap_or_else(Engine::default_engine)
}

pub fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    std::thread::Builder::new().name(name.into()).spawn(body).expect("spawn service thread");
}

/// Runs the service: binds the pipe (and the TCP port when remote listening
/// is configured), starts the host threads, lets `make_app` build the
/// backend, then serves clients until shutdown.
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
    // A port file left by a service that did not exit cleanly.
    let _ = std::fs::remove_file(instance.port_path());
    let settings = canvas_common::settings::Settings::load(&instance.home);
    log(&instance, &format!("service started, build {}", build_id()));

    let (pump_tx, pump_rx) = bounded::<()>(1);
    let (exited_tx, exited_rx) = unbounded::<String>();
    let (clip_tx, clip_rx) = bounded::<String>(16);
    let (changed_tx, changed_rx) = bounded::<String>(256);
    let (notify_tx, notify_rx) = bounded::<Notice>(64);
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
        notify: notify_tx,
        last_input_client: AtomicU64::new(0),
        active_input: Mutex::new(None),
        priority_wake: prio_tx,
        bytes: Arc::new(AtomicU64::new(0)),
        frames: AtomicU64::new(0),
        last_activity: Mutex::new(Instant::now()),
        started: Instant::now(),
        shutting_down: AtomicBool::new(false),
        engine: configured_engine(&instance),
        fallback_logged: AtomicBool::new(false),
        compress: settings.compress(),
        input_sessions: Mutex::new(HashMap::new()),
        app: OnceLock::new(),
    });
    let app = make_app(&host);
    let _ = host.app.set(app);

    spawn_named("pump", { let h = host.clone(); move || pump(h, pump_rx) });
    spawn_named("exits", { let h = host.clone(); move || exit_loop(h, exited_rx) });
    spawn_named("clipboard", { let h = host.clone(); move || clipboard_loop(h, clip_rx) });
    spawn_named("changes", { let h = host.clone(); move || change_loop(h, changed_rx) });
    spawn_named("notifications", { let h = host.clone(); move || notify_loop(h, notify_rx) });
    spawn_named("idle", { let h = host.clone(); move || idle_loop(h) });
    spawn_named("priority", { let h = host.clone(); move || priority_loop(h, prio_rx) });
    spawn_named("liveness", { let h = host.clone(); move || liveness_loop(h) });
    if let Some(addr) = settings.listen() {
        listen_tcp(&host, &addr);
    }

    loop {
        match server.accept() {
            Ok(pipe) => {
                let host = host.clone();
                spawn_named("client", move || {
                    if let Err(error) = serve_client(&host, Link::Pipe(pipe)) {
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

/// Also serves clients on a loopback TCP port (non-loopback addresses are
/// refused) and publishes the port in `<home>/service-<instance>.port`.
fn listen_tcp(host: &Arc<Host>, addr: &str) {
    let listener = match tcp::listen_loopback(addr) {
        Ok(listener) => listener,
        Err(error) => {
            log(&host.instance, &format!("not listening for remote clients on {addr}: {error}"));
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let path = host.instance.port_path();
    let temp = path.with_extension(format!("port.{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&temp, port.to_string()).and_then(|_| std::fs::rename(&temp, &path)) {
        log(&host.instance, &format!("cannot publish the remote port: {error}"));
    }
    log(&host.instance, &format!("listening for remote clients on 127.0.0.1:{port}"));
    let host = host.clone();
    spawn_named("tcp-accept", move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let host = host.clone();
                    spawn_named("remote-client", move || {
                        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                        if let Err(error) = serve_client(&host, Link::Tcp(stream)) {
                            log(&host.instance, &format!("remote client {peer} ended: {error:#}"));
                        }
                    });
                }
                Err(error) => {
                    log(&host.instance, &format!("remote accept failed: {error}"));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
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
        let clients: Vec<Arc<Client>> = lock(&self.clients).values().cloned().collect();
        clients.iter().flat_map(|c| lock(&c.subs).keys().cloned().collect::<Vec<_>>()).collect()
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
                engine: self.effective_engine(),
            },
            Hooks {
                pump: self.pump.clone(),
                exited: self.exited.clone(),
                clipboard: self.clipboard.clone(),
                bytes: self.bytes.clone(),
                changed: self.changed.clone(),
                notify: Some(self.notify.clone()),
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
            self.app().terminal_stopping(&terminal.id);
            terminal.stop(false);
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline && terms.iter().any(|t| t.alive.load(Ordering::Acquire)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        for terminal in &terms {
            terminal.force_kill();
        }
        self.app().shutdown();
        self.remove_published_files();
        log(&self.instance, "service exiting");
        std::process::exit(0);
    }

    /// The token and port files are only valid while this process runs.
    fn remove_published_files(&self) {
        let _ = std::fs::remove_file(self.instance.token_path());
        let _ = std::fs::remove_file(self.instance.port_path());
    }

    /// The configured engine, or alacritty once Ghostty failed to start (logged once).
    fn effective_engine(&self) -> Engine {
        if self.engine == Engine::Ghostty && !Engine::Ghostty.available() {
            if !self.fallback_logged.swap(true, Ordering::AcqRel) {
                log(&self.instance, "the ghostty engine failed to start; new terminals use alacritty");
            }
            return Engine::Alacritty;
        }
        self.engine
    }

    /// Stops a terminal; a graceful stop first lets the backend prepare the
    /// agent (on a helper thread, so the client's messages keep flowing).
    pub fn stop_terminal(self: &Arc<Self>, id: &str, force: bool) -> Result<()> {
        let terminal = self.terminal(id)?;
        if force || !terminal.copilot {
            terminal.stop(force);
            return Ok(());
        }
        let host = self.clone();
        spawn_named("stop", move || {
            host.app().terminal_stopping(&terminal.id);
            terminal.stop(false);
        });
        Ok(())
    }

    fn note_active(&self, id: &str) {
        let mut active = lock(&self.active_input);
        if active.as_deref() != Some(id) {
            *active = Some(id.to_owned());
            drop(active);
            let _ = self.priority_wake.try_send(());
        }
    }

    /// The input state of a client session (created on first use; the least
    /// recently connected session is forgotten beyond `MAX_INPUT_SESSIONS`).
    fn input_session(&self, key: &[u8]) -> Arc<Mutex<InputSession>> {
        let mut sessions = lock(&self.input_sessions);
        if sessions.len() >= MAX_INPUT_SESSIONS
            && !sessions.contains_key(key)
            && let Some(oldest) = sessions.iter().min_by_key(|(_, s)| lock(s).seen).map(|(k, _)| k.clone())
        {
            sessions.remove(&oldest);
        }
        sessions
            .entry(key.to_vec())
            .or_insert_with(|| Arc::new(Mutex::new(InputSession { applied: 0, owner: 0, seen: Instant::now() })))
            .clone()
    }
}

/// A client transport.
enum Link {
    Pipe(Pipe),
    Tcp(TcpStream),
}

type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>, Closer);

impl Link {
    fn split(self) -> std::io::Result<Halves> {
        Ok(match self {
            Link::Pipe(pipe) => {
                let (reader, writer, closer) = pipe.split()?;
                (Box::new(reader), Box::new(writer), Closer::new(move || closer.close()))
            }
            Link::Tcp(stream) => {
                let (reader, writer, closer) = tcp::split(stream)?;
                (Box::new(reader), Box::new(writer), Closer::new(move || closer.close()))
            }
        })
    }
}

fn serve_client(host: &Arc<Host>, link: Link) -> Result<()> {
    let remote = matches!(link, Link::Tcp(_));
    let (reader, writer, closer) = link.split()?;
    let mut reader = FrameReader::new(reader);
    let mut writer = FrameWriter::new(writer);
    let authenticated = Arc::new(AtomicBool::new(false));
    if remote {
        let (done, closer) = (authenticated.clone(), closer.clone());
        spawn_named("handshake-timeout", move || {
            std::thread::sleep(HANDSHAKE_TIMEOUT);
            if !done.load(Ordering::Acquire) {
                closer.close();
            }
        });
    }
    let accepted = handshake::accept(&mut reader, &mut writer, remote, build_id(), |remote_id| match remote_id {
        Some(id) => host.app().remote_secret(id),
        None => Some(host.token.clone()),
    });
    authenticated.store(true, Ordering::Release);
    let accepted = match accepted {
        Ok(accepted) => accepted,
        Err(error) => {
            closer.close();
            return Err(error);
        }
    };
    let v8 = accepted.protocol >= 8;
    let info = accepted.client.unwrap_or_default();
    if remote && v8 {
        reader.accept_compressed()?;
        if host.compress && info.caps & CAP_COMPRESS != 0 {
            writer.compress()?;
        }
    }

    let (out_tx, out_rx) = bounded::<ServiceMsg>(CLIENT_QUEUE);
    let id = host.next_client.fetch_add(1, Ordering::Relaxed);
    let mut client = Client::new(id, accepted.protocol, remote, info.caps, out_tx, closer.clone());
    // The newest connection of a client session owns its input; the previous
    // one (half-open after a network drop) stops applying input and is closed.
    let mut ready_ack = 0;
    if v8 {
        let session = host.input_session(&info.session);
        let previous = {
            let mut state = lock(&session);
            state.seen = Instant::now();
            ready_ack = state.applied;
            std::mem::replace(&mut state.owner, id)
        };
        let replaced = lock(&host.clients).get(&previous).cloned();
        if let Some(old) = replaced {
            log(&host.instance, &format!("client {previous} replaced by its reconnection {id}"));
            old.close();
        }
        client.session = Some(session);
    }
    let client = Arc::new(client);
    if let Some(liveness) = &client.liveness {
        liveness.received();
    }
    spawn_writer(writer, out_rx, &client);
    if remote {
        log(&host.instance, &format!("remote client {id} connected ({}, protocol {}, compression {})", info.name, accepted.protocol, host.compress && info.caps & CAP_COMPRESS != 0));
    }
    client.send(ServiceMsg::Ready8 { pid: std::process::id(), input_ack: ready_ack });
    let infos: Vec<_> = lock(&host.terms).values().map(|t| t.info()).collect();
    client.send(ServiceMsg::Terminals(infos));
    host.app().client_connected(&client);
    lock(&host.clients).insert(id, client.clone());
    host.touch();

    let result = (|| -> Result<()> {
        while let Some(message) = reader.recv::<ClientMsg>()? {
            if let Some(liveness) = &client.liveness {
                liveness.received();
            }
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
    if remote {
        log(&host.instance, &format!("remote client {id} disconnected"));
    }
    result
}

/// Writes queued messages; protocol-8 clients also get a heartbeat every 2 s.
fn spawn_writer(mut writer: FrameWriter, out: Receiver<ServiceMsg>, client: &Arc<Client>) {
    let heartbeat = client.liveness.as_ref().map(|l| l.interval);
    let session = client.session.clone();
    let closer = client.closer.clone();
    spawn_named("client-writer", move || {
        let mut beat = heartbeat.map(Beat::new);
        loop {
            let message = match &beat {
                Some(beat) => match out.recv_deadline(beat.next()) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match out.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                },
            };
            if let Some(message) = message
                && writer.send(&message).is_err()
            {
                break;
            }
            if beat.as_mut().is_some_and(|b| b.due(Instant::now())) {
                let input_ack = session.as_ref().map(|s| lock(s).applied).unwrap_or(0);
                if writer.send(&ServiceMsg::Heartbeat { input_ack }).is_err() {
                    break;
                }
            }
        }
        closer.close();
    });
}

/// Closes protocol-8 connections that stayed silent too long (no heartbeat).
fn liveness_loop(host: Arc<Host>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let clients: Vec<Arc<Client>> = lock(&host.clients).values().cloned().collect();
        for client in clients {
            if let Some(liveness) = &client.liveness
                && liveness.dead()
            {
                log(&host.instance, &format!("client {} silent for {:.1} s; closing", client.id, liveness.silence().as_secs_f32()));
                client.close();
            }
        }
    }
}

fn subscribe(host: &Host, client: &Client, ids: Vec<String>, focused: Option<String>) {
    let mut subs = lock(&client.subs);
    subs.retain(|id, _| ids.contains(id));
    for id in ids.into_iter().take(16) {
        subs.entry(id).or_default();
    }
    drop(subs);
    *lock(&client.focused) = focused;
    let _ = host.pump.try_send(());
}

fn apply_input(host: &Host, client: &Client, id: &str, input: &Input) -> Result<()> {
    host.last_input_client.store(client.id, Ordering::Relaxed);
    if !matches!(input, Input::Focus(_)) {
        host.note_active(id);
        host.app().input_noted(id);
    }
    host.terminal(id)?.send_input(input)?;
    let _ = host.pump.try_send(());
    Ok(())
}

fn dispatch(host: &Arc<Host>, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
    match message {
        ClientMsg::Launch { id, kind, cwd, cols, rows } => {
            if let Err(error) = host.launch(&id, kind, &cwd, cols, rows) {
                client.send(ServiceMsg::Error { id: Some(id), message: format!("{error:#}") });
            }
        }
        ClientMsg::Subscribe { ids } => subscribe(host, client, ids, None),
        ClientMsg::Subscribe8 { ids, focused } => subscribe(host, client, ids, focused),
        ClientMsg::Input { id, input } => apply_input(host, client, &id, &input)?,
        ClientMsg::Input8 { id, input, seq } => match &client.session {
            Some(session) => {
                // Held while applying: a reconnection's Ready8 ack is exact.
                let mut state = lock(session);
                if state.admit(client.id, seq) {
                    apply_input(host, client, &id, &input)?;
                }
            }
            None => apply_input(host, client, &id, &input)?,
        },
        ClientMsg::Heartbeat => {}
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
        ClientMsg::Stop { id, force } => host.stop_terminal(&id, force)?,
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
        ClientMsg::Hello { .. } | ClientMsg::Hello8 { .. } | ClientMsg::Auth { .. } => bail!("unexpected handshake message"),
        other => host.app().message(client, other)?,
    }
    Ok(())
}

/// Row hash over cells and hyperlink targets (indices differ between snapshots).
fn row_hash8(row: &[Run8], links: &[String]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for run in row {
        (run.col, &run.text, &run.clusters, run.wide, run.fg, run.bg, run.flags, run.underline).hash(&mut hasher);
        let target = (run.link > 0).then(|| links.get(run.link as usize - 1)).flatten();
        target.hash(&mut hasher);
    }
    row.len().hash(&mut hasher);
    hasher.finish()
}

/// Rewrites a row's link indices into `table` (targets referenced by the frame).
fn relink(row: &mut [Run8], targets: &[String], table: &mut Vec<String>) {
    for run in row.iter_mut().filter(|r| r.link > 0) {
        run.link = match targets.get(run.link as usize - 1) {
            Some(target) => match table.iter().position(|t| t == target) {
                Some(index) => index as u32 + 1,
                None => {
                    table.push(target.clone());
                    table.len() as u32
                }
            },
            None => 0,
        };
    }
}

/// The frame bringing `sub` up to `snap` and the subscription state after it
/// was sent, or `None` when nothing a client sees changed. `hashes` are the
/// snapshot's row hashes.
fn diff_frame(id: &str, sub: &Sub, snap: Snapshot, hashes: Vec<u64>) -> Option<(Frame8, Sub)> {
    let Snapshot { cols, rows, lines, links: targets, cursor, cursor_shape, cursor_blink, display_offset, history, modes, .. } = snap;
    let full = sub.cols != cols || sub.rows != rows || sub.hashes.len() != hashes.len();
    let same_rows = !full && sub.hashes == hashes;
    let same_state = sub.cursor == cursor && sub.offset == display_offset && sub.shape == cursor_shape && sub.blink == cursor_blink && sub.modes == modes;
    if same_rows && same_state {
        return None;
    }
    let mut links = Vec::new();
    let changed: Vec<(u16, Vec<Run8>)> = lines
        .into_iter()
        .enumerate()
        .filter(|(y, _)| full || sub.hashes[*y] != hashes[*y])
        .map(|(y, mut row)| {
            relink(&mut row, &targets, &mut links);
            (y as u16, row)
        })
        .collect();
    let next = Sub {
        generation: sub.generation,
        hashes,
        cols,
        rows,
        cursor,
        offset: display_offset,
        shape: cursor_shape,
        blink: cursor_blink,
        modes,
        sent_at: sub.sent_at,
        placements: sub.placements.clone(),
        images: sub.images.clone(),
    };
    let frame = Frame8 { id: id.to_owned(), cols, rows, full, changed, cursor, cursor_shape, cursor_blink, display_offset, history, modes, links };
    Some((frame, next))
}

fn pump(host: Arc<Host>, wake: Receiver<()>) {
    let mut next_allowed = Instant::now();
    // When a coalesced (remote) tile may get its next frame.
    let mut due: Option<Instant> = None;
    loop {
        let wait = due.map(|d| d.saturating_duration_since(Instant::now())).unwrap_or(PUMP_POLL).min(PUMP_POLL);
        let _ = wake.recv_timeout(wait);
        let now = Instant::now();
        if now < next_allowed {
            std::thread::sleep(next_allowed - now);
        }
        due = None;
        let clients: Vec<Arc<Client>> = lock(&host.clients).values().cloned().collect();
        for client in clients {
            if let Some(at) = pump_client(&host, &client) {
                due = Some(due.map_or(at, |d| d.min(at)));
            }
        }
        next_allowed = Instant::now() + FRAME_INTERVAL;
    }
}

/// Sends one client the frames its subscriptions need. Never holds a lock
/// while snapshotting a terminal. Returns when a coalesced tile is due.
fn pump_client(host: &Host, client: &Client) -> Option<Instant> {
    // Backpressure: a slow client gets fewer, newer frames instead of a growing queue.
    if client.out.len() > CLIENT_QUEUE / 2 {
        return None;
    }
    let focused = lock(&client.focused).clone();
    let wanted: Vec<(String, u64, Option<Instant>)> = lock(&client.subs).iter().map(|(id, s)| (id.clone(), s.generation, s.sent_at)).collect();
    let mut due: Option<Instant> = None;
    for (id, seen, sent_at) in wanted {
        let terminal = lock(&host.terms).get(&id).cloned();
        let Some(terminal) = terminal else { continue };
        let generation = terminal.generation();
        if generation == seen {
            continue;
        }
        if client.remote
            && let Some(at) = sent_at
        {
            let interval = if focused.as_deref() == Some(id.as_str()) { FOCUSED_INTERVAL } else { BACKGROUND_INTERVAL };
            let ready = at + interval;
            if Instant::now() < ready {
                due = Some(due.map_or(ready, |d| d.min(ready)));
                continue;
            }
        }
        let Some(mut snap) = terminal.snapshot() else { continue };
        let images = std::mem::take(&mut snap.images);
        if client.caps & CAP_IMAGES != 0 && !send_images(client, &terminal, &id, &images) {
            break;
        }
        let hashes: Vec<u64> = snap.lines.iter().map(|row| row_hash8(row, &snap.links)).collect();
        let mut subs = lock(&client.subs);
        // Unsubscribed while we took the snapshot.
        let Some(sub) = subs.get_mut(&id) else { continue };
        let Some((frame, next)) = diff_frame(&id, sub, snap, hashes) else {
            sub.generation = generation;
            continue;
        };
        // Not queued (full or gone): the client keeps what it had; retried next pass.
        if client.try_send(ServiceMsg::Frame8(frame)).is_err() {
            break;
        }
        host.frames.fetch_add(1, Ordering::Relaxed);
        *sub = Sub { generation, sent_at: Some(Instant::now()), ..next };
    }
    due
}

/// Sends a `CAP_IMAGES` client the images a terminal's placements need that
/// it does not have, then the placements when they changed. Images that
/// cannot be sent (gone, too large) are remembered with their generation and
/// left out of the placements. False when the client's queue is full.
fn send_images(client: &Client, terminal: &Terminal, id: &str, images: &[SnapshotImage]) -> bool {
    let (known, shown) = {
        let subs = lock(&client.subs);
        let Some(sub) = subs.get(id) else { return true };
        (sub.images.clone(), sub.placements.clone())
    };
    let placements: Vec<Placement> = images.iter().map(|i| i.placement.clone()).collect();
    let current = |i: &SnapshotImage| known.get(&i.placement.image_id).is_some_and(|(generation, _)| *generation == i.generation);
    if placements == shown && images.iter().all(current) {
        return true;
    }
    // Only images placed now are kept: the client may forget the others,
    // and they are sent again when placed again.
    let mut have: HashMap<u32, (u64, bool)> = HashMap::new();
    for image in images {
        let image_id = image.placement.image_id;
        if have.contains_key(&image_id) {
            continue;
        }
        if current(image) {
            have.insert(image_id, known[&image_id]);
            continue;
        }
        let delivered = match terminal.image_png(image_id, image.generation) {
            Some(encoded) => {
                let message = ServiceMsg::Image { term: id.to_owned(), image_id, width: encoded.width, height: encoded.height, rgba_or_png: encoded.png.to_vec(), format: IMAGE_PNG };
                if client.try_send(message).is_err() {
                    return false;
                }
                true
            }
            None => false,
        };
        have.insert(image_id, (image.generation, delivered));
    }
    let visible: Vec<Placement> = placements.iter().filter(|p| have.get(&p.image_id).is_some_and(|(_, ok)| *ok)).cloned().collect();
    if client.try_send(ServiceMsg::Placements { term: id.to_owned(), placements: visible }).is_err() {
        return false;
    }
    if let Some(sub) = lock(&client.subs).get_mut(id) {
        sub.images = have;
        sub.placements = placements;
    }
    true
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
        // Not holding `terms` while broadcasting (lock order: clients before terms).
        let terminal = lock(&host.terms).get(&id).cloned();
        if let Some(terminal) = terminal {
            let (code, raw) = (*lock(&terminal.exit_code), *lock(&terminal.raw_exit_code));
            let note = match (raw != code, terminal.was_forced()) {
                (_, true) => format!(" (stopped; process code {}, killed after the grace period)", raw.unwrap_or_default()),
                (true, false) => format!(" (stopped; process code {})", raw.unwrap_or_default()),
                (false, false) => String::new(),
            };
            log(&host.instance, &format!("terminal {id} exited with {code:?}{note}"));
            host.broadcast(ServiceMsg::Terminals(vec![terminal.info()]));
        }
        host.app().terminals_changed();
        host.app().terminal_exited(&id);
    }
}

/// Delivers programs' desktop notifications to the clients that show them.
fn notify_loop(host: Arc<Host>, notices: Receiver<Notice>) {
    for (term, title, body) in notices {
        host.broadcast(ServiceMsg::Notification { term, title, body });
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
        let client = lock(&host.clients).get(&target).cloned();
        if let Some(client) = client {
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
            host.app().shutdown();
            host.remove_published_files();
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CAP_FRAME8, SERVICE_V7_VARIANTS, TermInfo};

    fn client(protocol: u32, caps: u32) -> (Client, Receiver<ServiceMsg>) {
        let (tx, rx) = bounded(64);
        (Client::new(1, protocol, false, caps, tx, Closer::new(|| {})), rx)
    }

    fn snapshot(rows: &[&str], link_row: Option<usize>) -> Snapshot {
        let lines = rows
            .iter()
            .enumerate()
            .map(|(y, text)| vec![Run8 { col: 0, text: (*text).into(), link: if link_row == Some(y) { 2 } else { 0 }, ..Default::default() }])
            .collect();
        Snapshot {
            cols: 10,
            rows: rows.len() as u16,
            lines,
            links: vec!["https://unused".into(), "https://used".into()],
            cursor: Some((0, 0)),
            ..Default::default()
        }
    }

    fn frame_of(sub: &mut Sub, snap: Snapshot) -> Option<Frame8> {
        let hashes = snap.lines.iter().map(|row| row_hash8(row, &snap.links)).collect();
        let (frame, next) = diff_frame("t", sub, snap, hashes)?;
        *sub = next;
        Some(frame)
    }

    #[test]
    fn protocol7_clients_are_never_sent_protocol8_messages() {
        let (old, rx) = client(7, CAP_FRAME8);
        let frame = frame_of(&mut Sub::default(), snapshot(&["a", "b"], Some(1))).unwrap();
        for message in [
            ServiceMsg::Ready8 { pid: 1, input_ack: 0 },
            ServiceMsg::Terminals(vec![TermInfo::default()]),
            ServiceMsg::Frame8(frame.clone()),
            ServiceMsg::Heartbeat { input_ack: 3 },
            ServiceMsg::Clipboard("x".into()),
        ] {
            assert!(old.send(message));
        }
        let received: Vec<ServiceMsg> = rx.try_iter().collect();
        assert_eq!(received.len(), 4, "the heartbeat is dropped");
        for message in &received {
            assert!(postcard::to_stdvec(message).unwrap()[0] < SERVICE_V7_VARIANTS, "protocol-7 client got {message:?}");
        }
        assert!(matches!(&received[0], ServiceMsg::Ready { .. }));
        let ServiceMsg::Frame(v7) = &received[2] else { panic!("protocol-7 frame") };
        assert_eq!(v7.changed[1].1[0].link.as_deref(), Some("https://used"));
        let (new, rx) = client(8, CAP_FRAME8);
        new.send(ServiceMsg::Frame8(frame));
        new.send(ServiceMsg::Heartbeat { input_ack: 3 });
        assert!(matches!(rx.try_recv(), Ok(ServiceMsg::Frame8(_))));
        assert!(matches!(rx.try_recv(), Ok(ServiceMsg::Heartbeat { input_ack: 3 })));
    }

    #[test]
    fn frames_carry_changed_rows_and_only_the_links_they_reference() {
        let mut sub = Sub::default();
        let first = frame_of(&mut sub, snapshot(&["a", "b", "c"], Some(1))).unwrap();
        assert!(first.full);
        assert_eq!(first.changed.len(), 3);
        assert_eq!(first.links, vec!["https://used".to_owned()], "unreferenced targets are left out");
        assert_eq!(first.changed[1].1[0].link, 1);
        assert!(frame_of(&mut sub, snapshot(&["a", "b", "c"], Some(1))).is_none(), "nothing changed");
        let second = frame_of(&mut sub, snapshot(&["a", "b", "X"], Some(1))).unwrap();
        assert!(!second.full);
        assert_eq!(second.changed.iter().map(|(y, _)| *y).collect::<Vec<_>>(), vec![2]);
        assert!(second.links.is_empty());
        let mut moved = snapshot(&["a", "b", "X"], Some(1));
        moved.cursor_shape = protocol::CURSOR_BAR;
        let third = frame_of(&mut sub, moved).unwrap();
        assert!(third.changed.is_empty() && third.cursor_shape == protocol::CURSOR_BAR, "cursor shape alone is a frame");
        let resized = frame_of(&mut sub, snapshot(&["a", "b"], None)).unwrap();
        assert!(resized.full);
    }

    #[test]
    fn input_is_applied_once_and_only_from_the_newest_connection() {
        let mut session = InputSession { applied: 0, owner: 1, seen: Instant::now() };
        assert!(session.admit(1, 1));
        assert!(session.admit(1, 2));
        assert!(!session.admit(1, 2), "a duplicate seq is dropped");
        assert!(!session.admit(1, 1));
        session.owner = 2;
        assert!(!session.admit(1, 3), "the replaced connection no longer applies input");
        assert!(session.admit(2, 5));
        assert_eq!(session.applied, 5);
    }
}
