//! The terminal host server (L1). Owns every hosted terminal so sessions
//! survive client restarts; streams only visible terminals to clients as row
//! diffs; keeps the session you type in responsive (priority manager).
//!
//! Clients connect over the named pipe (local) or, when remote listening is
//! on (`remote.listen`, or once a pairing code was opened), over TCP on
//! loopback (remote clients behind a Dev Tunnel). Both carry the same framed
//! protocol and handshake; TCP connections may also pair a new client.
//! Protocol-8 clients send and get
//! heartbeats, number their input (applied at most once per client session)
//! and, over TCP, get frames coalesced per tile and optionally compressed.
//!
//! A TCP connection that opens with an HTTP request (`GET `) is a WebSocket
//! (`canvas_common::ws`): the iPhone client reaches the port through Dev
//! Tunnels web forwarding. It carries the same byte stream; only paired
//! clients (or pairing) are accepted on it.
//!
//! Terminal sizes follow the client that typed in a terminal last, or claimed
//! it (tmux "latest", `canvas_common::sizing`).
//!
//! Unauthenticated TCP connections are limited: at most
//! [`MAX_UNAUTHENTICATED`] at once (more are closed at once), frames of at
//! most `PRE_AUTH_FRAME` bytes, and [`HANDSHAKE_TIMEOUT`] to finish (a read
//! deadline on the socket, no thread of its own). Over TCP the service token
//! only authenticates this user's own processes on this machine.
//!
//! Everything that is not about terminals (session history, the canvas
//! document, the event bus, …) is delegated to a [`HostApp`], the L3 backend
//! that shares this process.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};

use crate::emulator::{Engine, Snapshot, SnapshotImage};
use crate::outbox::{Next, Outbox, Push, Taken};
pub use crate::outbox::Outgoing;
use crate::paths::{Instance, build_id, now_ms};
use canvas_sys::ipc::{Pipe, PipeServer};
use crate::protocol::{self, CAP_ACK, CAP_COMPRESS, CAP_IMAGES, ClientMsg, Frame8, ImageRef, Input, LaunchKind, MAX_FRAME, PRE_AUTH_FRAME, Placement, Run8, SERVICE_ACKS, ServiceMsg, ServiceStats};
use crate::term::{EncodedImage, Hooks, ImagePng, Notice, Spawn, Terminal, lock};
use canvas_common::handshake::{Credential, Opening, Pairing};
use canvas_common::sizing::SizePolicy;
use canvas_common::wire::{Beat, Closer, FrameReader, FrameWriter, Liveness};
use canvas_common::{handshake, tcp, ws};

const FRAME_INTERVAL: Duration = Duration::from_millis(4);
/// Remote clients: the focused tile at most every 16 ms, others every 100 ms.
const FOCUSED_INTERVAL: Duration = Duration::from_millis(16);
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(100);
/// Remote clients without flow control that typed in the last 2 s: their
/// other tiles at most every 250 ms (the stopgap, `outbox::Pace`).
const TYPING_BACKGROUND_INTERVAL: Duration = Duration::from_millis(250);
/// Local clients that type in a tile: their other tiles at most every 33 ms
/// (the typed-in tile is not paced).
const LOCAL_BACKGROUND_INTERVAL: Duration = Duration::from_millis(33);
const PUMP_POLL: Duration = Duration::from_millis(50);
/// TCP clients that have not authenticated by then are dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// TCP connections in their handshake at once; more are closed at once.
pub const MAX_UNAUTHENTICATED: usize = 16;
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
/// Client sessions remembered for input deduplication.
const MAX_INPUT_SESSIONS: usize = 256;
/// Messages a client's worker may have waiting; a reader with more to hand
/// it waits (the client's own backpressure).
const WORK_QUEUE: usize = 4096;
/// A client worker busy with one message this long is a wedge sign.
pub const WORK_STUCK_MS: u64 = 30_000;
/// A remote client that takes none of the data written to it for this long
/// is dropped (its writer never waits on it longer).
pub const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// What the system buffers for sending to a remote client (`SO_SNDBUF`):
/// the rest waits in the client's outbox, ordered by priority.
const CLIENT_SEND_BUFFER: usize = 64 << 10;
/// Bytes of image messages queued for one client at most; more wait.
const IMAGE_BUDGET: usize = 32 << 20;
/// When images left out for the budget are tried again.
const IMAGE_RETRY: Duration = Duration::from_millis(50);

/// A launch the backend resolved into a concrete program.
pub struct Resolved {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// An agent CLI: graceful stop (Ctrl+D), tracked history, idle auto-stop.
    pub agent: bool,
    /// A VM session's terminal: its program runs in the guest behind this
    /// link (`program` and `args` are then not run here).
    pub remote: Option<Arc<dyn crate::term::RemotePty>>,
}

/// The L3 backend running in the same process (the "agent supervisor").
pub trait HostApp: Send + Sync {
    /// Turns an agent launch (`LaunchKind::Copilot…`) into a program to run
    /// in `cwd` (the folder the terminal starts in).
    fn resolve_launch(&self, id: &str, kind: LaunchKind, cwd: &str) -> Result<Resolved>;
    /// Environment a plain command's terminal gets (`LaunchKind::Command`,
    /// e.g. the demo's shells): the canvas integration.
    fn command_env(&self, _id: &str) -> Vec<(String, String)> {
        Vec::new()
    }
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
    /// What the service knows about a paired remote client id
    /// (`ClientInfo::remote_id`, already checked to be a valid name).
    fn remote_credential(&self, id: &str) -> Credential;
    /// A paired remote client authenticated (notes when it was last seen).
    fn remote_seen(&self, _id: &str) {}
    /// Whether a paired remote client may still be connected (checked again
    /// once it was admitted, against a revocation meanwhile).
    fn remote_active(&self, id: &str) -> bool {
        matches!(self.remote_credential(id), Credential::Secret(_))
    }
    /// Pairs new remote clients (`None`: this service does not).
    fn pairing(&self) -> Option<&dyn Pairing> {
        None
    }
    /// The service is about to exit: persist what must not be lost.
    fn shutdown(&self) {}
    /// A VM session's satellite authenticated (its credential said so in the
    /// handshake): serve its link until it ends. It never becomes a client:
    /// no `Ready8`, terminal list, snapshot or broadcast reaches it. The
    /// default refuses it (a service without VM sessions).
    fn serve_satellite(&self, link: SatelliteLink) -> Result<()> {
        link.closer.close();
        bail!("this service does not serve satellites")
    }
}

/// A satellite's connection, handed to [`HostApp::serve_satellite`] right
/// after the handshake (compression already negotiated).
pub struct SatelliteLink {
    pub reader: FrameReader,
    pub writer: FrameWriter,
    pub closer: Closer,
    /// What it said in `Hello8` (`remote_id` is its `sat-…` credential id).
    pub info: canvas_common::protocol::ClientInfo,
    /// It came through a WebSocket (a tunnel's web forwarding).
    pub websocket: bool,
}

/// What a client last received of one subscribed terminal.
#[derive(Default)]
struct Sub {
    /// The terminal (one run of it) the frames came from: a relaunch under
    /// the same id starts over with a full frame (its generations restart).
    terminal: Weak<Terminal>,
    generation: u64,
    hashes: Vec<u64>,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16)>,
    offset: u32,
    shape: u8,
    blink: bool,
    modes: u32,
    /// Image placements last sent (CAP_IMAGES).
    placements: Vec<Placement>,
    /// The placements the client was last sent (those whose image it has).
    visible: Vec<Placement>,
    /// Images of the current placements: id -> (generation, delivered).
    images: HashMap<u32, (u64, bool)>,
    /// An image waits (being encoded, or over the client's image budget):
    /// the terminal is looked at again then even when it did not change.
    images_due: Option<Instant>,
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
    /// The paired remote client this connection authenticated as.
    pub remote_id: Option<String>,
    /// The process at the other end of a pipe connection.
    pub peer_pid: Option<u32>,
    /// Connected through a WebSocket (Dev Tunnels web forwarding: the iPhone).
    pub websocket: bool,
    /// `CAP_*` bits from the handshake.
    caps: u32,
    /// What waits for the writer ([`crate::outbox`]).
    outbox: Arc<Outbox>,
    subs: Mutex<HashMap<String, Sub>>,
    /// The focused tile (`Subscribe8`).
    focused: Mutex<Option<String>>,
    session: Option<Arc<Mutex<InputSession>>>,
    liveness: Option<Arc<Liveness>>,
    closer: Closer,
    /// Set once the service dropped this client ([`Host::drop_client`]).
    dropped: AtomicBool,
    /// When its worker began the message it handles (ms; 0: idle).
    work_since: AtomicU64,
    /// Everything it read was handled and its connection thread is done
    /// (a reconnection waits for it: [`Client::wait_drained`]).
    drained: (Mutex<bool>, std::sync::Condvar),
}

impl Drop for Client {
    fn drop(&mut self) {
        // Its writer ends with it.
        self.outbox.close();
    }
}

impl Client {
    fn new(id: u64, protocol: u32, remote: bool, caps: u32, outbox: Arc<Outbox>, closer: Closer) -> Self {
        Self {
            id,
            protocol,
            remote,
            remote_id: None,
            peer_pid: None,
            websocket: false,
            caps,
            outbox,
            subs: Mutex::new(HashMap::new()),
            focused: Mutex::new(None),
            session: None,
            liveness: (protocol >= 8).then(|| Arc::new(Liveness::standard())),
            closer,
            dropped: AtomicBool::new(false),
            work_since: AtomicU64::new(0),
            drained: (Mutex::new(false), std::sync::Condvar::new()),
        }
    }

    /// Its connection thread is done: everything it read was handled.
    fn drained(&self) {
        *lock(&self.drained.0) = true;
        self.drained.1.notify_all();
    }

    /// Waits (at most `limit`) until what this connection read was handled.
    fn wait_drained(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut drained = lock(&self.drained.0);
        while !*drained {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return };
            drained = self.drained.1.wait_timeout(drained, left).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// Queues a message in the shape this client reads; false when the client
    /// is gone. Nothing is dropped silently: state is coalesced, and a client
    /// whose lanes overflow is dropped (it reconnects and gets everything again).
    pub fn send(&self, message: ServiceMsg) -> bool {
        let Some(message) = message.shaped_for(self.protocol, self.caps) else { return !self.outbox.is_closed() };
        match self.outbox.push(message) {
            Push::Queued => true,
            Push::Closed => false,
            Push::Full => {
                self.fell_behind();
                false
            }
        }
    }

    /// A lane overflowed: the client is closed (its writer drops it and logs why, once).
    fn fell_behind(&self) {
        let id = self.id;
        self.outbox.overflowed(|w| format!("client {id} fell behind ({} control, {} bulk message(s), about {} KB waiting); closing", w.control, w.bulk, w.bytes >> 10));
        self.closer.close();
    }

    /// Queues the frame of a tile (one per tile waits at most).
    fn send_frame(&self, tile: &str, frame: ServiceMsg) -> Push {
        match frame.shaped_for(self.protocol, self.caps) {
            Some(frame) => self.outbox.push_frame(tile, frame),
            None => Push::Queued,
        }
    }

    /// Queues an encoded image (`CAP_IMAGES` clients) and counts it against the image budget.
    fn send_image(&self, term: &str, image_id: u32, image: &EncodedImage) -> Push {
        if self.protocol < 8 || self.caps & CAP_IMAGES == 0 {
            return Push::Queued;
        }
        let message = Outgoing::Image { term: term.to_owned(), image_id, width: image.width, height: image.height, png: image.png.clone() };
        self.outbox.push_image(message, image.png.len())
    }

    /// Whether an image of `len` bytes fits the image budget now (one image always fits an empty queue).
    fn image_fits(&self, len: usize) -> bool {
        let queued = self.outbox.image_bytes();
        queued == 0 || queued + len <= IMAGE_BUDGET
    }

    /// Ends the connection.
    pub fn close(&self) {
        self.closer.close();
    }

    /// Whether the service dropped this client (its replies go nowhere).
    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }

    /// `CAP_*` bits from the handshake.
    pub fn caps(&self) -> u32 {
        self.caps
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
    clipboard: Sender<(String, String)>,
    /// Terminal ids whose title, progress or bell count changed.
    changed: Sender<String>,
    /// Desktop notifications with their text.
    notify: Sender<Notice>,
    last_input_client: AtomicU64,
    /// The client that last sent input to each terminal (gets its OSC 52 copies).
    input_clients: Mutex<HashMap<String, u64>>,
    /// The terminal that most recently received keyboard input (its agent
    /// gets CPU priority for a while: `priority::FOCUS_LAPSE`).
    active_input: Mutex<crate::priority::Focus>,
    priority_wake: Sender<()>,
    bytes: Arc<AtomicU64>,
    frames: AtomicU64,
    last_activity: Mutex<Instant>,
    started: Instant,
    pub shutting_down: AtomicBool,
    /// Terminal engine for new terminals (`ATELIER_ENGINE`).
    pub engine: Engine,
    fallback_logged: AtomicBool,
    /// Compress frames for remote clients that accept it (`remote.compress`).
    compress: bool,
    /// Input deduplication per client session id.
    input_sessions: Mutex<HashMap<Vec<u8>, Arc<Mutex<InputSession>>>>,
    /// The loopback listener for remote clients, while listening.
    remote: Mutex<Option<RemoteListener>>,
    /// TCP connections in their handshake.
    unauthenticated: Arc<AtomicUsize>,
    /// When a refused surplus connection was last logged.
    surplus_logged: Mutex<Option<Instant>>,
    /// This service's user (SID): over TCP the token only authenticates it.
    user_sid: String,
    /// Each terminal's size wishes and owner (tmux "latest").
    sizes: Mutex<HashMap<String, SizePolicy>>,
    /// Terminal ids whose launch is under way ([`Host::launch`]), with when it began.
    launching: Mutex<HashMap<String, u64>>,
    /// Clients dropped whose reader has not ended yet, with when they were
    /// dropped (a wedge sign when it lasts: [`Health`]).
    closing: Mutex<HashMap<u64, u64>>,
    /// Connections whose messages are being served (listed or not: a dropped
    /// one's worker may still handle what it read).
    serving: Mutex<HashMap<u64, Arc<Client>>>,
    /// Keys a launch claimed (an agent session id) and the terminal that has
    /// each: never two live terminals with one key ([`Host::claim_launch_key`]).
    launch_keys: Mutex<HashMap<String, String>>,
    app: OnceLock<Arc<dyn HostApp>>,
}

/// What a launch did ([`Host::start`]).
enum Started {
    New,
    /// It was running already.
    Running,
    /// Another launch of it is under way (it tells every client when it is done).
    Launching,
}

/// A terminal id reserved by its launch until the launch is over.
struct LaunchReservation<'a> {
    host: &'a Host,
    id: String,
}

impl Drop for LaunchReservation<'_> {
    fn drop(&mut self) {
        lock(&self.host.launching).remove(&self.id);
    }
}

/// The loopback listener's address and the flag that stops its accept loop.
struct RemoteListener {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

/// A place among the [`MAX_UNAUTHENTICATED`] handshakes; freed when dropped.
struct HandshakeSlot(Arc<AtomicUsize>);

impl HandshakeSlot {
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        count.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_UNAUTHENTICATED).then_some(n + 1)).ok()?;
        Some(Self(count.clone()))
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Appends a line to the instance's service log. The line is timestamped now
/// and written by a logger thread: a caller waits at most [`LOG_WAIT`] for it
/// (so it is on disk when this returns, as before) and not at all while the
/// file is stalled (a scan or a busy disk once froze every thread that
/// logged, the frame pump and the heartbeat watchdog included). Lines beyond
/// [`LOG_QUEUE`] waiting are dropped and counted in the next line written.
pub fn log(instance: &Instance, message: &str) {
    let line = format!("{} [{}] {message}\n", now_ms(), std::process::id());
    let path = instance.log_path();
    match logger() {
        Some(logger) => logger.append(path, line),
        None => append_log(&path, line.as_bytes()),
    }
}

/// How long [`log`] waits for its line to be written.
const LOG_WAIT: Duration = Duration::from_millis(250);
/// Lines waiting for the logger thread at most.
const LOG_QUEUE: usize = 4096;
/// The log is rotated (to `.log.old`) beyond this size.
const LOG_LIMIT: u64 = 4 * 1024 * 1024;

/// Appends `bytes` to a log file, rotating it first when it is too large.
/// When another program holds the log so it cannot be renamed (Windows),
/// it is copied to `.log.old` and emptied instead.
fn append_log(path: &std::path::Path, bytes: &[u8]) {
    // After a rotation that failed both ways, the next try waits a minute (not every line).
    static FAILED_AT: AtomicU64 = AtomicU64::new(0);
    let failed_at = FAILED_AT.load(Ordering::Relaxed);
    if (failed_at == 0 || now_ms().saturating_sub(failed_at) > 60_000) && std::fs::metadata(path).map(|m| m.len() > LOG_LIMIT).unwrap_or(false) {
        let old = path.with_extension("log.old");
        let rotated = std::fs::rename(path, &old).is_ok()
            || (std::fs::copy(path, &old).is_ok() && std::fs::OpenOptions::new().write(true).open(path).and_then(|file| file.set_len(0)).is_ok());
        FAILED_AT.store(if rotated { 0 } else { now_ms() }, Ordering::Relaxed);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(bytes);
    }
}

/// The logger thread's queue and how far it got.
struct Logger {
    lines: Sender<(u64, std::path::PathBuf, String)>,
    next: AtomicU64,
    /// The last line written (its number) and the wake-up for callers waiting on it.
    written: Mutex<u64>,
    wake: std::sync::Condvar,
    /// When the write under way started (ms since `epoch`; 0: idle).
    busy_since: AtomicU64,
    epoch: Instant,
    dropped: AtomicU64,
}

impl Logger {
    fn append(&self, path: std::path::PathBuf, line: String) {
        let seq = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        if self.lines.try_send((seq, path, line)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // A stalled file (one write under way for a second) is not waited for.
        let busy = self.busy_since.load(Ordering::Acquire);
        if busy != 0 && (self.epoch.elapsed().as_millis() as u64).saturating_sub(busy) > 1_000 {
            return;
        }
        let deadline = Instant::now() + LOG_WAIT;
        let mut written = lock(&self.written);
        while *written < seq {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            written = self.wake.wait_timeout(written, left).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// Writes queued lines in batches (one open per file and batch).
    fn run(&self, lines: Receiver<(u64, std::path::PathBuf, String)>) {
        while let Ok(first) = lines.recv() {
            let mut batch = vec![first];
            batch.extend(lines.try_iter().take(LOG_QUEUE));
            let last = batch.iter().map(|(seq, _, _)| *seq).max().unwrap_or(0);
            self.busy_since.store((self.epoch.elapsed().as_millis() as u64).max(1), Ordering::Release);
            let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
            for (_, path, line) in batch {
                match files.iter_mut().find(|(p, _)| *p == path) {
                    Some((_, text)) => text.push_str(&line),
                    None => files.push((path, line)),
                }
            }
            let dropped = self.dropped.swap(0, Ordering::Relaxed);
            for (index, (path, mut text)) in files.into_iter().enumerate() {
                if index == 0 && dropped > 0 {
                    text.push_str(&format!("{} [{}] {dropped} log line(s) dropped: the log file did not keep up\n", now_ms(), std::process::id()));
                }
                append_log(&path, text.as_bytes());
            }
            self.busy_since.store(0, Ordering::Release);
            let mut written = lock(&self.written);
            *written = (*written).max(last);
            drop(written);
            self.wake.notify_all();
        }
    }
}

/// The process's logger (`None` when its thread could not start: lines are then written by the caller).
fn logger() -> Option<&'static Logger> {
    static LOGGER: OnceLock<Option<&'static Logger>> = OnceLock::new();
    *LOGGER.get_or_init(|| {
        let (tx, rx) = bounded(LOG_QUEUE);
        let logger: &'static Logger = Box::leak(Box::new(Logger {
            lines: tx,
            next: AtomicU64::new(0),
            written: Mutex::new(0),
            wake: std::sync::Condvar::new(),
            busy_since: AtomicU64::new(0),
            epoch: Instant::now(),
            dropped: AtomicU64::new(0),
        }));
        try_spawn("log", move || logger.run(rx)).ok().map(|()| logger)
    })
}

/// The engine new terminals use: `ATELIER_ENGINE`, else
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

/// Starts a thread; an error (no resources) is returned instead of a panic.
pub fn try_spawn<F: FnOnce() + Send + 'static>(name: &str, body: F) -> std::io::Result<()> {
    std::thread::Builder::new().name(name.into()).spawn(body).map(|_| ())
}

/// Runs the service: binds the pipe (and the TCP port when remote listening
/// is configured), starts the host threads, lets `make_app` build the
/// backend, then serves clients until shutdown.
pub fn run(instance: Instance, make_app: impl FnOnce(&Arc<Host>) -> Arc<dyn HostApp>) -> Result<()> {
    canvas_sys::process::raise_priority();
    // Self-tests stand in for a service of the build before the rename by
    // serving its endpoint (`ATELIER_SELFTEST=1` and `ATELIER_TEST_LEGACY_ENDPOINT=1`).
    let simulated_legacy = canvas_common::env::var("ATELIER_SELFTEST").is_ok_and(|v| v.trim() == "1")
        && canvas_common::env::var("ATELIER_TEST_LEGACY_ENDPOINT").is_ok_and(|v| v.trim() == "1")
        && instance.legacy_pipe_name().is_some();
    let endpoint = if simulated_legacy { instance.legacy_pipe_name().unwrap_or_default() } else { instance.pipe_name() };
    let mut server = match PipeServer::bind(&endpoint) {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("a service for this canvas is already running")
        }
        Err(error) => return Err(error).context("cannot create the service pipe"),
    };
    // A service of the build before the rename serves another endpoint for
    // the same home and instance: never a second service beside it.
    if !simulated_legacy
        && let Some(old) = instance.legacy_pipe_name()
        && canvas_sys::ipc::is_served(&old)
    {
        drop(server);
        bail!(
            "a service of {} (the build before the rename) is running for this canvas; stop it first (`{} service stop`, or update it from a client)",
            canvas_common::legacy::PRODUCT,
            canvas_common::legacy::CLI
        )
    }
    if let Err(error) = canvas_sys::process::contain_descendants() {
        log(&instance, &format!("job containment unavailable: {error}"));
    }
    #[cfg(unix)]
    {
        let ended = sweep_orphans(&instance);
        if ended > 0 {
            log(&instance, &format!("ended {ended} process(es) a previous service left running"));
        }
    }
    let token = protocol::nonce();
    canvas_sys::token::write_token(&instance.token_path(), &token)?;
    // Files left by a service that did not exit cleanly.
    let _ = std::fs::remove_file(instance.port_path());
    let _ = std::fs::remove_file(instance.listen_error_path());
    let settings = canvas_common::settings::Settings::load(&instance.home);
    log(&instance, &format!("service started, build {}", build_id()));

    let (pump_tx, pump_rx) = bounded::<()>(1);
    let (exited_tx, exited_rx) = unbounded::<String>();
    let (clip_tx, clip_rx) = bounded::<(String, String)>(16);
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
        input_clients: Mutex::new(HashMap::new()),
        active_input: Mutex::new(crate::priority::Focus::default()),
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
        remote: Mutex::new(None),
        unauthenticated: Arc::default(),
        surplus_logged: Mutex::new(None),
        user_sid: canvas_sys::process::current_user_id().unwrap_or_default(),
        sizes: Mutex::new(HashMap::new()),
        launching: Mutex::new(HashMap::new()),
        closing: Mutex::new(HashMap::new()),
        serving: Mutex::new(HashMap::new()),
        launch_keys: Mutex::new(HashMap::new()),
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
    #[cfg(unix)]
    spawn_named("remember", { let h = host.clone(); move || remember_loop(h) });
    spawn_named("liveness", { let h = host.clone(); move || liveness_loop(h) });
    spawn_named("health", { let h = host.clone(); move || health_loop(h) });
    if let Some(addr) = settings.listen() {
        // Failures are logged and recorded for `doctor` by `listen_remote`. A
        // port still held by a service that is exiting is tried again for a while.
        if host.listen_remote(&addr).is_err() {
            let h = host.clone();
            spawn_named("listen-retry", move || {
                for _ in 0..30 {
                    std::thread::sleep(Duration::from_millis(500));
                    if h.listen_remote(&addr).is_ok() {
                        return;
                    }
                }
            });
        }
    }

    loop {
        match server.accept() {
            Ok(pipe) => {
                let host = host.clone();
                let spawned = try_spawn("client", move || {
                    if let Err(error) = serve_client(&host, Link::Pipe(pipe), None) {
                        log(&host.instance, &format!("client ended: {error:#}"));
                    }
                });
                // The connection (moved into the closure) is dropped and closes.
                if let Err(error) = spawned {
                    log(&instance, &format!("cannot start a client thread: {error}; connection dropped"));
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(error) => {
                log(&instance, &format!("accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Serves clients on a loopback TCP listener (remote clients behind a tunnel)
/// until `stop` is set. Connections beyond [`MAX_UNAUTHENTICATED`] handshakes
/// are closed at once.
fn accept_tcp(host: Arc<Host>, listener: std::net::TcpListener, stop: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match stream {
            Ok(stream) => {
                let Some(slot) = HandshakeSlot::take(&host.unauthenticated) else {
                    drop(stream);
                    host.log_surplus();
                    continue;
                };
                let client_host = host.clone();
                let spawned = try_spawn("remote-client", move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    if let Err(error) = serve_client(&client_host, Link::Tcp(stream), Some(slot)) {
                        log(&client_host.instance, &format!("remote client {peer} ended: {error:#}"));
                    }
                });
                if let Err(error) = spawned {
                    log(&host.instance, &format!("cannot start a remote client thread: {error}; connection dropped"));
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(error) => {
                log(&host.instance, &format!("remote accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    log(&host.instance, "stopped listening for remote clients");
}

impl Host {
    /// Also serves clients on a loopback TCP port (an IP literal; other
    /// addresses are refused; bound exclusively) and publishes it in
    /// `<home>/service-<instance>.port` (owner-only). Returns the port;
    /// listening twice keeps the first listener. A failure is logged and
    /// kept in `service-<instance>.listen-error` (for `doctor`) until a
    /// later attempt succeeds.
    pub fn listen_remote(self: &Arc<Self>, addr: &str) -> Result<u16> {
        let mut current = lock(&self.remote);
        if let Some(listener) = &*current {
            return Ok(listener.addr.port());
        }
        let started = (|| -> Result<(SocketAddr, Arc<AtomicBool>)> {
            let listener = tcp::listen_loopback(addr).with_context(|| {
                format!("cannot listen on {addr} (another program, or another user's service, may hold the port; pick another with `atelier remote tunnel-setup --port PORT`)")
            })?;
            let bound = listener.local_addr()?;
            let stop = Arc::new(AtomicBool::new(false));
            let (host, flag) = (self.clone(), stop.clone());
            try_spawn("tcp-accept", move || accept_tcp(host, listener, flag)).context("cannot start the remote accept thread")?;
            Ok((bound, stop))
        })();
        let (bound, stop) = match started {
            Ok(started) => started,
            Err(error) => {
                log(&self.instance, &format!("NOT LISTENING for remote clients: {error:#}"));
                let _ = canvas_sys::fs::write_private(&self.instance.listen_error_path(), format!("{} {error:#}", now_ms()).as_bytes());
                return Err(error);
            }
        };
        let _ = std::fs::remove_file(self.instance.listen_error_path());
        if let Err(error) = canvas_sys::fs::write_private(&self.instance.port_path(), bound.port().to_string().as_bytes()) {
            log(&self.instance, &format!("cannot publish the remote port: {error}"));
        }
        log(&self.instance, &format!("listening for remote clients on {bound}"));
        *current = Some(RemoteListener { addr: bound, stop });
        Ok(bound.port())
    }

    /// Stops listening for remote clients (connections stay); false when it was not listening.
    pub fn stop_remote(&self) -> bool {
        let Some(listener) = lock(&self.remote).take() else { return false };
        listener.stop.store(true, Ordering::Release);
        // Wakes the accept loop, which sees the flag and closes the listener.
        let _ = TcpStream::connect_timeout(&listener.addr, Duration::from_millis(500));
        let _ = std::fs::remove_file(self.instance.port_path());
        true
    }

    /// The loopback port remote clients connect to, while listening.
    pub fn remote_port(&self) -> Option<u16> {
        lock(&self.remote).as_ref().map(|l| l.addr.port())
    }

    /// Logs refused surplus connections at most every 10 s.
    fn log_surplus(&self) {
        let mut last = lock(&self.surplus_logged);
        if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
            *last = Some(Instant::now());
            log(&self.instance, &format!("more than {MAX_UNAUTHENTICATED} TCP connections are authenticating; closing new ones"));
        }
    }

    /// Whether the TCP peer at `tcp` may authenticate with the token: a
    /// process of this user on this machine.
    fn token_allowed(&self, tcp: &TcpEnds) -> bool {
        let owner = canvas_sys::net::peer_process(tcp.local, tcp.peer).ok();
        let user = owner.and_then(|pid| canvas_sys::process::process_user_id(pid).ok());
        let allowed = tcp::token_peer_allowed(tcp.peer, user.as_deref(), &self.user_sid);
        if !allowed {
            log(&self.instance, &format!("refused token authentication over TCP from {} (process {owner:?}, user {user:?})", tcp.peer));
        }
        allowed
    }

    /// The backend; its threads may call in while `run` is still installing it.
    fn app(&self) -> &Arc<dyn HostApp> {
        loop {
            if let Some(app) = self.app.get() {
                return app;
            }
            std::thread::yield_now();
        }
    }

    /// Drops a client at once, without waiting on it: its connection is shut
    /// (its reader and writer end, or fail at the write deadline), it leaves
    /// the client list (no frame, broadcast or watchdog sees it again) and the
    /// rest of the cleanup runs on a helper thread. Idempotent: only the first
    /// call logs `why`. The client's reader thread, whenever it returns, finds
    /// it gone and only repeats the (idempotent) cleanup.
    pub fn drop_client(self: &Arc<Self>, client: &Arc<Client>, why: Option<&str>) {
        if client.dropped.swap(true, Ordering::AcqRel) {
            return;
        }
        lock(&self.closing).insert(client.id, now_ms());
        client.closer.close();
        client.outbox.close();
        // Logged once closed: a slow log never delays the close.
        if let Some(why) = why {
            log(&self.instance, why);
        }
        let removed = {
            let mut clients = lock(&self.clients);
            match clients.get(&client.id) {
                Some(listed) if Arc::ptr_eq(listed, client) => clients.remove(&client.id),
                _ => None,
            }
        };
        if removed.is_some() {
            let (host, id) = (self.clone(), client.id);
            let cleanup = move || {
                host.size_leave(id, None);
                host.app().client_disconnected(id);
            };
            if try_spawn("client-drop", cleanup.clone()).is_err() {
                cleanup();
            }
        }
    }

    /// Clients the service listed (connected and not dropped).
    pub fn client_count(&self) -> usize {
        lock(&self.clients).len()
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

    /// Starts terminal `id`, at most once: while it runs, or while another
    /// launch of it is under way (two clients resuming the same session at
    /// once, a reconnection replaying a launch), this does nothing and the
    /// caller shows the one terminal there is. The id is reserved under one
    /// lock before anything is resolved or spawned.
    pub fn launch(&self, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16) -> Result<()> {
        self.start(id, kind, cwd, cols, rows).map(|_| ())
    }

    /// [`Host::launch`], saying whether it started the terminal.
    fn start(&self, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16) -> Result<Started> {
        anyhow::ensure!(!id.is_empty() && id.len() <= 100, "invalid terminal id");
        let _reservation = match self.reserve_launch(id) {
            Ok(reservation) => reservation,
            Err(started) => return Ok(started),
        };
        let resolved = match kind {
            LaunchKind::Command { program, args } => Resolved { program, args, env: self.app().command_env(id), agent: false, remote: None },
            other => self.app().resolve_launch(id, other, cwd)?,
        };
        if let Some(pty) = resolved.remote {
            self.adopt_remote(id, cols, rows, resolved.agent, pty);
            return Ok(Started::New);
        }
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
                raw: None,
            },
        )?;
        // The pid stays last: `selftest copilot` parses the number after " pid ".
        log(&self.instance, &format!("launched {id} engine {} pid {}", terminal.engine.name(), terminal.pid));
        self.tell_terms(|terms| {
            let info = terminal.info();
            terms.insert(id.to_owned(), terminal);
            ((), vec![ServiceMsg::Terminals(vec![info])])
        });
        self.app().terminals_changed();
        Ok(Started::New)
    }

    /// Changes (or reads) the terminal list and queues what every client
    /// must learn of it under the client list's lock, as [`Host::register`]
    /// does (clients before terms): clients get terminal states in the order
    /// they happened, never an older one after a newer (R2-A-08).
    fn tell_terms<R>(&self, change: impl FnOnce(&mut HashMap<String, Arc<Terminal>>) -> (R, Vec<ServiceMsg>)) -> R {
        let clients = lock(&self.clients);
        let (result, messages) = change(&mut lock(&self.terms));
        for client in clients.values() {
            for message in &messages {
                client.send(message.clone());
            }
        }
        result
    }

    /// Sends one client the state of terminals (`None`: all) under the
    /// client list's lock, so a change broadcast meanwhile is never overtaken.
    fn send_terms(&self, client: &Client, id: Option<&str>) {
        let _clients = lock(&self.clients);
        let infos: Vec<_> = lock(&self.terms).iter().filter(|(term, _)| id.is_none_or(|id| id == term.as_str())).map(|(_, t)| t.info()).collect();
        client.send(ServiceMsg::Terminals(infos));
    }

    /// Reserves terminal `id` for a launch; otherwise it runs (its info) or
    /// another launch of it is under way (logged).
    fn reserve_launch(&self, id: &str) -> std::result::Result<LaunchReservation<'_>, Started> {
        let mut launching = lock(&self.launching);
        if lock(&self.terms).get(id).is_some_and(|t| t.alive.load(Ordering::Acquire)) {
            return Err(Started::Running);
        }
        if launching.insert(id.to_owned(), now_ms()).is_some() {
            drop(launching);
            log(&self.instance, &format!("terminal {id} is already being launched; not started twice"));
            return Err(Started::Launching);
        }
        Ok(LaunchReservation { host: self, id: id.to_owned() })
    }

    /// Whether a launch of terminal `id` is under way.
    pub fn is_launching(&self, id: &str) -> bool {
        lock(&self.launching).contains_key(id)
    }

    /// Whether terminal `id` runs or is being launched.
    pub fn running_or_launching(&self, id: &str) -> bool {
        lock(&self.launching).contains_key(id) || lock(&self.terms).get(id).is_some_and(|t| t.alive.load(Ordering::Acquire))
    }

    /// Claims `key` (an agent session id) for terminal `id` during its
    /// launch. Fails with the other terminal's id while another terminal that
    /// claimed it runs or is being launched: one session never gets two
    /// processes, whichever boxes ask for it.
    pub fn claim_launch_key(&self, id: &str, key: &str) -> std::result::Result<(), String> {
        let mut keys = lock(&self.launch_keys);
        if let Some(other) = keys.get(key).filter(|other| other.as_str() != id)
            && self.running_or_launching(other)
        {
            return Err(other.clone());
        }
        // Keys of terminals that ended hold nothing: forgotten, so the map
        // does not grow with every session ever launched.
        keys.retain(|_, owner| owner.as_str() == id || self.running_or_launching(owner));
        keys.insert(key.to_owned(), id.to_owned());
        Ok(())
    }

    /// A remote terminal (a VM session's: its program in the guest behind
    /// `pty`), at a launch or again after a restart of this service (its
    /// program kept running there). `pty` gets the terminal's feed.
    pub fn adopt_remote(&self, id: &str, cols: u16, rows: u16, agent: bool, pty: Arc<dyn crate::term::RemotePty>) {
        let hooks = Hooks {
            pump: self.pump.clone(),
            exited: self.exited.clone(),
            clipboard: self.clipboard.clone(),
            bytes: self.bytes.clone(),
            changed: self.changed.clone(),
            notify: Some(self.notify.clone()),
            raw: None,
        };
        let (terminal, feed) = Terminal::spawn_remote(crate::term::RemoteSpawn { id, cols, rows, copilot: agent, engine: self.effective_engine() }, hooks, pty.clone());
        let (cols, rows) = terminal.size();
        pty.started(feed, cols, rows);
        log(&self.instance, &format!("launched {id} on a VM (its program runs in the guest)"));
        self.tell_terms(|terms| {
            let info = terminal.info();
            terms.insert(id.to_owned(), terminal);
            ((), vec![ServiceMsg::Terminals(vec![info])])
        });
        self.app().terminals_changed();
    }

    pub fn stats(&self) -> ServiceStats {
        let terms = lock(&self.terms);
        let usage = canvas_sys::process::process_usage(None);
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
        // A VM session's program keeps running in its guest: its terminal is only let go (adopted again at the next start).
        let terms: Vec<Arc<Terminal>> = lock(&self.terms).values().filter(|t| !t.is_remote()).cloned().collect();
        for terminal in &terms {
            self.app().terminal_stopping(&terminal.id);
            terminal.stop(false);
        }
        // Each agent gets its whole grace period (its own stop kills it then).
        let deadline = Instant::now() + crate::term::STOP_GRACE + Duration::from_secs(1);
        while Instant::now() < deadline && terms.iter().any(|t| t.alive.load(Ordering::Acquire)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        for terminal in &terms {
            terminal.force_kill();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(remembered_path(&self.instance));
        self.app().shutdown();
        self.remove_published_files();
        log(&self.instance, "service exiting");
        std::process::exit(0);
    }

    /// The token, port and listen-error files are only valid while this process runs.
    fn remove_published_files(&self) {
        let _ = std::fs::remove_file(health_path(&self.instance));
        let _ = std::fs::remove_file(self.instance.token_path());
        let _ = std::fs::remove_file(self.instance.port_path());
        let _ = std::fs::remove_file(self.instance.listen_error_path());
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

    /// `client` sent input to terminal `id` (its OSC 52 copies go there).
    fn input_from(&self, client: u64, id: &str) {
        self.last_input_client.store(client, Ordering::Relaxed);
        let mut clients = lock(&self.input_clients);
        if clients.get(id) != Some(&client) {
            clients.insert(id.to_owned(), client);
        }
    }

    fn note_active(&self, id: &str) {
        let changed = lock(&self.active_input).note(id, Instant::now());
        if changed {
            let _ = self.priority_wake.try_send(());
        }
    }

    /// Resizes terminal `id` to `size` (when the size policy says so).
    fn apply_size(&self, id: &str, size: Option<(u16, u16)>) -> Result<()> {
        if let Some((cols, rows)) = size {
            self.terminal(id)?.resize(cols, rows)?;
            let _ = self.pump.try_send(());
        }
        Ok(())
    }

    /// `client` wants terminal `id` at `cols`×`rows` (`Resize`).
    fn size_wish(&self, client: u64, id: &str, cols: u16, rows: u16) -> Result<()> {
        let target = lock(&self.sizes).entry(id.to_owned()).or_default().wish(client, cols, rows);
        self.apply_size(id, target)
    }

    /// `client` typed in terminal `id` or claimed it: the terminal follows its size.
    fn size_claim(&self, client: u64, id: &str) -> Result<()> {
        let target = {
            let mut sizes = lock(&self.sizes);
            let policy = sizes.entry(id.to_owned()).or_default();
            if policy.owner() == Some(client) {
                // Every keystroke claims: nothing to do when it already owns it.
                return Ok(());
            }
            policy.claim(client)
        };
        self.apply_size(id, target)
    }

    /// `client` stopped showing terminal `id` (or disconnected): the newest
    /// remaining wish applies, after a moment when `client` owned the size
    /// (`SizePolicy::leave_at`; [`Host::size_expire`] ends the wait).
    fn size_leave(&self, client: u64, id: Option<&str>) {
        let now = Instant::now();
        let targets: Vec<(String, Option<(u16, u16)>)> = {
            let mut sizes = lock(&self.sizes);
            let mut targets = Vec::new();
            for (term, policy) in sizes.iter_mut().filter(|(term, _)| id.is_none_or(|i| i == term.as_str())) {
                let owned = policy.owner() == Some(client);
                let before = policy.target();
                let after = policy.leave_at(client, now);
                if (owned && policy.leaving_until().is_none()) || after != before {
                    targets.push((term.clone(), after));
                }
            }
            sizes.retain(|_, policy| !policy.is_empty());
            targets
        };
        for (term, target) in targets {
            let _ = self.apply_size(&term, target);
        }
    }

    /// Ends the waits of owners that left (`size_leave`) once they are over:
    /// their terminals take the newest remaining wish.
    fn size_expire(&self) {
        let now = Instant::now();
        let targets: Vec<(String, Option<(u16, u16)>)> = {
            let mut sizes = lock(&self.sizes);
            let targets = sizes.iter_mut().filter_map(|(term, policy)| policy.expire(now).map(|target| (term.clone(), target))).collect();
            sizes.retain(|_, policy| !policy.is_empty());
            targets
        };
        for (term, target) in targets {
            let _ = self.apply_size(&term, target);
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
/// A reader and a writer, and whether they carry a WebSocket.
type Sniffed = (Box<dyn Read + Send>, Box<dyn Write + Send>, bool);

/// The ends of a TCP connection and its closer (read deadlines, the token check).
struct TcpEnds {
    closer: tcp::TcpCloser,
    local: SocketAddr,
    peer: SocketAddr,
}

impl Link {
    fn split(self) -> std::io::Result<(Halves, Option<TcpEnds>)> {
        Ok(match self {
            Link::Pipe(pipe) => {
                let (reader, writer, closer) = pipe.split()?;
                ((Box::new(reader), Box::new(writer), Closer::new(move || closer.close())), None)
            }
            Link::Tcp(stream) => {
                let (local, peer) = (stream.local_addr()?, stream.peer_addr()?);
                let (reader, writer, closer) = tcp::split(stream)?;
                let ends = TcpEnds { closer: closer.clone(), local, peer };
                ((Box::new(reader), Box::new(writer), Closer::new(move || closer.close())), Some(ends))
            }
        })
    }
}

/// A client's name for the log (control characters escaped).
fn printable(name: &str) -> String {
    name.escape_debug().to_string()
}

/// A TCP connection that opens with an HTTP request is upgraded to a
/// WebSocket carrying the same byte stream; anything else is the plain
/// protocol (its first bytes are given back to the reader). Runs under the
/// handshake deadline. Returns the halves and whether it is a WebSocket.
fn sniff_websocket(mut reader: Box<dyn Read + Send>, mut writer: Box<dyn Write + Send>) -> std::io::Result<Sniffed> {
    let mut first = [0u8; 4];
    reader.read_exact(&mut first)?;
    if !ws::is_http_prefix(&first) {
        return Ok((Box::new(std::io::Cursor::new(first.to_vec()).chain(reader)), writer, false));
    }
    ws::accept(&first, &mut reader, &mut writer)?;
    let (reader, writer) = ws::split(ws::Role::Server, reader, writer);
    Ok((Box::new(reader), Box::new(writer), true))
}

fn serve_client(host: &Arc<Host>, link: Link, slot: Option<HandshakeSlot>) -> Result<()> {
    let remote = matches!(link, Link::Tcp(_));
    let peer_pid = match &link {
        Link::Pipe(pipe) => pipe.client_pid(),
        Link::Tcp(_) => None,
    };
    let ((reader, writer, closer), tcp_ends) = link.split()?;
    if let Some(ends) = &tcp_ends {
        ends.closer.set_deadline(Some(HANDSHAKE_TIMEOUT));
    }
    let (reader, writer, websocket) = if remote {
        match sniff_websocket(reader, writer) {
            Ok(halves) => halves,
            Err(error) => {
                closer.close();
                return Err(error.into());
            }
        }
    } else {
        (reader, writer, false)
    };
    let mut reader = FrameReader::new(reader).with_limit(PRE_AUTH_FRAME);
    let mut writer = FrameWriter::new(writer);
    let opening = handshake::serve(
        &mut reader,
        &mut writer,
        remote,
        build_id(),
        |remote_id| match (remote_id, &tcp_ends) {
            (Some(id), _) => host.app().remote_credential(id),
            // Through web forwarding only paired clients are served.
            (None, _) if websocket => Credential::Refused,
            // Over TCP the token only authenticates this user's processes here.
            (None, Some(ends)) if !host.token_allowed(ends) => Credential::Refused,
            (None, _) => Credential::Secret(host.token.clone()),
        },
        host.app().pairing(),
    );
    if let Some(ends) = &tcp_ends {
        ends.closer.set_deadline(None);
    }
    drop(slot);
    let accepted = match opening {
        Ok(Opening::Client(accepted)) => accepted,
        Ok(Opening::Paired { client_id, name }) => {
            let via = if websocket { " over a WebSocket" } else { "" };
            log(&host.instance, &format!("paired remote client {client_id:?} (it calls itself {name:?}){via}"));
            closer.close();
            return Ok(());
        }
        Err(error) => {
            closer.close();
            return Err(error);
        }
    };
    reader.set_limit(MAX_FRAME);
    let v8 = accepted.protocol >= 8;
    let info = accepted.client.unwrap_or_default();
    if let Some(remote_id) = info.remote_id.as_ref().filter(|_| !accepted.satellite) {
        host.app().remote_seen(remote_id);
    }
    if remote && v8 {
        reader.accept_compressed()?;
        if host.compress && info.caps & CAP_COMPRESS != 0 {
            writer.compress()?;
        }
    }
    // A satellite is decided by its credential, before anything a client gets is sent.
    if accepted.satellite {
        if info.caps & protocol::CAP_SATELLITE == 0 {
            closer.close();
            bail!("satellite {:?} did not announce CAP_SATELLITE", info.remote_id);
        }
        log(&host.instance, &format!("satellite {:?} connected{}", info.remote_id.as_deref().unwrap_or_default(), if websocket { " (WebSocket)" } else { "" }));
        return host.app().serve_satellite(SatelliteLink { reader, writer, closer, info, websocket });
    }

    // A client that stops taking data never holds its writer (or a WebSocket reader answering pings) for long.
    if let Some(ends) = &tcp_ends {
        ends.closer.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        // Little is buffered past the writer: what waits stays in the outbox, where an echo goes first.
        let _ = ends.closer.set_send_buffer(CLIENT_SEND_BUFFER);
    }
    let outbox = Arc::new(Outbox::new());
    let id = host.next_client.fetch_add(1, Ordering::Relaxed);
    let mut client = Client::new(id, accepted.protocol, remote, info.caps, outbox.clone(), closer.clone());
    client.remote_id = info.remote_id.clone();
    client.peer_pid = peer_pid;
    client.websocket = websocket;
    // The newest connection of a client session owns its input; the previous
    // one (half-open after a network drop) stops applying input and is closed.
    let mut ready_ack = 0;
    let mut replaced: Option<Arc<Client>> = None;
    if v8 {
        let session = host.input_session(&info.session);
        let previous = {
            let mut state = lock(&session);
            state.seen = Instant::now();
            ready_ack = state.applied;
            std::mem::replace(&mut state.owner, id)
        };
        replaced = lock(&host.clients).get(&previous).cloned().or_else(|| lock(&host.serving).get(&previous).cloned());
        if let Some(old) = &replaced {
            // Torn down at once: never waits on the old connection (it may be stuck writing).
            host.drop_client(old, Some(&format!("client {previous} replaced by its reconnection {id}")));
        }
        client.session = Some(session);
    }
    let client = Arc::new(client);
    if let Some(liveness) = &client.liveness {
        liveness.received();
        // Every byte that arrives counts, not only whole messages: a large
        // message on a slow link keeps the client alive while it flows.
        reader.set_liveness(liveness.clone());
        // A stalled write is the client's only while it is silent too: a
        // tunnel gives its window back in large steps (R2-A-02).
        if let Some(ends) = &tcp_ends {
            let liveness = liveness.clone();
            ends.closer.set_write_patience(Arc::new(move || liveness.silence() < CLIENT_WRITE_TIMEOUT), CLIENT_WRITE_PATIENCE);
        }
    }
    if let Err(error) = spawn_writer(Arc::downgrade(host), writer, outbox, &client, Some(host.pump.clone())) {
        closer.close();
        bail!("cannot start the client's writer thread: {error}");
    }
    if remote {
        let paired = info.remote_id.as_deref().map(|r| format!(" as paired client {r:?}")).unwrap_or_default();
        let via = if websocket { ", WebSocket" } else { "" };
        log(&host.instance, &format!("remote client {id} connected ({}, protocol {}, compression {}{via}){paired}", printable(&info.name), accepted.protocol, host.compress && info.caps & CAP_COMPRESS != 0));
    }
    client.send(ServiceMsg::Ready8 { pid: std::process::id(), input_ack: ready_ack });
    // Clients that acknowledge what they read (`CAP_ACK`) are told so; flow
    // control counts from the next byte. Remote clients that do not (older
    // builds, the phone) get the stopgap pacing while they type.
    client.send(ServiceMsg::Caps { bits: SERVICE_ACKS });
    if remote && info.caps & CAP_ACK == 0 {
        client.outbox.set_paced();
    }
    host.register(&client);
    let revoked = {
        // What follows is this end's work, not the client's silence.
        let _busy = client.liveness.as_ref().map(|l| l.busy());
        // A revocation between the handshake and now closed only the clients
        // listed then: check again now that this one is listed.
        let revoked = client.remote_id.as_ref().is_some_and(|remote_id| !host.app().remote_active(remote_id));
        if revoked {
            host.drop_client(&client, Some(&format!("remote client {:?} was revoked while it connected", client.remote_id.as_deref().unwrap_or_default())));
        }
        // What the replaced connection read before it went is handled first
        // (for a moment at most): this one's input never overtakes it (R2-A-06).
        if let Some(old) = replaced {
            old.wait_drained(DRAIN_WAIT);
        }
        revoked
    };
    host.touch();

    // The backend's snapshot is the worker's first job (R2-A-03): reading starts at once.
    let result = if revoked { Ok(()) } else { serve_messages(host, &client, &mut reader) };
    host.drop_client(&client, None);
    // Whatever a message handled meanwhile left for this client.
    host.size_leave(id, None);
    host.app().client_disconnected(id);
    {
        // Never left listed, even when it was dropped before it was listed.
        let mut clients = lock(&host.clients);
        if clients.get(&id).is_some_and(|listed| Arc::ptr_eq(listed, &client)) {
            clients.remove(&id);
        }
    }
    lock(&host.closing).remove(&id);
    client.drained();
    if remote {
        log(&host.instance, &format!("remote client {id} disconnected"));
    }
    result
}

impl Host {
    /// Lists a client that just connected and queues it the terminal list,
    /// under the client list's lock: a launch or an exit either is in that
    /// list or reaches the client as a broadcast, never neither. A client
    /// dropped meanwhile (its writer failed) is not listed.
    fn register(&self, client: &Arc<Client>) {
        let mut clients = lock(&self.clients);
        if client.is_dropped() {
            return;
        }
        // Lock order: clients before terms.
        let infos: Vec<_> = lock(&self.terms).values().map(|t| t.info()).collect();
        client.send(ServiceMsg::Terminals(infos));
        clients.insert(client.id, client.clone());
    }
}

/// Work a client's worker thread does in order ([`serve_messages`]).
enum Work {
    Message(ClientMsg),
    /// Input the reader admitted (numbered input is admitted in the order it
    /// arrives) for a terminal with work queued: applied after that work.
    Input { id: String, input: Input },
    /// The client stopped showing a terminal that had work queued (its
    /// launch): its size wish goes once that work is done (R2-A-07).
    Leave(String),
    /// The backend's snapshot for a client that just connected (its first job).
    Connected,
}

impl Work {
    /// The terminal it is about, while it waits.
    fn terminal(&self) -> Option<&str> {
        match self {
            Work::Message(message) => message_terminal(message),
            Work::Input { id, .. } | Work::Leave(id) => Some(id),
            Work::Connected => None,
        }
    }
}

/// Launches one client connection runs at once (each on a thread of its own).
const MAX_LAUNCHES: usize = 4;
/// Work for a terminal another launch is starting (another connection's, the
/// restore's) waits for that launch at most this long.
const LAUNCH_AWAIT: Duration = Duration::from_secs(20);
/// A reconnection waits at most this long for what its predecessor read to be handled.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// A write to a client still heard from waits at most this long (`tcp::TcpCloser::set_write_patience`).
const CLIENT_WRITE_PATIENCE: Duration = Duration::from_secs(60);

/// What a client connection has under way besides its reader: the worker's
/// queue per terminal, and the launches running on threads of their own
/// with the work held behind each, in order.
#[derive(Default)]
struct Pending {
    /// Per terminal: work queued or under way (later input for it waits behind).
    waiting: Mutex<HashMap<String, usize>>,
    /// Terminals with a launch under way on a thread of its own (this
    /// connection's, or another's it waits for): their later work, in order.
    held: Mutex<HashMap<String, std::collections::VecDeque<Work>>>,
    /// Launch threads running.
    threads: Mutex<usize>,
    idle: std::sync::Condvar,
}

impl Pending {
    /// A piece of work for `terminal` was handled.
    fn done(&self, terminal: Option<&str>) {
        let Some(terminal) = terminal else { return };
        let mut waiting = lock(&self.waiting);
        if let Some(count) = waiting.get_mut(terminal) {
            *count -= 1;
            if *count == 0 {
                waiting.remove(terminal);
            }
        }
    }

    /// Waits for room for one more launch thread and takes it.
    fn start_thread(&self) {
        let mut threads = lock(&self.threads);
        while *threads >= MAX_LAUNCHES {
            threads = self.idle.wait(threads).unwrap_or_else(|e| e.into_inner());
        }
        *threads += 1;
    }

    fn end_thread(&self) {
        *lock(&self.threads) -= 1;
        self.idle.notify_all();
    }

    /// Waits until no launch thread runs.
    fn wait_threads(&self) {
        let mut threads = lock(&self.threads);
        while *threads > 0 {
            threads = self.idle.wait(threads).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Handles one piece of work (on the worker or a launch thread); `quiet`:
/// an error is not sent (the input after a launch that failed: one notice).
fn handle(host: &Arc<Host>, client: &Arc<Client>, work: Work, quiet: bool) -> bool {
    let result = match work {
        Work::Message(message) => dispatch(host, client, message),
        Work::Input { id, input } => apply_input(host, client, &id, &input),
        Work::Leave(id) => {
            // Shown again meanwhile: it keeps its wish.
            if !lock(&client.subs).contains_key(&id) {
                host.size_leave(client.id, Some(&id));
            }
            Ok(())
        }
        Work::Connected => {
            host.app().client_connected(client);
            Ok(())
        }
    };
    match result {
        Ok(()) => true,
        Err(error) => {
            if !quiet {
                client.send(ServiceMsg::Error { id: None, message: format!("{error:#}") });
            }
            false
        }
    }
}

/// The worker's step: work for a terminal whose launch runs on a thread is
/// held behind it; a launch gets a thread of its own (launches of different
/// terminals run side by side, each terminal's work stays in order);
/// work for a terminal another launch is starting waits for it there;
/// everything else is handled here, in order.
fn run_work(host: &Arc<Host>, client: &Arc<Client>, pending: &Arc<Pending>, work: Work) {
    let terminal = work.terminal().map(str::to_owned);
    if let Some(id) = &terminal {
        let mut held = lock(&pending.held);
        if let Some(queue) = held.get_mut(id) {
            queue.push_back(work);
            return;
        }
        let launch = matches!(work, Work::Message(ClientMsg::Launch { .. }));
        if launch || host.is_launching(id) {
            held.insert(id.clone(), std::collections::VecDeque::new());
            drop(held);
            run_held(host, client, pending, id.clone(), work, !launch);
            return;
        }
    }
    handle(host, client, work, false);
    pending.done(terminal.as_deref());
}

/// Runs `first` for terminal `id` on a thread of its own (a launch; or,
/// `await_launch`, work that waits for another's launch of it first), then
/// the work held behind it, in order. A thread that cannot start runs here.
fn run_held(host: &Arc<Host>, client: &Arc<Client>, pending: &Arc<Pending>, id: String, first: Work, await_launch: bool) {
    pending.start_thread();
    let (host, client, pending_job) = (host.clone(), client.clone(), pending.clone());
    let body = move || {
        if await_launch {
            let until = Instant::now() + LAUNCH_AWAIT;
            while host.is_launching(&id) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let mut work = Some(first);
        let mut failed_input = false;
        loop {
            let next = match work.take() {
                Some(work) => work,
                None => {
                    let mut held = lock(&pending_job.held);
                    match held.get_mut(&id).and_then(std::collections::VecDeque::pop_front) {
                        Some(work) => work,
                        None => {
                            held.remove(&id);
                            break;
                        }
                    }
                }
            };
            // Keys for a terminal that did not start: one notice, not one per key.
            let input = matches!(next, Work::Input { .. });
            let handled = handle(&host, &client, next, input && failed_input);
            failed_input |= input && !handled;
            pending_job.done(Some(&id));
        }
        pending_job.end_thread();
    };
    let job = Arc::new(Mutex::new(Some(body)));
    let spawned = {
        let job = job.clone();
        try_spawn("client-launch", move || {
            let body = lock(&job).take();
            if let Some(body) = body {
                body();
            }
        })
    };
    if spawned.is_err() {
        let body = lock(&job).take();
        if let Some(body) = body {
            body();
        }
    }
}

/// The terminal a message is about (input, sizes, launches).
fn message_terminal(message: &ClientMsg) -> Option<&str> {
    match message {
        ClientMsg::Launch { id, .. }
        | ClientMsg::Stop { id, .. }
        | ClientMsg::Forget { id }
        | ClientMsg::Input { id, .. }
        | ClientMsg::Input8 { id, .. }
        | ClientMsg::Resize { id, .. }
        | ClientMsg::Claim { id }
        | ClientMsg::Scroll { id, .. }
        | ClientMsg::Wheel { id, .. }
        | ClientMsg::Click { id, .. } => Some(id),
        _ => None,
    }
}

/// Whether the reader handles a message itself: terminal input and sizes,
/// subscriptions and heartbeats, which are quick. Everything else (launches
/// and stops, documents, the bus, todo and notebook operations, requests,
/// uploads) may take long and goes to the client's worker, in order.
fn handled_inline(message: &ClientMsg) -> bool {
    matches!(
        message,
        ClientMsg::Input { .. }
            | ClientMsg::Input8 { .. }
            | ClientMsg::Heartbeat
            | ClientMsg::Subscribe { .. }
            | ClientMsg::Subscribe8 { .. }
            | ClientMsg::Resize { .. }
            | ClientMsg::Claim { .. }
            | ClientMsg::Scroll { .. }
            | ClientMsg::Wheel { .. }
            | ClientMsg::Click { .. }
            | ClientMsg::Shutdown
            | ClientMsg::Hello { .. }
            | ClientMsg::Hello8 { .. }
            | ClientMsg::Auth { .. }
            | ClientMsg::Sat(_)
            | ClientMsg::Received { .. }
    )
}

/// Reads a client's messages until its connection ends. Quick ones are
/// handled here (input, sizes, subscriptions, heartbeats, acknowledgements,
/// `Stats`, and a remote client's backend requests, which only queue); slow
/// ones go to the client's worker thread, which handles them in order, so the
/// reader keeps reading (heartbeats, keystrokes into other terminals) while a
/// document load runs. Launches run on threads of their own ([`run_work`]). A
/// message about a terminal with work waiting (a keystroke right after its
/// `Launch`), or that another launch is starting, waits behind that work. The
/// worker's first job is the backend's snapshot for the new client. Whatever
/// this thread does itself is not the client's silence ([`Liveness::busy`]).
fn serve_messages(host: &Arc<Host>, client: &Arc<Client>, reader: &mut FrameReader) -> Result<()> {
    let (work_tx, work_rx) = bounded::<Work>(WORK_QUEUE);
    let pending: Arc<Pending> = Arc::default();
    lock(&host.serving).insert(client.id, client.clone());
    let worker = {
        let (host, client, pending) = (host.clone(), client.clone(), pending.clone());
        std::thread::Builder::new().name("client-work".into()).spawn(move || {
            for work in work_rx {
                client.work_since.store(now_ms(), Ordering::Release);
                run_work(&host, &client, &pending, work);
                client.work_since.store(0, Ordering::Release);
            }
        })
    };
    let worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            lock(&host.serving).remove(&client.id);
            return Err(error.into());
        }
    };
    let _ = work_tx.send(Work::Connected);
    let mut undecodable = 0u64;
    let result = (|| -> Result<()> {
        while let Some(frame) = reader.recv_lenient::<ClientMsg>()? {
            // A client the service dropped is not served further.
            if client.is_dropped() {
                break;
            }
            if let Some(liveness) = &client.liveness {
                liveness.received();
            }
            host.touch();
            // A message of a newer build is skipped, not fatal: the stream is still in sync.
            let message = match frame {
                Ok(message) => message,
                Err(body) => {
                    undecodable += 1;
                    if undecodable == 1 {
                        log(&host.instance, &format!("client {} sent a message this build cannot read (kind {}); skipped (mixed builds?)", client.id, body.first().copied().unwrap_or_default()));
                    }
                    continue;
                }
            };
            let _busy = client.liveness.as_ref().map(|l| l.busy());
            // Work waits for it, or another launch of it is starting (R2-A-07).
            let queued = message_terminal(&message).is_some_and(|id| lock(&pending.waiting).contains_key(id) || host.is_launching(id));
            let mut leave = Vec::new();
            match message {
                // A tile hidden while its launch waits: its size wish goes once the launch is done.
                ClientMsg::Subscribe { ids } => leave = subscribe(host, client, ids, None),
                ClientMsg::Subscribe8 { ids, focused } => leave = subscribe(host, client, ids, focused),
                message if !queued && (handled_inline(&message) || answered_at_once(client, &message)) => {
                    if let Err(error) = dispatch(host, client, message) {
                        client.send(ServiceMsg::Error { id: None, message: format!("{error:#}") });
                    }
                }
                message => {
                    let work = match message {
                        // Admitted now, in arrival order; applied behind its terminal's work.
                        ClientMsg::Input8 { id, input, seq } => match admit(client, seq) {
                            true => Work::Input { id, input },
                            false => continue,
                        },
                        ClientMsg::Input { id, input } => Work::Input { id, input },
                        other => Work::Message(other),
                    };
                    if !queue(&pending, &work_tx, work) {
                        break;
                    }
                }
            }
            leave.retain(|id| lock(&pending.waiting).contains_key(id));
            for id in leave {
                if !queue(&pending, &work_tx, Work::Leave(id)) {
                    break;
                }
            }
        }
        Ok(())
    })();
    drop(work_tx);
    // Not a pending close any more: what the worker still does shows as its work.
    lock(&host.closing).remove(&client.id);
    // What was read before the end is still handled (as it would have been inline).
    let _ = worker.join();
    pending.wait_threads();
    lock(&host.serving).remove(&client.id);
    result
}

/// Queues work for the client's worker (counted for its terminal); false once the worker is gone.
fn queue(pending: &Pending, work_tx: &Sender<Work>, work: Work) -> bool {
    if let Some(id) = work.terminal() {
        *lock(&pending.waiting).entry(id.to_owned()).or_default() += 1;
    }
    work_tx.send(work).is_ok()
}

/// Messages the reader also answers itself: `Stats` (counts), and a remote
/// client's backend requests (parsed and queued for the backend's threads;
/// remote administration is refused at once to remote clients). A local
/// client's requests stay on its worker (its remote administration writes
/// settings).
fn answered_at_once(client: &Client, message: &ClientMsg) -> bool {
    match message {
        ClientMsg::Stats => true,
        ClientMsg::Rpc { .. } => client.remote,
        _ => false,
    }
}

/// Writes queued messages; protocol-8 clients also get a heartbeat every 2 s.
/// A message that cannot be encoded (too large) is skipped, not fatal. A write
/// that fails (the connection ended, or the client took nothing until the
/// write deadline) or a lane that overflowed drops the client at once. Once a
/// frame is written the pump is woken: the tile may get its next one.
fn spawn_writer(host: Weak<Host>, mut writer: FrameWriter, outbox: Arc<Outbox>, client: &Arc<Client>, pump: Option<Sender<()>>) -> std::io::Result<()> {
    let heartbeat = client.liveness.as_ref().map(|l| l.interval);
    let session = client.session.clone();
    let closer = client.closer.clone();
    // Weak: the writer must not keep the client (and so its outbox) alive.
    let weak = Arc::downgrade(client);
    try_spawn("client-writer", move || {
        let mut failure: Option<std::io::Error> = None;
        let mut beat = heartbeat.map(Beat::new);
        loop {
            let deadline = beat.as_ref().map_or_else(|| Instant::now() + Duration::from_secs(3600), Beat::next);
            let sent = match outbox.next(deadline) {
                Next::Closed => break,
                Next::Timeout => Ok(0),
                Next::Item(Taken { message, frame_of, urgent }) => {
                    // Flow control counts from the byte after `Caps` (the client does too).
                    let caps = matches!(&message, Outgoing::Message(ServiceMsg::Caps { .. }));
                    let sent = match message {
                        Outgoing::Message(message) => writer.send(&message),
                        Outgoing::Image { term, image_id, width, height, png } => {
                            let sent = writer.send(&ImageRef { term: &term, image_id, width, height, png: &png });
                            outbox.image_written(png.len());
                            sent
                        }
                    };
                    match &sent {
                        Ok(_) if caps => outbox.start_window(),
                        Ok(bytes) => outbox.wrote(*bytes, urgent),
                        Err(_) => {}
                    }
                    if let Some(tile) = frame_of {
                        // Too large to encode: the client never got it, though
                        // the tile's diff state counts it. Its next frame is
                        // full instead of a diff against rows it lacks (S3-11).
                        if sent.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::InvalidData)
                            && let Some(client) = weak.upgrade()
                            && let Some(sub) = lock(&client.subs).get_mut(&tile)
                        {
                            sub.hashes.clear();
                        }
                        outbox.frame_written(&tile);
                        if let Some(pump) = &pump {
                            let _ = pump.try_send(());
                        }
                    }
                    sent
                }
            };
            match sent {
                Ok(_) => {}
                // Nothing was written: the connection is still in sync.
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
            if beat.as_mut().is_some_and(|b| b.due(Instant::now())) {
                let input_ack = session.as_ref().map(|s| lock(s).applied).unwrap_or(0);
                match writer.send(&ServiceMsg::Heartbeat { input_ack }) {
                    Ok(bytes) => outbox.wrote(bytes, true),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        closer.close();
        outbox.close();
        if let (Some(host), Some(client)) = (host.upgrade(), weak.upgrade()) {
            let why = outbox.overflow().or_else(|| {
                failure.filter(|e| e.kind() == std::io::ErrorKind::TimedOut).map(|_| format!("client {} took no data for {} s; closing", client.id, CLIENT_WRITE_TIMEOUT.as_secs()))
            });
            host.drop_client(&client, why.as_deref());
        }
    })
}

/// What the service writes every [`HEALTH_EVERY`] for `service status` and
/// `doctor` (`<home>/run/health-<instance>.json`): things that are under way
/// and should not last. Written by a thread of its own, so a stale file is
/// itself a sign (a stalled disk, or a starved process).
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone, PartialEq)]
pub struct Health {
    pub pid: u32,
    pub updated_ms: u64,
    /// Clients dropped whose connection thread has not returned: (client, since ms).
    #[serde(default)]
    pub pending_closes: Vec<(u64, u64)>,
    /// Launches under way: (terminal, since ms).
    #[serde(default)]
    pub launching: Vec<(String, u64)>,
    /// Clients whose worker is busy with one message (a launch, a document
    /// load, a request; the client's later slow messages wait): (client, since ms).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working: Vec<(u64, u64)>,
}

/// How often the health file is written.
pub const HEALTH_EVERY: Duration = Duration::from_secs(2);
/// A terminal that printed nothing this long after its launch looks stuck.
pub const QUIET_AFTER_MS: u64 = 30_000;

pub fn health_path(instance: &Instance) -> std::path::PathBuf {
    instance.home.join("run").join(format!("health-{}.json", instance.name))
}

/// The running service's health file, when there is one.
pub fn read_health(instance: &Instance) -> Option<Health> {
    serde_json::from_str(&std::fs::read_to_string(health_path(instance)).ok()?).ok()
}

/// Signs that the service is wedged (README, `service status`): its health
/// file stale, a client close or a launch that does not end, a terminal that
/// printed nothing long after its launch. `terms` as the service lists them;
/// `now` on the service's clock (the same machine).
pub fn wedge_signs(health: Option<&Health>, terms: &[crate::protocol::TermInfo], now: u64) -> Vec<String> {
    let mut signs = Vec::new();
    let age = |since: u64| now.saturating_sub(since) / 1000;
    if let Some(health) = health {
        if now.saturating_sub(health.updated_ms) > 10_000 {
            signs.push(format!("its health file was last written {} s ago (a stalled thread or disk)", age(health.updated_ms)));
        }
        for (client, since) in health.pending_closes.iter().filter(|(_, since)| now.saturating_sub(*since) > 5_000) {
            signs.push(format!("client {client} was closed {} s ago and its connection thread has not returned", age(*since)));
        }
        for (id, since) in health.launching.iter().filter(|(_, since)| now.saturating_sub(*since) > 30_000) {
            signs.push(format!("terminal {id} has been launching for {} s", age(*since)));
        }
        for (client, since) in health.working.iter().filter(|(_, since)| now.saturating_sub(*since) > WORK_STUCK_MS) {
            signs.push(format!("a message of client {client} has been handled for {} s (its later requests wait)", age(*since)));
        }
    }
    for term in terms.iter().filter(|t| t.alive && t.last_output_ms == 0 && now.saturating_sub(t.started_ms) > QUIET_AFTER_MS) {
        signs.push(format!("terminal {} (pid {}) printed nothing in the {} s since its launch", term.id, term.pid, age(term.started_ms)));
    }
    signs
}

/// Writes the health file every [`HEALTH_EVERY`].
fn health_loop(host: Arc<Host>) {
    let path = health_path(&host.instance);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    loop {
        if host.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        // Dropped connections whose worker still handles what they read count too.
        let clients: Vec<Arc<Client>> = lock(&host.serving).values().cloned().collect();
        let health = Health {
            pid: std::process::id(),
            updated_ms: now_ms(),
            pending_closes: lock(&host.closing).iter().map(|(id, since)| (*id, *since)).collect(),
            launching: lock(&host.launching).iter().map(|(id, since)| (id.clone(), *since)).collect(),
            working: clients.iter().filter_map(|c| Some((c.id, c.work_since.load(Ordering::Acquire))).filter(|(_, since)| *since != 0)).collect(),
        };
        if let Ok(text) = serde_json::to_string(&health) {
            // Replaced whole: a reader never sees half a file.
            let partial = path.with_extension("json.tmp");
            if std::fs::write(&partial, text).is_ok() && std::fs::rename(&partial, &path).is_err() {
                let _ = std::fs::remove_file(&partial);
            }
        }
        std::thread::sleep(HEALTH_EVERY);
    }
}

/// Closes protocol-8 connections that stayed silent too long (no heartbeat);
/// on the same tick, ends the size waits of owners that left.
fn liveness_loop(host: Arc<Host>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        host.size_expire();
        let clients: Vec<Arc<Client>> = lock(&host.clients).values().cloned().collect();
        for client in clients {
            if let Some(liveness) = &client.liveness
                && liveness.dead()
            {
                // Logged once: the client leaves the list at once.
                host.drop_client(&client, Some(&format!("client {} silent for {:.1} s; closing", client.id, liveness.silence().as_secs_f32())));
            }
        }
    }
}

/// Replaces a client's subscriptions; returns the terminals it no longer shows.
fn subscribe(host: &Host, client: &Client, ids: Vec<String>, focused: Option<String>) -> Vec<String> {
    let mut subs = lock(&client.subs);
    let hidden: Vec<String> = subs.keys().filter(|id| !ids.contains(id)).cloned().collect();
    subs.retain(|id, _| ids.contains(id));
    let mut shown = Vec::new();
    for id in ids.into_iter().take(16) {
        if !subs.contains_key(&id) {
            shown.push(id.clone());
        }
        subs.entry(id).or_default();
    }
    let shown: Vec<String> = subs.keys().cloned().collect();
    // A frame of a tile no longer shown is not sent (its `Sub` is gone: a new one starts with a full frame).
    client.outbox.retain_tiles(&shown);
    client.outbox.set_focused(focused.clone());
    drop(subs);
    // A terminal this client no longer shows stops following its size; one
    // it shows again within the moment its size is kept keeps it.
    for id in &hidden {
        host.size_leave(client.id, Some(id));
    }
    for id in shown {
        if let Some(policy) = lock(&host.sizes).get_mut(&id) {
            policy.back(client.id);
        }
    }
    *lock(&client.focused) = focused;
    let _ = host.pump.try_send(());
    hidden
}

fn apply_input(host: &Host, client: &Client, id: &str, input: &Input) -> Result<()> {
    host.input_from(client.id, id);
    if !matches!(input, Input::Focus(_)) {
        // The terminal takes the size of the client typing in it. A resize
        // the PTY refused never costs the key (the next `Resize` retries it).
        let _ = host.size_claim(client.id, id);
        host.note_active(id);
        host.app().input_noted(id);
        // Its echo is due: a client under the stopgap pacing gets less else meanwhile.
        client.outbox.typed();
    }
    host.terminal(id)?.send_input(input)?;
    let _ = host.pump.try_send(());
    Ok(())
}

/// Whether this client may apply numbered input `seq` now (each at most
/// once, only from the session's newest connection); records it. The
/// session's lock is held only for that, never while the input is applied.
fn admit(client: &Client, seq: u64) -> bool {
    client.session.as_ref().is_none_or(|session| lock(session).admit(client.id, seq))
}

fn dispatch(host: &Arc<Host>, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
    match message {
        ClientMsg::Launch { id, kind, cwd, cols, rows } => match host.start(&id, kind, &cwd, cols, rows) {
            // A new terminal has the launching client's size and follows it.
            Ok(Started::New) => {
                let mut sizes = lock(&host.sizes);
                let policy = sizes.entry(id).or_default();
                policy.wish(client.id, cols.clamp(20, 1000), rows.clamp(5, 500));
                policy.claim(client.id);
            }
            // It runs already: the client learns so (a retry heals a list it missed),
            // as it is now (an exit meanwhile is not overtaken).
            Ok(Started::Running) => host.send_terms(client, Some(&id)),
            // Its launch under way tells every client when it is done.
            Ok(Started::Launching) => {}
            Err(error) => {
                client.send(ServiceMsg::Error { id: Some(id), message: format!("{error:#}") });
            }
        },
        // A full refresh: every terminal here; the backend sends the sessions.
        ClientMsg::RefreshSessions => {
            host.send_terms(client, None);
            host.app().message(client, ClientMsg::RefreshSessions)?;
        }
        ClientMsg::Subscribe { ids } => {
            subscribe(host, client, ids, None);
        }
        ClientMsg::Subscribe8 { ids, focused } => {
            subscribe(host, client, ids, focused);
        }
        ClientMsg::Input { id, input } => apply_input(host, client, &id, &input)?,
        ClientMsg::Input8 { id, input, seq } => {
            if admit(client, seq) {
                apply_input(host, client, &id, &input)?;
            }
        }
        ClientMsg::Heartbeat => {}
        // What the client read so far: flow control lets more go.
        ClientMsg::Received { bytes } => client.outbox.acked(bytes),
        ClientMsg::Resize { id, cols, rows } => {
            host.terminal(&id)?;
            host.size_wish(client.id, &id, cols, rows)?;
        }
        ClientMsg::Claim { id } => {
            host.terminal(&id)?;
            host.size_claim(client.id, &id)?;
        }
        ClientMsg::Scroll { id, delta } => {
            host.terminal(&id)?.scroll(delta);
            let _ = host.pump.try_send(());
        }
        ClientMsg::Wheel { id, up, col, row, mods } => {
            host.input_from(client.id, &id);
            host.terminal(&id)?.wheel(up, col, row, mods)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Click { id, col, row, button, mods } => {
            host.input_from(client.id, &id);
            let _ = host.size_claim(client.id, &id);
            host.terminal(&id)?.click(button, col, row, mods)?;
            let _ = host.pump.try_send(());
        }
        ClientMsg::Stop { id, force } => host.stop_terminal(&id, force)?,
        ClientMsg::Forget { id } => {
            let forgotten = host.tell_terms(|terms| match terms.get(&id).is_some_and(|t| !t.alive.load(Ordering::Acquire)) {
                true => {
                    terms.remove(&id);
                    (true, vec![ServiceMsg::TerminalGone(id.clone())])
                }
                false => (false, Vec::new()),
            });
            if forgotten {
                lock(&host.input_clients).remove(&id);
                lock(&host.sizes).remove(&id);
            }
        }
        // A paired remote client (a phone, a home PC) disconnects; it never
        // ends the service and every session on it.
        ClientMsg::Shutdown if client.remote_id.is_some() => bail!("remote clients cannot shut the service down"),
        ClientMsg::Shutdown => {
            let host = host.clone();
            spawn_named("shutdown", move || host.shutdown());
        }
        ClientMsg::Stats => {
            client.send(ServiceMsg::Stats(host.stats()));
        }
        ClientMsg::Hello { .. } | ClientMsg::Hello8 { .. } | ClientMsg::Auth { .. } => bail!("unexpected handshake message"),
        ClientMsg::Sat(_) => bail!("satellite messages come only from a satellite's own connection"),
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
        terminal: sub.terminal.clone(),
        generation: sub.generation,
        hashes,
        cols,
        rows,
        cursor,
        offset: display_offset,
        shape: cursor_shape,
        blink: cursor_blink,
        modes,
        placements: sub.placements.clone(),
        visible: sub.visible.clone(),
        images: sub.images.clone(),
        images_due: sub.images_due,
    };
    let frame = Frame8 { id: id.to_owned(), cols, rows, full, changed, cursor, cursor_shape, cursor_blink, display_offset, history, modes, links };
    Some((frame, next))
}

/// A screen the pump asked a terminal's emulator for and has not received yet.
struct Ask {
    terminal: Arc<Terminal>,
    reply: Receiver<Snapshot>,
    /// The terminal's generation when it was asked: the answer shows at least that.
    generation: u64,
}

/// A screen received, shared by every client that shows its terminal.
struct Answer {
    terminal: Arc<Terminal>,
    /// Without its images (`images`).
    snapshot: Snapshot,
    images: Vec<SnapshotImage>,
    hashes: Vec<u64>,
    generation: u64,
}

/// Sends clients the frames their subscriptions need. It never waits on a
/// terminal: the screens it needs are asked for all at once
/// ([`Terminal::ask_snapshot`], at most one request per terminal) and turned
/// into frames as their answers come, so a busy or stuck engine holds only
/// its own frames. A tile gets its next frame once its last one was written
/// (one waits per tile at most; remote tiles are paced from that write),
/// focused tiles first.
fn pump(host: Arc<Host>, wake: Receiver<()>) {
    let mut asks: HashMap<String, Ask> = HashMap::new();
    // When each terminal was last asked: at most every FRAME_INTERVAL.
    let mut asked_at: HashMap<String, Instant> = HashMap::new();
    let mut due: Option<Instant> = None;
    loop {
        let poll = Instant::now() + PUMP_POLL;
        wait_for_work(&wake, &asks, due.map_or(poll, |d| d.min(poll)));
        while wake.try_recv().is_ok() {}
        let mut answers: HashMap<String, Answer> = HashMap::new();
        asks.retain(|id, ask| match ask.reply.try_recv() {
            Ok(mut snapshot) => {
                let images = std::mem::take(&mut snapshot.images);
                let hashes = snapshot.lines.iter().map(|row| row_hash8(row, &snapshot.links)).collect();
                answers.insert(id.clone(), Answer { terminal: ask.terminal.clone(), snapshot, images, hashes, generation: ask.generation });
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        due = None;
        let mut wanted: HashMap<String, (Arc<Terminal>, u64)> = HashMap::new();
        let clients: Vec<Arc<Client>> = lock(&host.clients).values().cloned().collect();
        for focused in [true, false] {
            for client in &clients {
                if let Some(at) = pump_client(&host, client, focused, &answers, &asks, &mut wanted) {
                    due = sooner(due, at);
                }
            }
        }
        let now = Instant::now();
        for (id, (terminal, generation)) in wanted {
            if let Some(next) = asked_at.get(&id).map(|at| *at + FRAME_INTERVAL).filter(|next| *next > now) {
                due = sooner(due, next);
                continue;
            }
            match terminal.ask_snapshot() {
                Some(reply) => {
                    asked_at.insert(id.clone(), now);
                    asks.insert(id, Ask { terminal, reply, generation });
                }
                // Its control queue is full (a busy engine) or it stopped: asked again later.
                None => due = sooner(due, now + PUMP_POLL),
            }
        }
        asked_at.retain(|id, at| asks.contains_key(id) || now.saturating_duration_since(*at) < Duration::from_secs(1));
    }
}

/// Waits until the pump is woken, a screen it asked for arrives, or `until`.
fn wait_for_work(wake: &Receiver<()>, asks: &HashMap<String, Ask>, until: Instant) {
    let mut select = crossbeam_channel::Select::new();
    select.recv(wake);
    for ask in asks.values() {
        select.recv(&ask.reply);
    }
    let _ = select.ready_deadline(until);
}

/// The earlier of two optional times.
fn sooner(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(a.map_or(b, |a| a.min(b)))
}

/// One client's pass (its focused tile, or its other tiles): frames from the
/// screens received (`answers`), and the terminals whose screen it needs
/// (`wanted`; not those already asked). Never holds a lock while waiting and
/// never waits for an image. Returns when a paced tile or a waiting image is due.
fn pump_client(host: &Host, client: &Client, focused_pass: bool, answers: &HashMap<String, Answer>, asks: &HashMap<String, Ask>, wanted: &mut HashMap<String, (Arc<Terminal>, u64)>) -> Option<Instant> {
    let focused = lock(&client.focused).clone();
    let ids: Vec<String> = lock(&client.subs).keys().filter(|id| (focused.as_deref() == Some(id.as_str())) == focused_pass).cloned().collect();
    let mut due: Option<Instant> = None;
    for id in ids {
        let terminal = lock(&host.terms).get(&id).cloned();
        let Some(terminal) = terminal else { continue };
        let (seen, images_due) = {
            let mut subs = lock(&client.subs);
            let Some(sub) = subs.get_mut(&id) else { continue };
            // A relaunch under the same id starts over (its generations restart).
            if !std::ptr::eq(sub.terminal.as_ptr(), Arc::as_ptr(&terminal)) {
                *sub = Sub { terminal: Arc::downgrade(&terminal), ..Sub::default() };
            }
            (sub.generation, sub.images_due)
        };
        let generation = terminal.generation();
        // Images that waited are looked at again once due, even if nothing else changed.
        let images_now = match images_due {
            Some(at) if at > Instant::now() => {
                due = sooner(due, at);
                false
            }
            Some(_) => true,
            None => false,
        };
        if generation == seen && !images_now {
            continue;
        }
        // Its last frame is not written yet: the writer wakes the pump once it is.
        let (pending, written) = client.outbox.frame_state(&id);
        if pending {
            continue;
        }
        // Remote tiles are paced; so are a local client's tiles other than
        // the one it types in (a busy tile would cost a snapshot every pass, S3-15).
        // A remote client that does not acknowledge and typed just now gets
        // its other tiles even less often (the stopgap: its echo goes first).
        let interval = match (client.remote, focused_pass) {
            (true, true) => Some(FOCUSED_INTERVAL),
            (true, false) if client.outbox.slow_others() => Some(TYPING_BACKGROUND_INTERVAL),
            (true, false) => Some(BACKGROUND_INTERVAL),
            (false, false) if focused.is_some() => Some(LOCAL_BACKGROUND_INTERVAL),
            (false, _) => None,
        };
        if let (Some(interval), Some(at)) = (interval, written) {
            let ready = at + interval;
            if Instant::now() < ready {
                due = sooner(due, ready);
                continue;
            }
        }
        let answer = answers.get(&id).filter(|a| Arc::ptr_eq(&a.terminal, &terminal) && a.generation >= seen);
        let Some(answer) = answer else {
            if !asks.get(&id).is_some_and(|ask| Arc::ptr_eq(&ask.terminal, &terminal)) {
                wanted.entry(id).or_insert((terminal, generation));
            }
            continue;
        };
        if client.caps & CAP_IMAGES != 0 {
            match send_images(client, &terminal, &id, &answer.images) {
                ImagesSent::Full => continue,
                ImagesSent::Later(at) => due = sooner(due, at),
                ImagesSent::Done => {}
            }
        }
        let mut subs = lock(&client.subs);
        // Unsubscribed meanwhile.
        let Some(sub) = subs.get_mut(&id) else { continue };
        match diff_frame(&id, sub, answer.snapshot.clone(), answer.hashes.clone()) {
            None => sub.generation = answer.generation,
            // Not queued (gone): the client keeps what it had.
            Some((frame, next)) => {
                if client.send_frame(&id, ServiceMsg::Frame8(frame)) == Push::Queued {
                    host.frames.fetch_add(1, Ordering::Relaxed);
                    *sub = Sub { generation: answer.generation, ..next };
                }
            }
        }
        // The screen moved on since it was asked: look again soon.
        if generation > answer.generation {
            due = sooner(due, Instant::now() + FRAME_INTERVAL);
        }
    }
    due
}

/// What [`send_images`] did.
enum ImagesSent {
    /// Every placed image the client can get is queued.
    Done,
    /// Some wait (being encoded, or over the client's image budget): look again then.
    Later(Instant),
    /// The client's queue is full.
    Full,
}

/// Sends a `CAP_IMAGES` client the images a terminal's placements need that
/// it does not have, then the placements when what it can show changed.
/// Images that cannot be sent (gone, too large) are remembered with their
/// generation and left out of the placements. An image still being encoded,
/// or over the client's queued-image budget, waits; meanwhile the client keeps
/// the pixels it has. Encoded images are shared, not copied per client.
fn send_images(client: &Client, terminal: &Arc<Terminal>, id: &str, images: &[SnapshotImage]) -> ImagesSent {
    let (known, shown, visible_sent, waiting) = {
        let subs = lock(&client.subs);
        let Some(sub) = subs.get(id) else { return ImagesSent::Done };
        (sub.images.clone(), sub.placements.clone(), sub.visible.clone(), sub.images_due.is_some())
    };
    let placements: Vec<Placement> = images.iter().map(|i| i.placement.clone()).collect();
    let current = |i: &SnapshotImage| known.get(&i.placement.image_id).is_some_and(|(generation, _)| *generation == i.generation);
    if placements == shown && images.iter().all(current) && !waiting {
        return ImagesSent::Done;
    }
    let now = Instant::now();
    let mut retry: Option<Instant> = None;
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
        let wait = match terminal.image_png(image_id, image.generation) {
            ImagePng::Ready(encoded) if client.image_fits(encoded.png.len()) => match client.send_image(id, image_id, &encoded) {
                Push::Queued => {
                    have.insert(image_id, (encoded.generation, true));
                    continue;
                }
                Push::Closed => return ImagesSent::Full,
                // Over the bulk budget: it waits.
                Push::Full => now + IMAGE_RETRY,
            },
            ImagePng::Ready(_) => now + IMAGE_RETRY,
            ImagePng::Pending(at) => at,
            ImagePng::Unavailable => {
                have.insert(image_id, (image.generation, false));
                continue;
            }
        };
        retry = sooner(retry, wait);
        // Until the new pixels go out the client shows the ones it has.
        if let Some(old) = known.get(&image_id).filter(|(_, delivered)| *delivered) {
            have.insert(image_id, *old);
        }
    }
    let visible: Vec<Placement> = placements.iter().filter(|p| have.get(&p.image_id).is_some_and(|(_, ok)| *ok)).cloned().collect();
    if visible != visible_sent && !client.send(ServiceMsg::Placements { term: id.to_owned(), placements: visible.clone() }) {
        return ImagesSent::Full;
    }
    if let Some(sub) = lock(&client.subs).get_mut(id) {
        sub.images = have;
        sub.placements = placements;
        sub.visible = visible;
        sub.images_due = retry;
    }
    retry.map_or(ImagesSent::Done, ImagesSent::Later)
}

/// Keeps the session you type in responsive on a busy machine
/// (`crate::priority`): its agent and the agent's console host run above
/// normal, sessions shown on screen at normal, all others below normal; the
/// tools and builds an agent starts never run above normal. Background
/// sessions still use all spare CPU; they just yield to you. Only processes
/// inside our own sessions (and their console hosts) are touched.
/// `ATELIER_NO_PRIORITY=1` turns it off. Unix forbids raising priorities
/// without privilege, so there the others are lowered instead
/// (`canvas_sys::process::set_priority`: `PRIO_DARWIN_BG` on macOS, nice on
/// Linux; MAC-LINUX-PLAN §5.3).
fn priority_loop(host: Arc<Host>, wake: Receiver<()>) {
    use crate::priority::{Session, classify, reapply};
    use canvas_sys::process::Class;
    if canvas_common::env::var_os("ATELIER_NO_PRIORITY").is_some() {
        return;
    }
    let mut applied: HashMap<u32, Class> = HashMap::new();
    loop {
        // Re-check every 2 s so new child processes (tools, builds) follow
        // their session and a lapsed focus is lowered.
        let _ = wake.recv_timeout(Duration::from_secs(2));
        if host.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let terms: Vec<Arc<Terminal>> = lock(&host.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).cloned().collect();
        if terms.is_empty() {
            applied.clear();
            continue;
        }
        let active = lock(&host.active_input).current(Instant::now()).map(str::to_owned);
        let visible = host.visible();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut procs = crate::priority::Procs::new();
        for (pid, parent, name) in canvas_sys::process::process_entries() {
            children.entry(parent).or_default().push(pid);
            procs.insert(pid, (parent, name));
        }
        let sessions: Vec<Session> = terms
            .iter()
            .map(|t| Session {
                root: t.pid,
                // Still the same process (its pid not reused).
                console_host: t.console_host.filter(|(pid, started)| canvas_sys::process::process_start_ms(*pid) == Some(*started)).map(|(pid, _)| pid),
                class: if active.as_deref() == Some(t.id.as_str()) {
                    Class::Focused
                } else if visible.contains(&t.id) {
                    Class::Visible
                } else {
                    Class::Background
                },
            })
            .collect();
        let classes = classify(&sessions, &procs, |root| canvas_sys::process::process_tree(root, &children));
        reapply(&mut applied, &classes, canvas_sys::process::set_priority);
    }
}

/// Remembers every live terminal's descendants (pid and start time) every
/// 2 s, in memory for `force_kill` and in `<home>/run/terminals-<instance>.json`
/// for the next service: a service that died cannot end processes that
/// re-parented away from their terminal's session (an MCP server that called
/// `setsid`; macOS has no subreaper), so its successor does at start
/// ([`sweep_orphans`]). Unix only (Windows uses a kill-on-close job).
#[cfg(unix)]
fn remember_loop(host: Arc<Host>) {
    let path = remembered_path(&host.instance);
    if let Some(dir) = path.parent() {
        let _ = canvas_sys::fs::private_dir(dir);
    }
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if host.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let terms: Vec<Arc<Terminal>> = lock(&host.terms).values().filter(|t| t.alive.load(Ordering::Acquire) && t.pid != 0).cloned().collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, parent) in canvas_sys::process::process_pairs() {
            children.entry(parent).or_default().push(pid);
        }
        let mut all: Vec<(u32, u64)> = Vec::new();
        for terminal in &terms {
            let mut remembered = lock(&terminal.remembered);
            // Forget processes that ended (or whose pid was reused).
            remembered.retain(|(pid, started)| canvas_sys::process::process_start_ms(*pid) == Some(*started));
            for pid in canvas_sys::process::process_tree(terminal.pid, &children) {
                if let Some(started) = canvas_sys::process::process_start_ms(pid)
                    && !remembered.iter().any(|(p, _)| *p == pid)
                {
                    remembered.push((pid, started));
                }
            }
            // The tree walk only finds what is still below the terminal: keep
            // what moved away (still the same process) too.
            all.extend(remembered.iter().copied());
        }
        let text = remembered_text(&all);
        let _ = canvas_sys::fs::write_private(&path, text.as_bytes());
    }
}

#[cfg(unix)]
fn remembered_path(instance: &Instance) -> std::path::PathBuf {
    instance.home.join("run").join(format!("terminals-{}.txt", instance.name))
}

/// `pid start_ms` per line.
#[cfg(unix)]
fn remembered_text(processes: &[(u32, u64)]) -> String {
    processes.iter().map(|(pid, started)| format!("{pid} {started}\n")).collect()
}

/// Ends the processes a previous service of this instance remembered for its
/// terminals and that still run (same pid and start time): that service died
/// without stopping them. Returns how many were ended.
#[cfg(unix)]
pub fn sweep_orphans(instance: &Instance) -> usize {
    let path = remembered_path(instance);
    let Ok(text) = std::fs::read_to_string(&path) else { return 0 };
    let mut ended = 0;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(pid), Some(started)) = (words.next().and_then(|w| w.parse::<u32>().ok()), words.next().and_then(|w| w.parse::<u64>().ok())) else { continue };
        if pid != std::process::id() && canvas_sys::process::process_start_ms(pid) == Some(started) && canvas_sys::process::kill_tree(pid).is_ok_and(|n| n > 0) {
            ended += 1;
        }
    }
    let _ = std::fs::remove_file(&path);
    ended
}

fn exit_loop(host: Arc<Host>, exited: Receiver<String>) {
    for id in exited {
        // Not holding `terms` while broadcasting (lock order: clients before terms).
        let terminal = lock(&host.terms).get(&id).cloned();
        if let Some(terminal) = terminal {
            let (code, raw) = (*lock(&terminal.exit_code), *lock(&terminal.raw_exit_code));
            let note = match (raw != code, terminal.was_forced()) {
                (_, true) if terminal.ui_quit_lingered() => format!(" (stopped; its UI quit on request, process code {} ended 3 s later)", raw.unwrap_or_default()),
                (_, true) => format!(" (stopped; process code {}, killed after the grace period)", raw.unwrap_or_default()),
                (true, false) => format!(" (stopped; process code {})", raw.unwrap_or_default()),
                (false, false) => String::new(),
            };
            log(&host.instance, &format!("terminal {id} exited with {code:?}{note}"));
            // A relaunch meanwhile told every client about the new run already:
            // the end of this one must not overwrite it (checked and queued
            // under the client list's lock: no relaunch falls in between).
            host.tell_terms(|terms| match terms.get(&id).is_some_and(|t| Arc::ptr_eq(t, &terminal)) {
                true => ((), vec![ServiceMsg::Terminals(vec![terminal.info()])]),
                false => ((), Vec::new()),
            });
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
        host.tell_terms(|terms| {
            let infos: Vec<crate::protocol::TermInfo> = ids.iter().filter_map(|id| terms.get(id).map(|t| t.info())).collect();
            ((), if infos.is_empty() { Vec::new() } else { vec![ServiceMsg::Terminals(infos)] })
        });
    }
}

/// Who gets a terminal's OSC 52 copy: the client that last sent input to that
/// terminal while it is connected, else the client that last sent any input.
/// In remote mode that is the machine you typed on (its clipboard, PLAN.md §6).
fn clipboard_target(per_terminal: Option<u64>, last: u64, connected: impl Fn(u64) -> bool) -> Option<u64> {
    per_terminal.filter(|c| connected(*c)).or_else(|| connected(last).then_some(last))
}

fn clipboard_loop(host: Arc<Host>, clipboard: Receiver<(String, String)>) {
    for (terminal, text) in clipboard {
        let per_terminal = lock(&host.input_clients).get(&terminal).copied();
        let last = host.last_input_client.load(Ordering::Relaxed);
        let clients = lock(&host.clients);
        let client = clipboard_target(per_terminal, last, |id| clients.contains_key(&id)).and_then(|id| clients.get(&id).cloned());
        drop(clients);
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

    fn client(protocol: u32, caps: u32) -> (Client, Arc<Outbox>) {
        let outbox = Arc::new(Outbox::new());
        (Client::new(1, protocol, false, caps, outbox.clone(), Closer::new(|| {})), outbox)
    }

    /// The ordinary messages a test client was sent so far, in the order its writer takes them.
    fn drain(outbox: &Outbox) -> Vec<ServiceMsg> {
        let mut out = Vec::new();
        while let Next::Item(Taken { message, frame_of, .. }) = outbox.next(Instant::now()) {
            if let Some(tile) = frame_of {
                outbox.frame_written(&tile);
            }
            if let Outgoing::Message(message) = message {
                out.push(message);
            }
        }
        out
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
        let received: Vec<ServiceMsg> = drain(&rx);
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
        let received = drain(&rx);
        assert!(matches!(received[0], ServiceMsg::Frame8(_)));
        assert!(matches!(received[1], ServiceMsg::Heartbeat { input_ack: 3 }));
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
    fn a_row_that_starts_or_stops_wrapping_is_sent_again() {
        let mut sub = Sub::default();
        frame_of(&mut sub, snapshot(&["0123456789", "x"], None)).unwrap();
        let mut wrapped = snapshot(&["0123456789", "x"], None);
        wrapped.lines[0].push(Run8::soft_wrap(10));
        let frame = frame_of(&mut sub, wrapped.clone()).expect("the wrap state changed: a frame");
        assert_eq!(frame.changed, vec![(0, wrapped.lines[0].clone())]);
        assert!(frame_of(&mut sub, wrapped).is_none());
        let frame = frame_of(&mut sub, snapshot(&["0123456789", "x"], None)).expect("unwrapped again (a resize reflowed it)");
        assert_eq!(frame.changed.iter().map(|(y, _)| *y).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn osc52_copies_go_to_the_client_that_typed_into_the_terminal() {
        let connected = |ids: &'static [u64]| move |id: u64| ids.contains(&id);
        // Home typed into this terminal; the Cloud PC's window typed last elsewhere.
        assert_eq!(clipboard_target(Some(2), 1, connected(&[1, 2])), Some(2));
        assert_eq!(clipboard_target(Some(2), 1, connected(&[1])), Some(1), "that client is gone: the last typist");
        assert_eq!(clipboard_target(None, 1, connected(&[1, 2])), Some(1));
        assert_eq!(clipboard_target(None, 3, connected(&[1, 2])), None, "nobody connected typed");
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

    /// A write end that appends to a shared buffer.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn wedge_signs_name_what_is_stuck_and_nothing_else() {
        let now = 1_000_000;
        let term = |id: &str, alive: bool, started_ms: u64, last_output_ms: u64| TermInfo { id: id.into(), alive, started_ms, last_output_ms, pid: 7, ..Default::default() };
        let terms = [term("quiet", true, now - 40_000, 0), term("young", true, now - 5_000, 0), term("talks", true, now - 40_000, now - 39_000), term("gone", false, now - 90_000, 0)];
        let fresh = Health {
            pid: 1,
            updated_ms: now - 1_000,
            pending_closes: vec![(26, now - 20_000), (27, now - 1_000)],
            launching: vec![("slow".into(), now - 45_000), ("now".into(), now - 2_000)],
            working: vec![(31, now - 40_000), (32, now - 3_000)],
        };
        let signs = wedge_signs(Some(&fresh), &terms, now);
        assert_eq!(signs.len(), 4, "{signs:?}");
        assert!(signs[0].starts_with("client 26 was closed 20 s ago"), "{signs:?}");
        assert!(signs[1].starts_with("terminal slow has been launching for 45 s"), "{signs:?}");
        assert!(signs[2].starts_with("a message of client 31 has been handled for 40 s"), "{signs:?}");
        assert!(signs[3].starts_with("terminal quiet (pid 7) printed nothing in the 40 s"), "{signs:?}");
        let stale = Health { updated_ms: now - 60_000, ..Health::default() };
        assert!(wedge_signs(Some(&stale), &[], now)[0].contains("last written 60 s ago"));
        assert!(wedge_signs(None, &terms[1..], now).is_empty(), "an older service without a health file: only the terminals count");
    }

    #[test]
    fn the_image_budget_counts_queued_bytes() {
        let (client, _outbox) = client(8, CAP_IMAGES);
        assert!(client.image_fits(100 << 20), "one image always fits an empty queue");
        let png: Arc<[u8]> = vec![0u8; IMAGE_BUDGET - (1 << 20)].into();
        assert_eq!(client.send_image("t", 1, &EncodedImage { generation: 1, width: 1, height: 1, png }), Push::Queued);
        assert!(client.image_fits(1 << 20));
        assert!(!client.image_fits((1 << 20) + 1), "over the budget: it waits");
    }

    #[test]
    fn queued_images_are_shared_and_written_as_image_messages() {
        let sink = Shared::default();
        let outbox = Arc::new(Outbox::new());
        let client = Arc::new(Client::new(1, 8, true, CAP_IMAGES | protocol::CAP_FRAME8, outbox.clone(), Closer::new(|| {})));
        let png: Arc<[u8]> = vec![7u8; 3 << 20].into();
        let image = EncodedImage { generation: 4, width: 2, height: 3, png: png.clone() };
        assert_eq!(client.send_image("t", 5, &image), Push::Queued);
        assert_eq!(client.send_image("t", 6, &image), Push::Queued);
        assert_eq!(outbox.image_bytes(), 2 * png.len());
        assert_eq!(Arc::strong_count(&png), 4, "queued twice without a copy");
        spawn_writer(std::sync::Weak::new(), FrameWriter::new(Box::new(sink.clone())), outbox.clone(), &client, None).unwrap();
        let started = Instant::now();
        while outbox.image_bytes() != 0 {
            assert!(started.elapsed() < Duration::from_secs(5), "the writer took the images off the budget");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        let bytes = lock(&sink.0).clone();
        let mut reader = FrameReader::new(Box::new(std::io::Cursor::new(bytes)));
        let mut ids = Vec::new();
        while let Ok(Some(message)) = reader.recv::<ServiceMsg>() {
            if let ServiceMsg::Image { image_id, width, height, rgba_or_png, format, .. } = message {
                assert_eq!((width, height, format, rgba_or_png.len()), (2, 3, protocol::IMAGE_PNG, 3 << 20));
                ids.push(image_id);
            }
        }
        assert_eq!(ids, [5, 6]);
    }

    /// A backend with nothing but the host.
    struct NoApp;

    impl HostApp for NoApp {
        fn resolve_launch(&self, _: &str, _: LaunchKind, _: &str) -> Result<Resolved> {
            bail!("no agents here")
        }
        fn client_connected(&self, _: &Arc<Client>) {}
        fn client_disconnected(&self, _: u64) {}
        fn message(&self, _: &Arc<Client>, _: ClientMsg) -> Result<()> {
            Ok(())
        }
        fn input_noted(&self, _: &str) {}
        fn terminals_changed(&self) {}
        fn has_work(&self) -> bool {
            false
        }
        fn remote_credential(&self, _: &str) -> Credential {
            Credential::Refused
        }
    }

    /// Whether the service closed `stream` within `within` (end of stream or a reset).
    fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
        // macOS refuses socket options on a connection the peer already reset (EINVAL): closed.
        if stream.set_read_timeout(Some(within)).is_err() {
            return true;
        }
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => !matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock),
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_new_service_ends_what_a_dead_one_remembered() {
        use std::os::unix::process::CommandExt;
        let home = std::env::temp_dir().join(format!("cc-sweep-{}", uuid_like()));
        let instance = Instance::new(Some("sweep".into()), Some(home.clone()), Some(home.join("sessions"))).unwrap();
        // A process in a session of its own (as an MCP server that called setsid).
        let mut orphan = std::process::Command::new("/bin/sleep").arg("30").process_group(0).spawn().unwrap();
        let started = canvas_sys::process::process_start_ms(orphan.id()).unwrap();
        let path = remembered_path(&instance);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A recycled pid (another start time) is never touched.
        std::fs::write(&path, format!("{} {started}\n{} 1\n", orphan.id(), std::process::id())).unwrap();
        assert_eq!(sweep_orphans(&instance), 1);
        orphan.wait().unwrap();
        assert!(!path.exists(), "the list is used once");
        assert_eq!(sweep_orphans(&instance), 0);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    fn uuid_like() -> String {
        format!("{}-{}", std::process::id(), now_ms())
    }

    #[test]
    fn a_flood_of_unauthenticated_connections_neither_stops_the_service_nor_locks_out_clients() {
        let home = std::env::temp_dir().join(format!("cc-flood-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"remote":{"listen":"127.0.0.1:0"}}"#).unwrap();
        let instance = Instance::new(Some("flood".into()), Some(home.clone()), Some(home.clone())).unwrap();
        let service = instance.clone();
        std::thread::spawn(move || run(service, |_| Arc::new(NoApp)));
        let started = Instant::now();
        let port = loop {
            if let Some(port) = std::fs::read_to_string(instance.port_path()).ok().and_then(|p| p.trim().parse::<u16>().ok()) {
                break port;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the service listens");
            std::thread::sleep(Duration::from_millis(50));
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // Bodies claimed far above the pre-authentication limit are refused at once.
        let mut huge: Vec<TcpStream> = (0..64)
            .map(|_| {
                let mut stream = TcpStream::connect(addr).unwrap();
                let _ = stream.write_all(&0x7fff_fff0u32.to_le_bytes());
                stream
            })
            .collect();
        // Silent peers: a few wait for their handshake; the surplus is closed at once.
        let mut silent: Vec<TcpStream> = (0..48).map(|_| TcpStream::connect(addr).unwrap()).collect();
        assert!(huge.iter_mut().all(|s| closed_within(s, Duration::from_secs(3))), "oversized frames end their connections");
        let waiting: Vec<TcpStream> = silent.drain(..).filter_map(|mut s| (!closed_within(&mut s, Duration::from_millis(300))).then_some(s)).collect();
        assert!(waiting.len() <= MAX_UNAUTHENTICATED, "{} connections were allowed to authenticate at once", waiting.len());
        // The handshake deadline frees the rest without a thread per connection.
        for mut stream in waiting {
            assert!(closed_within(&mut stream, HANDSHAKE_TIMEOUT + Duration::from_secs(2)), "the handshake deadline closes it");
        }
        // The service still serves a legitimate client: the token, from this user.
        let token = canvas_sys::token::read_token(&instance.token_path()).unwrap();
        let (reader, writer, closer) = tcp::split(TcpStream::connect(addr).unwrap()).unwrap();
        let (mut reader, mut writer) = (FrameReader::new(Box::new(reader)), FrameWriter::new(Box::new(writer)));
        let info = protocol::ClientInfo { name: "test".into(), caps: protocol::CAP_FRAME8, session: vec![3; 16], remote_id: None };
        let greeting = handshake::open(&mut reader, &mut writer, &token, "test", Some(info)).expect("a legitimate client connects");
        assert_eq!(greeting.build, build_id(), "the build is told after authentication");
        assert!(matches!(reader.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Ready8 { .. })));
        closer.close();
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert!(log.contains("frame too large") || log.contains("closing new ones"), "{log}");
    }

    /// A backend that knows one paired client, "phone".
    struct PhoneApp(Vec<u8>);

    impl HostApp for PhoneApp {
        fn resolve_launch(&self, _: &str, _: LaunchKind, _: &str) -> Result<Resolved> {
            bail!("no agents here")
        }
        fn client_connected(&self, _: &Arc<Client>) {}
        fn client_disconnected(&self, _: u64) {}
        fn message(&self, _: &Arc<Client>, _: ClientMsg) -> Result<()> {
            Ok(())
        }
        fn input_noted(&self, _: &str) {}
        fn terminals_changed(&self) {}
        fn has_work(&self) -> bool {
            false
        }
        fn remote_credential(&self, id: &str) -> Credential {
            if id == "phone" { Credential::Secret(self.0.clone()) } else { Credential::Refused }
        }
    }

    /// A WebSocket to the service's remote port, upgraded.
    fn websocket(addr: SocketAddr) -> (FrameReader, FrameWriter, TcpStream) {
        let stream = TcpStream::connect(addr).unwrap();
        let mut reader: Box<dyn Read + Send> = Box::new(stream.try_clone().unwrap());
        let mut writer: Box<dyn Write + Send> = Box::new(stream.try_clone().unwrap());
        ws::connect(&mut reader, &mut writer, "atelier-47600.usw2.devtunnels.ms", "/", &[("X-Tunnel-Authorization", "tunnel x")]).unwrap();
        let (reader, writer) = ws::split(ws::Role::Client, reader, writer);
        (FrameReader::new(Box::new(reader)), FrameWriter::new(Box::new(writer)), stream)
    }

    #[test]
    fn websocket_clients_on_the_remote_port_must_be_paired() {
        let home = std::env::temp_dir().join(format!("cc-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"remote":{"listen":"127.0.0.1:0"}}"#).unwrap();
        let instance = Instance::new(Some("ws".into()), Some(home.clone()), Some(home.clone())).unwrap();
        let secret = vec![9u8; 32];
        let service = instance.clone();
        let app_secret = secret.clone();
        std::thread::spawn(move || run(service, move |_| Arc::new(PhoneApp(app_secret))));
        let started = Instant::now();
        let port = loop {
            if let Some(port) = std::fs::read_to_string(instance.port_path()).ok().and_then(|p| p.trim().parse::<u16>().ok()) {
                break port;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the service listens");
            std::thread::sleep(Duration::from_millis(50));
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // The service token is refused through a WebSocket, even from this user.
        let token = canvas_sys::token::read_token(&instance.token_path()).unwrap();
        let (mut reader, mut writer, _s) = websocket(addr);
        let info = protocol::ClientInfo { name: "iphone".into(), caps: protocol::CAP_FRAME8 | CAP_COMPRESS, session: vec![4; 16], remote_id: None };
        assert!(handshake::open(&mut reader, &mut writer, &token, "test", Some(info.clone())).is_err(), "no token over web forwarding");
        // A paired client is served exactly like over TCP, compression included.
        let (mut reader, mut writer, _s) = websocket(addr);
        let phone = protocol::ClientInfo { remote_id: Some("phone".into()), ..info };
        let greeting = handshake::open(&mut reader, &mut writer, &secret, "test", Some(phone)).expect("the paired phone connects");
        assert_eq!(greeting.build, build_id());
        assert!(matches!(reader.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Ready8 { .. })));
        reader.accept_compressed().unwrap();
        writer.compress().unwrap();
        // A paired remote client cannot end the service.
        writer.send(&ClientMsg::Shutdown).unwrap();
        let refused = loop {
            match reader.recv::<ServiceMsg>().unwrap() {
                Some(ServiceMsg::Error { message, .. }) => break message,
                Some(_) => continue,
                None => panic!("the service closed the connection"),
            }
        };
        assert!(refused.contains("cannot shut the service down"), "{refused}");
        writer.send(&ClientMsg::Heartbeat).unwrap();
        assert!(!host_shutting_down(&instance), "still running");
        // A plain TCP client is still served on the same port.
        let (r, w, closer) = tcp::split(TcpStream::connect(addr).unwrap()).unwrap();
        let (mut r, mut w) = (FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w)));
        let plain = protocol::ClientInfo { name: "tui".into(), caps: protocol::CAP_FRAME8, session: vec![5; 16], remote_id: None };
        handshake::open(&mut r, &mut w, &token, "test", Some(plain)).expect("the token over plain TCP, from this user");
        closer.close();
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert!(log.contains("WebSocket) as paired client \"phone\""), "{log}");
    }

    /// Whether the test service logged its shutdown.
    fn host_shutting_down(instance: &Instance) -> bool {
        std::fs::read_to_string(instance.log_path()).unwrap_or_default().contains("shutdown requested")
    }

    /// A service of its own (TCP on a loopback port) with the backend `make_app` builds.
    fn test_service(name: &str, make_app: impl FnOnce(&Arc<Host>) -> Arc<dyn HostApp> + Send + 'static) -> (Instance, SocketAddr) {
        let home = std::env::temp_dir().join(format!("cc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"remote":{"listen":"127.0.0.1:0"}}"#).unwrap();
        let instance = Instance::new(Some(name.into()), Some(home.clone()), Some(home.clone())).unwrap();
        let service = instance.clone();
        std::thread::spawn(move || run(service, make_app));
        let started = Instant::now();
        let port = loop {
            if let Some(port) = std::fs::read_to_string(instance.port_path()).ok().and_then(|p| p.trim().parse::<u16>().ok()) {
                break port;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the service listens");
            std::thread::sleep(Duration::from_millis(50));
        };
        (instance, SocketAddr::from(([127, 0, 0, 1], port)))
    }

    /// A protocol-8 client over TCP: what it receives arrives on the channel.
    fn test_client(instance: &Instance, addr: SocketAddr, name: &str) -> (Receiver<ServiceMsg>, FrameWriter, tcp::TcpCloser) {
        let (rx, writer, closer) = test_client_with(instance, addr, name, protocol::CAP_FRAME8);
        let (tx, plain) = unbounded();
        std::thread::spawn(move || {
            for (message, _) in rx {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        (plain, writer, closer)
    }

    /// [`test_client`] with `caps`; each message comes with the bytes read up to its end.
    fn test_client_with(instance: &Instance, addr: SocketAddr, name: &str, caps: u32) -> (Receiver<(ServiceMsg, u64)>, FrameWriter, tcp::TcpCloser) {
        let token = canvas_sys::token::read_token(&instance.token_path()).unwrap();
        let (reader, writer, closer) = tcp::split(TcpStream::connect(addr).unwrap()).unwrap();
        let stats = Arc::new(canvas_common::wire::WireStats::default());
        let (mut reader, mut writer) = (FrameReader::new(Box::new(reader)).with_stats(stats.clone()), FrameWriter::new(Box::new(writer)));
        let session: Vec<u8> = name.bytes().chain(std::iter::repeat(0)).take(16).collect();
        let info = protocol::ClientInfo { name: name.into(), caps, session, remote_id: None };
        handshake::open(&mut reader, &mut writer, &token, "test", Some(info)).unwrap();
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            while let Ok(Some(message)) = reader.recv::<ServiceMsg>() {
                if tx.send((message, stats.bytes_in.load(Ordering::Relaxed))).is_err() {
                    break;
                }
            }
        });
        (rx, writer, closer)
    }

    /// Waits for a message that `want` accepts.
    fn expect(rx: &Receiver<ServiceMsg>, within: Duration, what: &str, mut want: impl FnMut(&ServiceMsg) -> bool) -> ServiceMsg {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("{what}: nothing within {within:?}"));
            match rx.recv_timeout(left) {
                Ok(message) if want(&message) => return message,
                Ok(_) => {}
                Err(error) => panic!("{what}: {error}"),
            }
        }
    }

    fn lists_alive(message: &ServiceMsg, id: &str) -> bool {
        matches!(message, ServiceMsg::Terminals(infos) if infos.iter().any(|i| i.id == id && i.alive))
    }

    /// A program that waits a while in a terminal.
    fn waiting_program() -> LaunchKind {
        #[cfg(windows)]
        return LaunchKind::Command { program: "cmd.exe".into(), args: vec!["/c".into(), "ping -n 60 127.0.0.1 >nul".into()] };
        #[cfg(not(windows))]
        LaunchKind::Command { program: "/bin/sh".into(), args: vec!["-c".into(), "sleep 60".into()] }
    }

    /// A backend that launches a terminal from another thread while a client
    /// connects (the restore after a restart, another window), takes long
    /// for `Track`, and notes the messages it got in order.
    struct BusyApp {
        host: OnceLock<Weak<Host>>,
        launch_on_connect: Mutex<Option<String>>,
        slow: Duration,
        seen: Mutex<Vec<String>>,
    }

    impl HostApp for BusyApp {
        fn resolve_launch(&self, _: &str, _: LaunchKind, _: &str) -> Result<Resolved> {
            bail!("no agents here")
        }
        fn client_connected(&self, _: &Arc<Client>) {
            let Some(id) = lock(&self.launch_on_connect).take() else { return };
            let host = self.host.get().and_then(Weak::upgrade).unwrap();
            let cwd = std::env::temp_dir().to_string_lossy().into_owned();
            std::thread::spawn(move || host.launch(&id, waiting_program(), &cwd, 80, 24)).join().unwrap().unwrap();
        }
        fn client_disconnected(&self, _: u64) {}
        fn message(&self, _: &Arc<Client>, message: ClientMsg) -> Result<()> {
            if matches!(message, ClientMsg::Track { .. }) {
                std::thread::sleep(self.slow);
            }
            lock(&self.seen).push(format!("{message:?}").split([' ', '{', '(']).next().unwrap_or_default().to_owned());
            Ok(())
        }
        fn input_noted(&self, _: &str) {}
        fn terminals_changed(&self) {}
        fn has_work(&self) -> bool {
            false
        }
        fn remote_credential(&self, _: &str) -> Credential {
            Credential::Refused
        }
    }

    fn busy_app(launch_on_connect: Option<&str>, slow: Duration) -> Arc<BusyApp> {
        Arc::new(BusyApp { host: OnceLock::new(), launch_on_connect: Mutex::new(launch_on_connect.map(str::to_owned)), slow, seen: Mutex::default() })
    }

    #[test]
    fn a_launch_while_a_client_connects_reaches_it_and_a_launch_of_a_running_terminal_answers() {
        let app = busy_app(Some("t1"), Duration::ZERO);
        let (host_tx, host_rx) = bounded::<Arc<Host>>(1);
        let made = app.clone();
        let (instance, addr) = test_service("gap", move |host| {
            let _ = made.host.set(Arc::downgrade(host));
            let _ = host_tx.send(host.clone());
            made
        });
        let host = host_rx.recv().unwrap();
        // The launch completes while the client is being connected (after its terminal list was taken).
        let (rx, mut writer, closer) = test_client(&instance, addr, "gap-client");
        expect(&rx, Duration::from_secs(10), "the terminal launched while it connected", |m| lists_alive(m, "t1"));
        // A launch of the running terminal starts nothing and tells the client it runs.
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        writer.send(&ClientMsg::Launch { id: "t1".into(), kind: waiting_program(), cwd, cols: 80, rows: 24 }).unwrap();
        expect(&rx, Duration::from_secs(5), "the answer to a launch of a running terminal", |m| lists_alive(m, "t1"));
        // A full refresh lists every terminal.
        writer.send(&ClientMsg::RefreshSessions).unwrap();
        expect(&rx, Duration::from_secs(5), "the terminal list a refresh sends", |m| lists_alive(m, "t1"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&app.seen).iter().any(|m| m == "RefreshSessions") {
            assert!(Instant::now() < deadline, "the backend got the refresh too");
            std::thread::sleep(Duration::from_millis(20));
        }
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert_eq!(log.matches("launched t1 ").count(), 1, "{log}");
        // A client dropped before it was listed (its writer failed at once) is never listed.
        let gone = Arc::new(Client::new(9999, 8, true, protocol::CAP_FRAME8, Arc::new(Outbox::new()), Closer::new(|| {})));
        gone.dropped.store(true, Ordering::Release);
        host.register(&gone);
        assert!(!lock(&host.clients).contains_key(&9999), "a dropped client is not listed");
        closer.close();
        let _ = host.terminal("t1").map(|t| t.stop(true));
    }

    #[test]
    fn slow_messages_run_in_order_beside_the_reader_and_never_get_a_client_dropped() {
        let app = busy_app(None, Duration::from_secs(7));
        let made = app.clone();
        let (instance, addr) = test_service("worker", move |host| {
            let _ = made.host.set(Arc::downgrade(host));
            made
        });
        let (rx, mut writer, closer) = test_client(&instance, addr, "worker-client");
        writer.send(&ClientMsg::Track { ids: Vec::new() }).unwrap();
        writer.send(&ClientMsg::RefreshSessions).unwrap();
        // Quick messages are not held behind the slow one: input to an unknown terminal is refused at once.
        std::thread::sleep(Duration::from_millis(300));
        let typed = Instant::now();
        writer.send(&ClientMsg::Input8 { id: "missing".into(), input: Input::Key { key: protocol::Key::Char('x'), mods: 0 }, seq: 1 }).unwrap();
        expect(&rx, Duration::from_secs(2), "the refusal of input to an unknown terminal", |m| matches!(m, ServiceMsg::Error { message, .. } if message.contains("missing")));
        assert!(typed.elapsed() < Duration::from_secs(1), "input waited {:?} behind a slow message", typed.elapsed());
        // The client keeps heartbeating; the slow message outlasts the silence timeout.
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            writer.send(&ClientMsg::Heartbeat).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            while let Ok(message) = rx.try_recv() {
                let _ = message;
            }
        }
        assert_eq!(*lock(&app.seen), ["Track", "RefreshSessions"], "in the order sent");
        writer.send(&ClientMsg::Heartbeat).unwrap();
        expect(&rx, Duration::from_secs(3), "a heartbeat: still connected", |m| matches!(m, ServiceMsg::Heartbeat { .. }));
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert!(!log.contains("silent for"), "{log}");
        closer.close();
    }

    /// A backend whose launches take `resolve` to resolve (then run a program
    /// that waits), whose connect snapshot takes `connect`, and which answers
    /// every request at once.
    struct SlowApp {
        resolve: Duration,
        connect: Duration,
    }

    impl HostApp for SlowApp {
        fn resolve_launch(&self, _: &str, _: LaunchKind, _: &str) -> Result<Resolved> {
            std::thread::sleep(self.resolve);
            let LaunchKind::Command { program, args } = waiting_program() else { unreachable!() };
            Ok(Resolved { program, args, env: Vec::new(), agent: false, remote: None })
        }
        fn client_connected(&self, _: &Arc<Client>) {
            std::thread::sleep(self.connect);
        }
        fn client_disconnected(&self, _: u64) {}
        fn message(&self, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
            if let ClientMsg::Rpc { id, .. } = message {
                client.send(ServiceMsg::RpcReply { id, json: "\"Done\"".into() });
            }
            Ok(())
        }
        fn input_noted(&self, _: &str) {}
        fn terminals_changed(&self) {}
        fn has_work(&self) -> bool {
            false
        }
        fn remote_credential(&self, _: &str) -> Credential {
            Credential::Refused
        }
    }

    /// A service with a [`SlowApp`], and its host.
    fn slow_service(name: &str, resolve: Duration, connect: Duration) -> (Instance, SocketAddr, Arc<Host>) {
        let (host_tx, host_rx) = bounded::<Arc<Host>>(1);
        let (instance, addr) = test_service(name, move |host| {
            let _ = host_tx.send(host.clone());
            Arc::new(SlowApp { resolve, connect })
        });
        (instance, addr, host_rx.recv().unwrap())
    }

    fn agent_launch(id: &str, cols: u16, rows: u16) -> ClientMsg {
        ClientMsg::Launch { id: id.into(), kind: LaunchKind::Copilot { new_name: None, prompt: None, resume: false }, cwd: std::env::temp_dir().to_string_lossy().into_owned(), cols, rows }
    }

    /// Waits until `check` holds (polling), or fails with `what`.
    fn eventually(within: Duration, what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !check() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn errors(rx: &Receiver<ServiceMsg>) -> Vec<String> {
        rx.try_iter().filter_map(|m| if let ServiceMsg::Error { message, .. } = m { Some(message) } else { None }).collect()
    }

    #[test]
    fn launches_run_side_by_side_and_each_terminals_work_stays_behind_its_launch() {
        let (instance, addr, host) = slow_service("parallel", Duration::from_millis(1500), Duration::ZERO);
        let (rx, mut writer, closer) = test_client(&instance, addr, "parallel-client");
        let started = Instant::now();
        writer.send(&agent_launch("a", 80, 24)).unwrap();
        writer.send(&ClientMsg::Resize { id: "a".into(), cols: 100, rows: 30 }).unwrap();
        writer.send(&agent_launch("b", 80, 24)).unwrap();
        writer.send(&ClientMsg::Resize { id: "b".into(), cols: 90, rows: 20 }).unwrap();
        // A request is not held behind the launches (R2-A-04).
        writer.send(&ClientMsg::Rpc { id: 7, json: "\"Info\"".into() }).unwrap();
        expect(&rx, Duration::from_millis(500), "the reply while two launches run", |m| matches!(m, ServiceMsg::RpcReply { id: 7, .. }));
        assert!(started.elapsed() < Duration::from_millis(700), "the reply waited {:?}", started.elapsed());
        eventually(Duration::from_secs(10), "both terminals start", || host.terminal("a").is_ok() && host.terminal("b").is_ok());
        let both = started.elapsed();
        assert!(both < Duration::from_millis(2800), "the launches ran one after the other ({both:?})");
        // Each resize was applied once its terminal existed: no "no terminal" error.
        eventually(Duration::from_secs(5), "the resizes held behind the launches", || host.terminal("a").unwrap().size() == (100, 30) && host.terminal("b").unwrap().size() == (90, 20));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(errors(&rx), Vec::<String>::new());
        closer.close();
        for id in ["a", "b"] {
            let _ = host.terminal(id).map(|t| t.stop(true));
        }
    }

    #[test]
    fn work_for_a_terminal_another_launch_is_starting_waits_for_it() {
        let (instance, addr, host) = slow_service("await", Duration::from_millis(1200), Duration::ZERO);
        let (rx, mut writer, closer) = test_client(&instance, addr, "await-client");
        // Another connection (or the restore after a restart) launches "c".
        let launcher = host.clone();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let other = std::thread::spawn(move || launcher.launch("c", LaunchKind::Copilot { new_name: None, prompt: None, resume: false }, &cwd, 80, 24));
        eventually(Duration::from_secs(2), "the other launch is under way", || host.is_launching("c"));
        writer.send(&ClientMsg::Resize { id: "c".into(), cols: 70, rows: 20 }).unwrap();
        writer.send(&ClientMsg::Claim { id: "c".into() }).unwrap();
        other.join().unwrap().unwrap();
        eventually(Duration::from_secs(5), "the resize waited for the launch", || host.terminal("c").is_ok_and(|t| t.size() == (70, 20)));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(errors(&rx), Vec::<String>::new(), "nothing refused while it started");
        closer.close();
        let _ = host.terminal("c").map(|t| t.stop(true));
    }

    #[test]
    fn a_tile_hidden_while_its_launch_runs_leaves_no_size_wish() {
        let (instance, addr, host) = slow_service("hidden", Duration::from_millis(1000), Duration::ZERO);
        let (_rx, mut writer, closer) = test_client(&instance, addr, "hidden-client");
        writer.send(&ClientMsg::Subscribe8 { ids: vec!["d".into()], focused: Some("d".into()) }).unwrap();
        writer.send(&agent_launch("d", 80, 24)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        writer.send(&ClientMsg::Subscribe8 { ids: Vec::new(), focused: None }).unwrap();
        eventually(Duration::from_secs(5), "the terminal starts", || host.terminal("d").is_ok());
        let id = *lock(&host.clients).keys().next().unwrap();
        // The launch recorded this client's wish; the hide that came before it takes it back after it (R2-A-07).
        eventually(Duration::from_secs(5), "the hidden tile's wish goes", || lock(&host.sizes).get("d").is_none_or(|p| p.owner() != Some(id)));
        closer.close();
        let _ = host.terminal("d").map(|t| t.stop(true));
    }

    #[test]
    fn a_slow_connect_snapshot_neither_holds_reading_nor_gets_the_client_dropped() {
        let (instance, addr, _host) = slow_service("connect", Duration::ZERO, Duration::from_secs(7));
        let (rx, mut writer, closer) = test_client(&instance, addr, "connect-client");
        let asked = Instant::now();
        writer.send(&ClientMsg::Stats).unwrap();
        expect(&rx, Duration::from_secs(2), "the stats while the snapshot is built", |m| matches!(m, ServiceMsg::Stats(_)));
        assert!(asked.elapsed() < Duration::from_secs(1), "{:?}", asked.elapsed());
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            writer.send(&ClientMsg::Heartbeat).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        }
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert!(!log.contains("silent for"), "{log}");
        closer.close();
    }

    #[test]
    fn a_reconnection_handles_what_its_predecessor_read_first() {
        let app = busy_app(None, Duration::from_secs(3));
        let made = app.clone();
        let (instance, addr) = test_service("drain", move |host| {
            let _ = made.host.set(Arc::downgrade(host));
            made
        });
        let (_rx, mut writer, _closer) = test_client(&instance, addr, "drain-client");
        writer.send(&ClientMsg::Track { ids: Vec::new() }).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        // The same client session reconnects while its old connection's slow message runs.
        let (_rx2, mut writer2, closer2) = test_client(&instance, addr, "drain-client");
        writer2.send(&ClientMsg::RefreshSessions).unwrap();
        // The old worker is draining: that is work under way, not a close that hangs (R2-A-06).
        std::thread::sleep(HEALTH_EVERY + Duration::from_millis(500));
        let health = read_health(&instance).expect("the health file");
        assert!(health.pending_closes.is_empty(), "{health:?}");
        assert!(!health.working.is_empty(), "{health:?}");
        eventually(Duration::from_secs(8), "both messages handled", || lock(&app.seen).len() == 2);
        assert_eq!(*lock(&app.seen), ["Track", "RefreshSessions"], "the reconnection's message after its predecessor's");
        closer2.close();
    }

    /// A backend that answers every `Rpc` at once with a reply of `.0` bytes (bulk).
    struct ReplyApp(usize);

    #[test]
    fn a_service_with_no_terminal_sends_an_empty_list_before_its_first_stats() {
        // A restarted service with nothing running yet: a reconnecting client
        // learns the terminals it knew are gone (R2-E-01).
        let (instance, addr) = test_service("empty", |_| Arc::new(NoApp));
        let (rx, mut writer, closer) = test_client(&instance, addr, "empty-client");
        writer.send(&ClientMsg::Stats).unwrap();
        let mut seen = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("the stats answer") {
                ServiceMsg::Stats(_) => break,
                ServiceMsg::Terminals(infos) => seen.push(format!("terminals {}", infos.len())),
                ServiceMsg::Heartbeat { .. } => {}
                other => seen.push(format!("{other:?}").split([' ', '{', '(']).next().unwrap_or_default().to_owned()),
            }
        }
        assert_eq!(seen, ["Ready8", "terminals 0"]);
        // A refresh answers with the empty list too.
        writer.send(&ClientMsg::RefreshSessions).unwrap();
        expect(&rx, Duration::from_secs(5), "the empty list a refresh sends", |m| matches!(m, ServiceMsg::Terminals(infos) if infos.is_empty()));
        closer.close();
    }

    impl HostApp for ReplyApp {
        fn resolve_launch(&self, _: &str, _: LaunchKind, _: &str) -> Result<Resolved> {
            bail!("no agents here")
        }
        fn client_connected(&self, _: &Arc<Client>) {}
        fn client_disconnected(&self, _: u64) {}
        fn message(&self, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
            if let ClientMsg::Rpc { id, .. } = message {
                client.send(ServiceMsg::RpcReply { id, json: "x".repeat(self.0) });
            }
            Ok(())
        }
        fn input_noted(&self, _: &str) {}
        fn terminals_changed(&self) {}
        fn has_work(&self) -> bool {
            false
        }
        fn remote_credential(&self, _: &str) -> Credential {
            Credential::Refused
        }
    }

    fn replies(rx: &Receiver<(ServiceMsg, u64)>, within: Duration) -> (usize, Option<u64>) {
        let (mut count, mut read) = (0, None);
        let until = Instant::now() + within;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok((message, bytes)) => {
                    read = Some(bytes);
                    count += matches!(message, ServiceMsg::RpcReply { .. }) as usize;
                }
                Err(_) => break,
            }
        }
        (count, read)
    }

    #[test]
    fn a_client_that_acknowledges_gets_bulk_within_its_window_and_older_clients_get_no_caps() {
        let (instance, addr) = test_service("flow", |_| Arc::new(ReplyApp(100 << 10)));
        let rpc = |id| ClientMsg::Rpc { id, json: "\"Info\"".into() };
        // An older client (no CAP_ACK): no `Caps`, and nothing waits for acknowledgements.
        let (rx, mut writer, closer) = test_client_with(&instance, addr, "older", protocol::CAP_FRAME8);
        for id in 0..8 {
            writer.send(&rpc(id)).unwrap();
        }
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while got.iter().filter(|m| matches!(m, ServiceMsg::RpcReply { .. })).count() < 8 {
            let left = until.checked_duration_since(Instant::now()).expect("an older client gets every reply unacknowledged");
            got.push(rx.recv_timeout(left).unwrap().0);
        }
        assert!(matches!(got[0], ServiceMsg::Ready8 { .. }));
        assert!(!got.iter().any(|m| matches!(m, ServiceMsg::Caps { .. })), "an older client never gets Caps");
        closer.close();
        // A client that acknowledges: `Caps` right after `Ready8`, then no more
        // bulk in flight than its window until it says what it read.
        let (rx, mut writer, closer) = test_client_with(&instance, addr, "acks", protocol::CAP_FRAME8 | CAP_ACK);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap().0, ServiceMsg::Ready8 { .. }));
        let (caps, base) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(caps, ServiceMsg::Caps { bits: SERVICE_ACKS }, "Caps follows Ready8");
        for id in 0..8 {
            writer.send(&rpc(id)).unwrap();
        }
        let (first, read) = replies(&rx, Duration::from_millis(800));
        assert!((1..=2).contains(&first), "{first} replies of 100 KB went unacknowledged (the window starts at 128 KB)");
        let mut total = first;
        let mut read = read.unwrap_or(base);
        let until = Instant::now() + Duration::from_secs(5);
        while total < 8 {
            assert!(Instant::now() < until, "acknowledgements let the rest go ({total} of 8)");
            writer.send(&ClientMsg::Received { bytes: read - base }).unwrap();
            let (more, now) = replies(&rx, Duration::from_millis(100));
            total += more;
            read = now.unwrap_or(read);
        }
        assert_eq!(total, 8);
        closer.close();
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        assert!(!log.contains("cannot read"), "Received is understood: {log}");
    }
}
