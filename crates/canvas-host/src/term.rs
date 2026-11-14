//! One hosted terminal: a ConPTY child, its emulator and its I/O threads.
//!
//! The emulator lives on a dedicated thread (engines need not be `Send`).
//! Clients and the frame pump reach it through a control channel (input,
//! clicks, wheel, scroll, snapshots); the PTY reader feeds it through
//! a separate output channel. The thread handles waiting control messages
//! before the next output chunk (one chunk still gets its turn after a long
//! burst), so keystrokes and snapshots never queue behind a flood of output.
//! The output channel is bounded in bytes
//! ([`OUTPUT_QUEUE`] chunks of at most [`READ_CHUNK`] bytes): when the
//! emulator falls behind, the reader blocks and ConPTY slows the program.
//!
//! A resize reaches the engine at its place in the output stream, not ahead
//! of output still queued: the engine takes the new size right after the
//! output the reader had taken from the PTY when the PTY was resized (drawn
//! for the old size), and before anything read later (ConPTY's repaint for
//! the new size). Fed at the old size, that repaint would scroll the top
//! rows away for good ([`Resizes`]).

use std::any::Any;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TrySendError, at, bounded, never, select_biased};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::emulator::frozen::Frozen;
use crate::emulator::inline::InlineImages;
use crate::emulator::{self, Config, Emulator, Engine, Events, ImageData, Scroll, Snapshot};
use crate::paths::now_ms;
use crate::protocol::{Input, Run, TermInfo};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Size of one PTY read; output chunks are never larger.
pub const READ_CHUNK: usize = 64 * 1024;
/// Output chunks waiting for the emulator (at most 4 MiB with [`READ_CHUNK`]).
pub const OUTPUT_QUEUE: usize = 64;
/// Control messages waiting for the emulator.
const CONTROL_QUEUE: usize = 1024;
/// Control messages handled before one waiting output chunk gets its turn.
const CONTROL_BURST: u32 = 64;
/// How long a request of the service's own (its typing, a stop's keys, an
/// image, a remote terminal's reset) waits for room in the control queue.
/// Clients' keys, clicks and scrolls never wait: a full queue refuses them
/// at once ([`Terminal::offer_input`]).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
/// How long an agent gets to quit on a stop request before it is killed ([`Terminal::stop`]).
pub const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long what an agent left attached to its terminal gets to end by
/// itself once the agent exited (an MCP server whose input closed saves
/// its state and quits) before it is ended ([`end_lingering`]).
const LEFTOVER_GRACE: Duration = Duration::from_secs(2);
/// How long a snapshot request waits for its answer.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
/// A recording stops (with a final note) once its file reaches this size.
const RECORD_LIMIT: u64 = 64 << 20;
/// Buffered recording data reaches the file at least this often.
const RECORD_FLUSH: Duration = Duration::from_millis(250);
/// Largest PNG sent to clients in one message.
pub const MAX_IMAGE_MESSAGE: usize = 16 << 20;
/// Encoded images kept per terminal.
const MAX_ENCODED_IMAGES: usize = 64;
/// A changed image is encoded again at most this often.
const IMAGE_REENCODE: Duration = Duration::from_millis(250);
/// When an encoding in progress is checked again (the encoder also wakes the pump).
const IMAGE_POLL: Duration = Duration::from_millis(250);
/// Threads encoding images for every terminal.
const IMAGE_WORKERS: usize = 2;
/// Encodings waiting for a worker.
const IMAGE_JOBS: usize = 256;

/// Small messages the emulator thread handles before any pending output.
enum Ctl {
    Input(Input),
    Wheel { up: bool, col: u16, row: u16, mods: u8 },
    Click { button: u8, col: u16, row: u16, mods: u8 },
    /// A resize waits in [`Resizes`]: apply it if its place in the output came.
    Resized,
    Scroll(Scroll),
    Snapshot(Sender<Snapshot>),
    Image(u32, Sender<Option<ImageData>>),
    /// A remote terminal's gap reset: a fresh engine drawn by these bytes
    /// (`emulator::replay::repaint` of the satellite's screen).
    Reset(Vec<u8>),
    /// The screen with the count of output bytes the engine had taken then.
    SnapshotAt(Sender<(Snapshot, u64)>),
    /// The program exited and its PTY is closed: once the output is drained
    /// the engine is frozen and its thread ends ([`EmulatorThread::freeze`]).
    Exited,
}

/// Engine resizes waiting for their place in the output stream: each is
/// applied once the engine has taken the output chunks the reader had read
/// before the PTY was resized, and before any later chunk.
#[derive(Default)]
struct Resizes {
    /// (output chunks read before the PTY resize, cols, rows), oldest first.
    queue: Mutex<std::collections::VecDeque<(u64, u16, u16)>>,
}

impl Resizes {
    fn push(&self, after: u64, cols: u16, rows: u16) {
        lock(&self.queue).push_back((after, cols, rows));
    }

    /// Takes back the newest resize when it is `resize` and not applied yet.
    fn withdraw(&self, resize: (u64, u16, u16)) -> bool {
        let mut queue = lock(&self.queue);
        queue.back() == Some(&resize) && queue.pop_back().is_some()
    }

    /// The size to take now that `fed` chunks were fed: the newest resize
    /// whose place came. The engine looks before every message, so resizes
    /// that come due together have no output between them: only the last
    /// one matters (a resize undone at once costs no reflow).
    fn due(&self, fed: u64) -> Option<(u16, u16)> {
        let mut queue = lock(&self.queue);
        let mut size = None;
        while queue.front().is_some_and(|(after, _, _)| *after <= fed) {
            size = queue.pop_front().map(|(_, cols, rows)| (cols, rows));
        }
        size
    }
}

/// The far end of a remote terminal's PTY (docs/VM-SESSIONS.md A2): a VM
/// session's satellite owns the PTY in its guest; this terminal keeps the
/// engine, frames and status on the hub. Implemented by the service's
/// satellite link.
pub trait RemotePty: Send + Sync {
    /// Whether input can go out now (an error says why not: the link is down).
    fn accepting(&self) -> Result<()>;
    /// Bytes for the program (already encoded by this terminal's engine).
    fn input(&self, bytes: Vec<u8>);
    fn resize(&self, cols: u16, rows: u16);
    /// Ends the program (`force`: at once; else as a terminal's hang-up).
    fn stop(&self, force: bool);
    /// The terminal exists at this size: its feed (output, gap resets, the
    /// exit) for the link.
    fn started(&self, _feed: RemoteFeed, _cols: u16, _rows: u16) {}
    /// Linked and caught up (no replay under way): text may be typed in for the program.
    fn settled(&self) -> bool {
        true
    }
}

/// Ends a remote terminal's program through its link (`force_kill`).
struct RemoteKiller(Arc<dyn RemotePty>);

impl std::fmt::Debug for RemoteKiller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteKiller")
    }
}

impl ChildKiller for RemoteKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        self.0.stop(true);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(RemoteKiller(self.0.clone()))
    }
}

/// What feeds a remote terminal: the link's reader hands it output, a gap
/// reset and the program's exit.
pub struct RemoteFeed {
    terminal: Weak<Terminal>,
    output: Sender<Vec<u8>>,
    control: Sender<Ctl>,
    exited: Sender<String>,
    pump: Sender<()>,
    bytes: Arc<AtomicU64>,
}

impl RemoteFeed {
    /// Program output; `false` when the engine is behind (the queue is full:
    /// the caller asks the satellite for a replay or a snapshot) or gone.
    pub fn output(&self, bytes: Vec<u8>) -> bool {
        let Some(terminal) = self.terminal.upgrade() else { return false };
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        terminal.last_output_ms.store(now_ms(), Ordering::Relaxed);
        self.output.try_send(bytes).is_ok()
    }

    /// The screen as the satellite's engine has it (after output the hub missed).
    pub fn reset(&self, vt: Vec<u8>) -> bool {
        send_control(&self.control, Ctl::Reset(vt), CONTROL_TIMEOUT).is_ok()
    }

    /// A dim line of the canvas's own in the terminal (*output skipped while offline*).
    pub fn note(&self, text: &str) -> bool {
        self.output.try_send(format!("\r\n\x1b[2m☁ {text}\x1b[0m\r\n").into_bytes()).is_ok()
    }

    /// The program exited with `code` (`error`: why, when it did not run).
    pub fn exited(&self, code: u32, error: Option<String>) {
        let Some(terminal) = self.terminal.upgrade() else { return };
        if !terminal.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(error) = error {
            *lock(&terminal.error) = Some(error);
        }
        *lock(&terminal.raw_exit_code) = Some(code);
        let requested = terminal.stopping.load(Ordering::Acquire);
        *lock(&terminal.exit_code) = Some(if requested { 0 } else { code });
        terminal.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pump.try_send(());
        let _ = self.exited.send(terminal.id.clone());
    }
}

/// A remote terminal (its PTY in a VM session's guest).
pub struct RemoteSpawn<'a> {
    pub id: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub copilot: bool,
    pub engine: Engine,
}

pub struct Terminal {
    pub id: String,
    /// An agent CLI (graceful stop, history tracking, idle auto-stop).
    pub copilot: bool,
    pub engine: Engine,
    control: Sender<Ctl>,
    /// The answer to a snapshot request that timed out (at most one per terminal is outstanding).
    pending_snapshot: Mutex<Option<Receiver<Snapshot>>>,
    /// Bumped by the emulator thread whenever the screen may have changed.
    generation: Arc<AtomicU64>,
    /// Raw bytes for the program (the PTY writer); dropped when it exits, so
    /// the writer thread ends.
    input: Mutex<Option<Sender<Vec<u8>>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub pid: u32,
    pub alive: AtomicBool,
    /// The exit code clients see: 0 when the canvas stopped the program
    /// (whatever its own code: a stop is not a failure).
    pub exit_code: Mutex<Option<u32>>,
    /// The process's own exit code (`exit_code` may report a requested stop as 0).
    pub raw_exit_code: Mutex<Option<u32>>,
    /// The process was killed (a stop's grace period ran out, or a forced stop).
    forced: AtomicBool,
    /// It was ended after its UI had quit on a stop request (see `stop`).
    ui_quit: AtomicBool,
    state: Arc<State>,
    pub started_ms: u64,
    pub last_output_ms: AtomicU64,
    pub error: Mutex<Option<String>>,
    /// The size the PTY and the emulator have (or have queued); see [`Terminal::resize`].
    size: AtomicU32,
    /// Output chunks the reader took from the PTY (where a resize goes in the stream).
    read_chunks: Arc<AtomicU64>,
    /// Engine resizes waiting for their place in the output.
    resizes: Arc<Resizes>,
    stopping: AtomicBool,
    /// Images encoded as PNG for clients, by image id.
    encoded: Mutex<HashMap<u32, CachedImage>>,
    /// Image ids an encoder is working on.
    encoding: Mutex<HashSet<u32>>,
    /// Wakes the frame pump (an encoded image is ready).
    pump: Sender<()>,
    /// The terminal's descendants seen so far, with their start times (Unix:
    /// the service's remember loop keeps them for a successor service, which
    /// ends those still running when this one died, MAC-LINUX-PLAN §5.2).
    pub remembered: Mutex<Vec<(u32, u64)>>,
    /// Its pseudoconsole host (`conhost.exe`, a child of the service, not of
    /// the program) and that host's start time: the priority manager keeps it
    /// at least at the program's class. `None` on Unix and when not found.
    pub console_host: Option<(u32, u64)>,
    /// A VM session's terminal: its PTY is in the guest (`None`: this machine's).
    remote: Option<Arc<dyn RemotePty>>,
    /// Set once the program exited and its PTY closed: the engine freezes.
    ended: Arc<AtomicBool>,
    /// The exited terminal's screen and scrollback once its engine is gone:
    /// snapshots and scrolling are answered from it (S3-06).
    frozen: OnceLock<Arc<Mutex<Frozen>>>,
    /// The program's start time (its pid is never taken for another process's).
    root_started: u64,
    /// An agent's job (Windows): what it started, also once the parents
    /// between are gone, so its end finds what it leaves ([`Terminal::leftovers`]).
    /// Closing it ends nothing (R2-B-01).
    #[cfg(windows)]
    job: Mutex<Option<canvas_sys::process::TreeJob>>,
    /// Why an agent got no job (logged once by the service).
    #[cfg(windows)]
    job_error: Mutex<Option<String>>,
    /// An agent's executable (`copilot.exe`): processes of it that it
    /// started are its own and end with it (Windows; Unix tells by session).
    #[cfg(windows)]
    program: Option<String>,
    /// The console the program's processes report ([`Terminal::note_console`]).
    #[cfg(windows)]
    console: OnceLock<u32>,
}

/// An image's pixels as a PNG file, shared by every client that needs it.
#[derive(Clone)]
pub struct EncodedImage {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub png: Arc<[u8]>,
}

/// The last encoding of one image.
struct CachedImage {
    generation: u64,
    /// `None`: the image is gone or too large to send.
    encoded: Option<EncodedImage>,
    at: Instant,
}

/// What can be sent of an image now ([`Terminal::image_png`]).
pub enum ImagePng {
    Ready(EncodedImage),
    /// Being encoded, or encoded too recently to encode again: ask again at this time.
    Pending(Instant),
    /// Gone, or too large to send.
    Unavailable,
}

/// A program's desktop notification: (terminal id, title, body).
pub type Notice = (String, String, String);

/// What the emulator thread reports back (title, progress, bells).
#[derive(Default)]
struct State {
    title: Mutex<String>,
    progress: AtomicU8,
    progress_seen: AtomicBool,
    progress_ms: AtomicU64,
    bells: AtomicU32,
    /// The program shows its full-screen (alternate) view.
    alt_screen: AtomicBool,
}

pub struct Spawn<'a> {
    pub id: &'a str,
    pub program: &'a str,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub copilot: bool,
    pub engine: Engine,
}

pub struct Hooks {
    /// Wakes the frame pump after new output.
    pub pump: Sender<()>,
    /// Receives the terminal id when its process exits.
    pub exited: Sender<String>,
    /// OSC 52 copies: (terminal id, text).
    pub clipboard: Sender<(String, String)>,
    pub bytes: Arc<AtomicU64>,
    /// Title, progress or bell changed: the terminal's info is resent.
    pub changed: Sender<String>,
    /// Desktop notifications with their text.
    pub notify: Option<Sender<Notice>>,
    /// Every chunk of the program's raw output, in order and before the
    /// engine takes it (a VM session's satellite keeps it for replays).
    pub raw: Option<Sender<Vec<u8>>>,
}

/// Delivers engine events to the terminal's state and hooks.
struct Sink<'a> {
    id: &'a str,
    state: &'a State,
    input: &'a Sender<Vec<u8>>,
    clipboard: &'a Sender<(String, String)>,
    changed: &'a Sender<String>,
    notify: Option<&'a Sender<Notice>>,
    dirty: bool,
    /// Answers queries (cursor reports, colours): off for a remote terminal,
    /// whose guest's own engine answers them (docs/VM-SESSIONS-PLAN.md §5.6).
    replies: bool,
}

impl Events for Sink<'_> {
    fn reply(&mut self, bytes: Vec<u8>) {
        if !self.replies {
            return;
        }
        let _ = self.input.try_send(bytes);
    }
    fn title(&mut self, title: Option<String>) {
        *lock(&self.state.title) = title.unwrap_or_default();
        self.dirty = true;
    }
    fn bell(&mut self) {
        self.state.bells.fetch_add(1, Ordering::Relaxed);
        self.dirty = true;
    }
    fn clipboard(&mut self, text: String) {
        let _ = self.clipboard.try_send((self.id.to_owned(), text));
    }
    fn progress(&mut self, state: u8) {
        let first = !self.state.progress_seen.swap(true, Ordering::Relaxed);
        if self.state.progress.swap(state, Ordering::Relaxed) != state || first {
            self.state.progress_ms.store(now_ms(), Ordering::Relaxed);
            self.dirty = true;
        }
    }
    fn notify(&mut self) {
        self.state.bells.fetch_add(1, Ordering::Relaxed);
        self.dirty = true;
    }
    fn notification(&mut self, title: String, body: String) {
        if let Some(notify) = self.notify {
            let _ = notify.try_send((self.id.to_owned(), title, body));
        }
    }
}

impl Sink<'_> {
    fn finish(self) {
        if self.dirty {
            let _ = self.changed.try_send(self.id.to_owned());
        }
    }
}

fn pack(cols: u16, rows: u16) -> u32 {
    ((cols as u32) << 16) | rows as u32
}

/// Opens a pseudoconsole and finds the console host it started (Windows):
/// the one host process of this service that was not there before. One
/// terminal opens at a time, so concurrent launches cannot mix them up.
fn open_pty(size: PtySize) -> Result<(portable_pty::PtyPair, Option<(u32, u64)>)> {
    static OPENING: Mutex<()> = Mutex::new(());
    let _one = lock(&OPENING);
    let before = canvas_sys::process::console_hosts();
    let pair = native_pty_system().openpty(size).context("cannot create a pseudoconsole")?;
    let mut started = canvas_sys::process::console_hosts();
    started.retain(|host| !before.contains(host));
    Ok((pair, if started.len() == 1 { started.pop() } else { None }))
}

impl Terminal {
    pub fn spawn(spec: Spawn<'_>, hooks: Hooks) -> Result<Arc<Self>> {
        let cols = spec.cols.clamp(20, 1000);
        let rows = spec.rows.clamp(5, 500);
        anyhow::ensure!(std::path::Path::new(spec.cwd).is_dir(), "working directory does not exist: {}", spec.cwd);
        let (pair, console_host) = open_pty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut command = CommandBuilder::new(spec.program);
        command.args(spec.args);
        command.cwd(spec.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (key, value) in spec.env {
            if value.is_empty() && key.starts_with('-') {
                // "-NAME" with an empty value removes NAME from the inherited environment.
                command.env_remove(&key[1..]);
            } else {
                command.env(key, value);
            }
        }
        let mut child = pair.slave.spawn_command(command).with_context(|| format!("cannot start {}", spec.program))?;
        let pid = child.process_id().unwrap_or(0);
        // An agent's job at once: what it starts before it is in the job is
        // only in the service's (put in below, R2-B-03).
        #[cfg(windows)]
        let job = match spec.copilot.then(|| child.as_raw_handle()) {
            Some(Some(process)) => canvas_sys::process::TreeJob::new(process).map_err(|error| error.to_string()),
            Some(None) => Err("no process handle".to_owned()),
            None => Err(String::new()),
        };
        drop(pair.slave);
        let root_started = canvas_sys::process::process_start_ms(pid).unwrap_or(0);
        let killer = child.clone_killer();
        // From here on the program runs: a failure ends it, never leaves it running unseen (S3-13).
        let io = pair.master.try_clone_reader().context("pty reader").and_then(|reader| Ok((reader, pair.master.take_writer().context("pty writer")?)));
        let (reader, writer) = match io {
            Ok(io) => io,
            Err(error) => {
                let _ = canvas_sys::process::kill_tree(pid);
                let _ = child.kill();
                return Err(error);
            }
        };
        #[cfg(windows)]
        let (job, job_error, program) = match job {
            Ok(job) => {
                job.adopt_tree(pid, root_started);
                (Some(job), None, canvas_sys::process::image_name(pid))
            }
            Err(error) => (None, Some(error).filter(|e| !e.is_empty()), None),
        };

        let (input_tx, input_rx) = bounded::<Vec<u8>>(4096);
        let (control_tx, control_rx) = bounded::<Ctl>(CONTROL_QUEUE);
        let (output_tx, output_rx) = bounded::<Vec<u8>>(OUTPUT_QUEUE);
        let state = Arc::new(State::default());
        let generation = Arc::new(AtomicU64::new(1));
        let config = Config { cols, rows, scrollback: 5000, kitty_keyboard: true, pull_scrollback: emulator::PTY_PULLS_SCROLLBACK };
        let engine = spec.engine;
        let resizes = Arc::new(Resizes::default());
        let ended = Arc::new(AtomicBool::new(false));
        let terminal = Arc::new(Self {
            id: spec.id.to_owned(),
            copilot: spec.copilot,
            engine,
            control: control_tx,
            pending_snapshot: Mutex::new(None),
            generation: generation.clone(),
            input: Mutex::new(Some(input_tx.clone())),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            pid,
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            raw_exit_code: Mutex::new(None),
            forced: AtomicBool::new(false),
            ui_quit: AtomicBool::new(false),
            state: state.clone(),
            started_ms: now_ms(),
            last_output_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            size: AtomicU32::new(pack(cols, rows)),
            read_chunks: Arc::new(AtomicU64::new(0)),
            resizes: resizes.clone(),
            stopping: AtomicBool::new(false),
            encoded: Mutex::new(HashMap::new()),
            encoding: Mutex::new(HashSet::new()),
            pump: hooks.pump.clone(),
            remembered: Mutex::new(Vec::new()),
            console_host,
            remote: None,
            ended: ended.clone(),
            frozen: OnceLock::new(),
            root_started,
            #[cfg(windows)]
            job: Mutex::new(job),
            #[cfg(windows)]
            job_error: Mutex::new(job_error),
            #[cfg(windows)]
            program,
            #[cfg(windows)]
            console: OnceLock::new(),
        });
        // An agent's console, if it is attached already (else read later, R2-B-01).
        if spec.copilot {
            terminal.note_console();
        }
        // Weak: the emulator thread must not keep the terminal alive.
        let weak = Arc::downgrade(&terminal);
        let fail: FailHook = Box::new(move |message| {
            if let Some(terminal) = weak.upgrade() {
                *lock(&terminal.error) = Some(message);
                terminal.force_kill();
            }
        });
        let weak = Arc::downgrade(&terminal);
        let freeze: FreezeHook = Box::new(move |frozen| {
            if let Some(terminal) = weak.upgrade() {
                let _ = terminal.frozen.set(frozen);
            }
        });
        let started = spawn_emulator(
            EmulatorThread {
                id: spec.id.to_owned(),
                cols,
                rows,
                control: control_rx,
                output: output_rx,
                generation,
                state,
                input: input_tx,
                clipboard: hooks.clipboard.clone(),
                changed: hooks.changed.clone(),
                pump: hooks.pump.clone(),
                fail: Some(fail),
                burst: 0,
                notify: hooks.notify.clone(),
                bridge: InlineImages::default(),
                replies: true,
                remake: None,
                fed: 0,
                resizes,
                chunks: 0,
                exit: Some(ExitHook { ended, freeze }),
                output_done: false,
            },
            move || emulator::create(engine, &config),
        )
        .and_then(|_| spawn_writer(writer, input_rx))
        .and_then(|_| spawn_reader(terminal.clone(), reader, output_tx, hooks.bytes, hooks.raw, terminal.read_chunks.clone()))
        .and_then(|_| spawn_waiter(terminal.clone(), child, hooks.exited, hooks.pump));
        if let Err(error) = started {
            // The program cannot be served: it is ended, not left running unseen (S3-13).
            terminal.force_kill();
            terminal.close_pty();
            return Err(error).context("cannot start the terminal's threads");
        }
        Ok(terminal)
    }

    /// A terminal whose program runs in a VM session's guest: the engine,
    /// frames and status here, the PTY there (`pty`). The returned feed
    /// gives it the program's output, gap resets and its exit.
    pub fn spawn_remote(spec: RemoteSpawn<'_>, hooks: Hooks, pty: Arc<dyn RemotePty>) -> (Arc<Self>, RemoteFeed) {
        let cols = spec.cols.clamp(20, 1000);
        let rows = spec.rows.clamp(5, 500);
        let (input_tx, input_rx) = bounded::<Vec<u8>>(4096);
        let (control_tx, control_rx) = bounded::<Ctl>(CONTROL_QUEUE);
        let (output_tx, output_rx) = bounded::<Vec<u8>>(OUTPUT_QUEUE);
        let state = Arc::new(State::default());
        let generation = Arc::new(AtomicU64::new(1));
        let config = Config { cols, rows, scrollback: 5000, kitty_keyboard: true, pull_scrollback: true };
        let engine = spec.engine;
        let resizes = Arc::new(Resizes::default());
        let terminal = Arc::new(Self {
            id: spec.id.to_owned(),
            copilot: spec.copilot,
            engine,
            control: control_tx.clone(),
            pending_snapshot: Mutex::new(None),
            generation: generation.clone(),
            input: Mutex::new(Some(input_tx.clone())),
            master: Mutex::new(None),
            killer: Mutex::new(Box::new(RemoteKiller(pty.clone()))),
            pid: 0,
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            raw_exit_code: Mutex::new(None),
            forced: AtomicBool::new(false),
            ui_quit: AtomicBool::new(false),
            state: state.clone(),
            started_ms: now_ms(),
            last_output_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            size: AtomicU32::new(pack(cols, rows)),
            read_chunks: Arc::new(AtomicU64::new(0)),
            resizes: resizes.clone(),
            stopping: AtomicBool::new(false),
            encoded: Mutex::new(HashMap::new()),
            encoding: Mutex::new(HashSet::new()),
            pump: hooks.pump.clone(),
            remembered: Mutex::new(Vec::new()),
            console_host: None,
            remote: Some(pty.clone()),
            // A remote terminal keeps its engine: its link may replay or reset it.
            ended: Arc::default(),
            frozen: OnceLock::new(),
            root_started: 0,
            #[cfg(windows)]
            job: Mutex::new(None),
            #[cfg(windows)]
            job_error: Mutex::new(None),
            #[cfg(windows)]
            program: None,
            #[cfg(windows)]
            console: OnceLock::new(),
        });
        let weak = Arc::downgrade(&terminal);
        let fail: FailHook = Box::new(move |message| {
            if let Some(terminal) = weak.upgrade() {
                *lock(&terminal.error) = Some(message);
                terminal.force_kill();
            }
        });
        spawn_emulator(
            EmulatorThread {
                id: spec.id.to_owned(),
                cols,
                rows,
                control: control_rx,
                output: output_rx,
                generation,
                state,
                input: input_tx,
                clipboard: hooks.clipboard.clone(),
                changed: hooks.changed.clone(),
                pump: hooks.pump.clone(),
                fail: Some(fail),
                burst: 0,
                notify: hooks.notify.clone(),
                bridge: InlineImages::default(),
                replies: false,
                fed: 0,
                remake: Some(Box::new(move |cols, rows| emulator::create(engine, &Config { cols, rows, scrollback: 5000, kitty_keyboard: true, pull_scrollback: true }))),
                resizes,
                chunks: 0,
                exit: None,
                output_done: false,
            },
            move || emulator::create(engine, &config),
        )
        .expect("spawn emulator thread");
        // Input the engine encoded goes to the guest's PTY through the link.
        let writer = pty.clone();
        std::thread::Builder::new()
            .name("remote-writer".into())
            .spawn(move || {
                for bytes in input_rx {
                    writer.input(bytes);
                }
            })
            .expect("spawn remote writer");
        let feed = RemoteFeed { terminal: Arc::downgrade(&terminal), output: output_tx, control: control_tx, exited: hooks.exited, pump: hooks.pump, bytes: hooks.bytes };
        (terminal, feed)
    }

    /// Its PTY is in a VM session's guest.
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// A local terminal, or a remote one whose link is up and caught up.
    pub fn remote_settled(&self) -> bool {
        self.remote.as_ref().is_none_or(|remote| remote.settled())
    }

    /// Its size now (columns, rows).
    pub fn size(&self) -> (u16, u16) {
        let packed = self.size.load(Ordering::Acquire);
        ((packed >> 16) as u16, packed as u16)
    }

    pub fn info(&self) -> TermInfo {
        TermInfo {
            id: self.id.clone(),
            alive: self.alive.load(Ordering::Acquire),
            exit_code: *lock(&self.exit_code),
            title: lock(&self.state.title).clone(),
            pid: self.pid,
            started_ms: self.started_ms,
            last_output_ms: self.last_output_ms.load(Ordering::Relaxed),
            copilot: self.copilot,
            error: lock(&self.error).clone(),
            progress: self.state.progress.load(Ordering::Relaxed),
            bells: self.state.bells.load(Ordering::Relaxed),
            progress_seen: self.state.progress_seen.load(Ordering::Relaxed),
            progress_ms: self.state.progress_ms.load(Ordering::Relaxed),
        }
    }

    /// Changes whenever the screen may have changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The exited terminal's frozen screen, once its engine is gone.
    fn frozen(&self) -> Option<MutexGuard<'_, Frozen>> {
        self.frozen.get().map(|frozen| lock(frozen))
    }

    /// Its engine is gone (the program exited): the last screen and the
    /// scrollback are kept as rows (S3-06).
    pub fn is_frozen(&self) -> bool {
        self.frozen.get().is_some()
    }

    /// The current viewport (a round trip to the emulator thread).
    pub fn snapshot(&self) -> Option<Snapshot> {
        if let Some(frozen) = self.frozen() {
            return Some(frozen.snapshot());
        }
        request_snapshot(&self.control, &self.pending_snapshot, CONTROL_TIMEOUT, SNAPSHOT_TIMEOUT)
    }

    /// Asks the emulator thread for its viewport without waiting: the answer
    /// comes on the returned channel. The frame pump asks every terminal it
    /// needs at once and takes the answers as they come, so a busy or stuck
    /// engine holds only its own frames. `None`: the control queue is full
    /// (a busy engine; ask again later) or the engine has stopped.
    pub fn ask_snapshot(&self) -> Option<Receiver<Snapshot>> {
        let (tx, rx) = bounded(1);
        match self.frozen() {
            Some(frozen) => tx.send(frozen.snapshot()).ok()?,
            None => self.control.try_send(Ctl::Snapshot(tx)).ok()?,
        }
        Some(rx)
    }

    /// The screen and the count of the program's output bytes the engine
    /// had taken when it was drawn (a satellite's gap reset names that
    /// offset, so the hub asks for the rest from there).
    pub fn snapshot_at(&self) -> Option<(Snapshot, u64)> {
        if let Some(frozen) = self.frozen() {
            return Some((frozen.snapshot(), frozen.fed));
        }
        let (tx, rx) = bounded(1);
        self.control.send_timeout(Ctl::SnapshotAt(tx), CONTROL_TIMEOUT).ok()?;
        rx.recv_timeout(SNAPSHOT_TIMEOUT).ok()
    }

    /// Scrolls a frozen screen (`false`: the engine runs; it scrolls itself).
    /// A screen scrolled keeps its scrollback among the most recent ones.
    fn scroll_frozen(&self, scroll: Scroll) -> bool {
        let Some(kept) = self.frozen.get() else { return false };
        lock(kept).scroll(scroll);
        crate::emulator::frozen::recent().touch(kept);
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pump.try_send(());
        true
    }

    /// An image as a PNG file, encoded once per generation on an encoder
    /// thread and shared by all clients. Never blocks: an image not encoded
    /// yet is queued for encoding ([`ImagePng::Pending`]), and a changed image
    /// is encoded again at most every [`IMAGE_REENCODE`].
    pub fn image_png(self: &Arc<Self>, id: u32, generation: u64) -> ImagePng {
        // An exited terminal's images were encoded when its engine froze (R2-B-06).
        if let Some(frozen) = self.frozen() {
            return frozen_image(&frozen, id, generation);
        }
        let now = Instant::now();
        if let Some(cached) = lock(&self.encoded).get(&id) {
            if cached.generation == generation {
                return cached.encoded.clone().map_or(ImagePng::Unavailable, ImagePng::Ready);
            }
            let next = cached.at + IMAGE_REENCODE;
            if next > now {
                return ImagePng::Pending(next);
            }
        }
        if lock(&self.encoding).insert(id) {
            let job = EncodeJob { terminal: Arc::downgrade(self), id, generation };
            if image_encoder().is_none_or(|jobs| jobs.try_send(job).is_err()) {
                lock(&self.encoding).remove(&id);
            }
        }
        ImagePng::Pending(now + IMAGE_POLL)
    }

    /// Encodes one image (an encoder thread): its pixels from the emulator
    /// thread, then PNG; the result is cached and the pump woken.
    fn encode_image(&self, id: u32, generation: u64) {
        let image = (|| {
            let (tx, rx) = bounded(1);
            self.control.send_timeout(Ctl::Image(id, tx), CONTROL_TIMEOUT).ok()?;
            rx.recv_timeout(SNAPSHOT_TIMEOUT).ok()?
        })();
        let encoded = image.filter(|image| emulator::image_size_ok(image.width, image.height)).and_then(|image| {
            let png = emulator::encode_png(image.width, image.height, &image.rgba)?;
            (png.len() <= MAX_IMAGE_MESSAGE).then(|| EncodedImage { generation: image.generation, width: image.width, height: image.height, png: png.into() })
        });
        let generation = encoded.as_ref().map_or(generation, |e| e.generation);
        {
            let mut cache = lock(&self.encoded);
            if cache.len() >= MAX_ENCODED_IMAGES && !cache.contains_key(&id) {
                cache.clear();
            }
            cache.insert(id, CachedImage { generation, encoded, at: Instant::now() });
        }
        lock(&self.encoding).remove(&id);
        let _ = self.pump.try_send(());
    }

    /// Queues a control message, waiting up to [`CONTROL_TIMEOUT`] for room
    /// (the service's own requests, on threads of their own).
    fn command(&self, ctl: Ctl) -> Result<()> {
        send_control(&self.control, ctl, CONTROL_TIMEOUT)
    }

    /// [`offer`]: a client's control message, never waited for.
    fn offer(&self, ctl: Ctl) -> Result<()> {
        offer(&self.control, ctl, &self.id)
    }

    /// Raw bytes straight to the program (`false`: gone, or its queue full).
    fn send_raw(&self, bytes: &[u8]) -> bool {
        lock(&self.input).as_ref().is_some_and(|input| input.try_send(bytes.to_vec()).is_ok())
    }

    /// A click: forwarded only when the app asked for mouse reports.
    pub fn click(&self, button: u8, col: u16, row: u16, mods: u8) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(remote) = &self.remote {
            remote.accepting()?;
        }
        self.offer(Ctl::Click { button, col, row, mods })
    }

    /// A client's keys, paste and focus for the program. Never waits: a
    /// terminal whose engine is far behind refuses the input at once (the
    /// error says so and names the terminal).
    pub fn offer_input(&self, input: &Input) -> Result<()> {
        anyhow::ensure!(self.alive.load(Ordering::Acquire), "terminal {} has exited", self.id);
        if let Some(remote) = &self.remote {
            remote.accepting()?;
        }
        self.offer(Ctl::Input(input.clone()))
    }

    /// The service's own typing (a passive run's message and its Enter, a
    /// stop's keys): waits up to [`CONTROL_TIMEOUT`] for room behind a busy
    /// engine instead of being refused at once as a client's keys are (R2-B-07).
    pub fn send_input(&self, input: &Input) -> Result<()> {
        anyhow::ensure!(self.alive.load(Ordering::Acquire), "terminal {} has exited", self.id);
        if let Some(remote) = &self.remote {
            remote.accepting()?;
        }
        self.command(Ctl::Input(input.clone()))
    }

    /// Resizes the PTY and the emulator together. Runs under the PTY lock so
    /// concurrent resizes reach both in the same order. The engine's resize
    /// is queued first, at its place in the output: after the chunks read so
    /// far (drawn for the old size), before whatever the resized PTY sends
    /// (ConPTY repaints at once). It never waits for a busy engine. When the
    /// PTY refuses, the engine goes back to the old size too and the size is
    /// not stored, so the next call retries instead of skipping it as unchanged.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(20, 1000);
        let rows = rows.clamp(5, 500);
        let packed = pack(cols, rows);
        let master = lock(&self.master);
        let current = self.size.load(Ordering::Acquire);
        // A frozen screen keeps the size the program last drew for.
        if current == packed || self.is_frozen() {
            return Ok(());
        }
        // A remote terminal's output has no place to wait for: at once.
        let place = || if self.remote.is_some() { 0 } else { self.read_chunks.load(Ordering::Acquire) };
        let after = place();
        self.queue_resize(after, cols, rows);
        if let Some(master) = master.as_ref()
            && let Err(error) = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        {
            // Not taken yet: withdrawn; else the engine goes back.
            if !self.resizes.withdraw((after, cols, rows)) {
                self.queue_resize(place(), (current >> 16) as u16, current as u16);
            }
            return Err(error);
        }
        if let Some(remote) = &self.remote {
            remote.resize(cols, rows);
        }
        self.size.store(packed, Ordering::Release);
        Ok(())
    }

    /// Queues an engine resize after output chunk `after` and wakes the
    /// engine (a full control queue means it is awake already).
    fn queue_resize(&self, after: u64, cols: u16, rows: u16) {
        self.resizes.push(after, cols, rows);
        let _ = self.control.try_send(Ctl::Resized);
    }

    /// Mouse wheel: forwarded to the app when it wants it, else scrolls history.
    pub fn wheel(&self, up: bool, col: u16, row: u16, mods: u8) -> Result<()> {
        if self.scroll_frozen(Scroll::Delta(if up { 3 } else { -3 })) {
            return Ok(());
        }
        self.offer(Ctl::Wheel { up, col, row, mods })
    }

    pub fn scroll(&self, delta: i32) {
        let scroll = match delta {
            i32::MAX => Scroll::Top,
            i32::MIN => Scroll::Bottom,
            d => Scroll::Delta(d),
        };
        if !self.scroll_frozen(scroll) {
            let _ = self.offer(Ctl::Scroll(scroll));
        }
    }

    /// Graceful stop for agents, each step only while the program still runs:
    /// Ctrl+D (Copilot CLI's "Shutdown"; encoded by the emulator, so programs
    /// using the kitty keyboard protocol read it as a key), again after 2 s
    /// (some agents ask for a second one), Ctrl+C twice after 3.5 s (Copilot
    /// and Claude Code exit on a second Ctrl+C; ConPTY turns a raw one into a
    /// console Ctrl+C event, which ends a program stuck after its UI quit),
    /// `/exit` and Enter after 6 s (the agents' own quit command, for one whose
    /// keys were taken by a prompt), then force after [`STOP_GRACE`] (a busy
    /// Copilot with many MCP servers needs more than the 5 s it once had: on
    /// `service stop` every one was killed). A program that left its
    /// full-screen view on the request (its UI quit) but whose process lingers
    /// is ended 3 s later (OpenCode 1.19 can linger for minutes on a busy machine).
    pub fn stop(self: &Arc<Self>, force: bool) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        if force || !self.copilot {
            self.force_kill();
            return;
        }
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let key = |c: char, mods: u8| Input::Key { key: crate::protocol::Key::Char(c), mods };
        let ctrl = crate::protocol::MOD_CTRL;
        if self.command(Ctl::Input(key('d', ctrl))).is_err() {
            self.send_raw(&[0x04]);
        }
        let me = self.clone();
        let full_screen = self.state.alt_screen.load(Ordering::Acquire);
        let stopper = std::thread::Builder::new().name("stop".into()).spawn(move || {
            // Measured on the clock, checked every 100 ms: a busy or throttled
            // machine may wake this thread late, never move the steps later.
            let asked = Instant::now();
            let mut ui_quit_at: Option<Instant> = None;
            // (when, what: raw bytes when the engine cannot encode the key)
            let steps: [(Duration, Input, &[u8]); 5] = [
                (Duration::from_secs(2), key('d', ctrl), &[0x04]),
                (Duration::from_millis(3_500), key('c', ctrl), &[0x03]),
                (Duration::from_millis(3_800), key('c', ctrl), &[0x03]),
                (Duration::from_secs(6), Input::Raw(b"/exit".to_vec()), b"/exit"),
                (Duration::from_millis(6_300), Input::Key { key: crate::protocol::Key::Enter, mods: 0 }, b"\r"),
            ];
            let mut next = 0;
            loop {
                if !me.alive.load(Ordering::Acquire) {
                    return;
                }
                let elapsed = asked.elapsed();
                if full_screen && ui_quit_at.is_none() && !me.state.alt_screen.load(Ordering::Acquire) {
                    ui_quit_at = Some(Instant::now());
                }
                if ui_quit_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(3)) {
                    me.ui_quit.store(true, Ordering::Release);
                    break;
                }
                if elapsed >= STOP_GRACE {
                    break;
                }
                // A program whose UI already quit only waits for its process.
                while ui_quit_at.is_none() && next < steps.len() && elapsed >= steps[next].0 {
                    let (_, input, raw) = &steps[next];
                    if me.command(Ctl::Input(input.clone())).is_err() {
                        me.send_raw(raw);
                    }
                    next += 1;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            me.force_kill();
        });
        if stopper.is_err() {
            // No thread for the graceful steps: stopped at once instead of never.
            self.force_kill();
        }
    }

    /// The program quit its UI on a stop request but its process lingered and
    /// was ended (not a failure to honour the request).
    pub fn ui_quit_lingered(&self) -> bool {
        self.ui_quit.load(Ordering::Acquire)
    }

    /// The program had to be killed (a stop's grace period ran out, or a forced stop).
    pub fn was_forced(&self) -> bool {
        self.forced.load(Ordering::Acquire)
    }

    pub fn force_kill(&self) {
        self.forced.store(true, Ordering::Release);
        if self.pid != 0 {
            let _ = canvas_sys::process::kill_tree(self.pid);
            for (pid, started, _) in self.leftovers() {
                canvas_sys::process::kill_started(pid, started);
            }
        }
        let _ = lock(&self.killer).kill();
    }

    /// What the program leaves that ends with it (R2-B-01): what is still
    /// attached to its terminal (its console on Windows, its session on
    /// Unix), and an agent's own program (its CLI, which holds the session's
    /// lock, S3-14), each as (pid, start time, its own program). What it
    /// started with a console of its own or none (a server, a VS Code, a
    /// build server other sessions use), in a session of its own, or that
    /// broke away from its job keeps running ([`canvas_sys::process::leftovers`]).
    fn leftovers(&self) -> Vec<(u32, u64, bool)> {
        if self.pid == 0 {
            return Vec::new();
        }
        let (program, found) = self.started_processes();
        canvas_sys::process::leftovers(self.pid, self.root_started, program, &found)
    }

    /// The processes an agent started, from its job (Windows: also those
    /// whose parents are gone; none without a job, whose console the
    /// pseudoconsole's closing ends anyway).
    #[cfg(windows)]
    fn started_processes(&self) -> (Option<&str>, Vec<canvas_sys::process::Leftover>) {
        use canvas_sys::process::{Leftover, console_of, process_entries, process_start_ms};
        let Some(pids) = lock(&self.job).as_ref().map(|job| job.pids()) else { return (None, Vec::new()) };
        self.note_console();
        // The console the program's processes report, and its host as found
        // when the pseudoconsole was opened.
        let host = self.console_host.filter(|(pid, started)| process_start_ms(*pid) == Some(*started)).map(|(pid, _)| pid);
        let consoles: Vec<u32> = self.console.get().copied().into_iter().chain(host).collect();
        let table: HashMap<u32, (u32, String)> = process_entries().into_iter().map(|(pid, parent, name)| (pid, (parent, name))).collect();
        let found = pids
            .into_iter()
            .filter(|pid| *pid != self.pid)
            .filter_map(|pid| {
                let (parent, name) = table.get(&pid)?.clone();
                let attached = console_of(pid).is_some_and(|console| consoles.contains(&console));
                Some(Leftover { pid, parent, started: process_start_ms(pid)?, name, attached })
            })
            .collect();
        (self.program.as_deref(), found)
    }

    /// Records the console the program is attached to, as the system
    /// reports it for every process on it, while it runs (Windows: read
    /// once; the agent's end compares with it after the agent is gone).
    fn note_console(&self) {
        #[cfg(windows)]
        if self.console.get().is_none()
            && self.alive.load(Ordering::Acquire)
            && canvas_sys::process::process_start_ms(self.pid) == Some(self.root_started)
            && let Some(console) = canvas_sys::process::console_of(self.pid)
        {
            let _ = self.console.set(console);
        }
    }

    /// The processes still in the terminal's session (Unix: its first
    /// process leads it; only its descendants can be in it).
    #[cfg(unix)]
    fn started_processes(&self) -> (Option<&str>, Vec<canvas_sys::process::Leftover>) {
        use canvas_sys::process::{Leftover, console_of, process_entries, process_start_ms};
        // A session's id is not given to another process while the session
        // lives; a leader running again under it is another process.
        if process_start_ms(self.pid).is_some_and(|started| started != self.root_started) {
            return (None, Vec::new());
        }
        let found = process_entries()
            .into_iter()
            .filter(|(pid, _, _)| *pid != self.pid && console_of(*pid) == Some(self.pid))
            .filter_map(|(pid, parent, name)| Some(Leftover { pid, parent, started: process_start_ms(pid)?, name, attached: true }))
            .collect();
        (None, found)
    }

    /// An agent that exited on its own (R2-B-01, S3-14): its own program
    /// ends now (a CLI left behind holds the session's lock); the rest of
    /// what is still attached to its terminal is answered, to end after
    /// [`LEFTOVER_GRACE`] unless it ends by itself first ([`end_lingering`]).
    fn end_own_leftovers(&self) -> Vec<(u32, u64)> {
        let mut rest = Vec::new();
        for (pid, started, own) in self.leftovers() {
            if own {
                canvas_sys::process::kill_started(pid, started);
            } else {
                rest.push((pid, started));
            }
        }
        rest
    }

    /// Puts an agent's processes started before it was in its job there
    /// (R2-B-03; Windows, while it is young). Answers how many.
    pub fn adopt_strays(&self) -> usize {
        self.note_console();
        #[cfg(windows)]
        if let Some(job) = lock(&self.job).as_ref() {
            return job.adopt_tree(self.pid, self.root_started);
        }
        0
    }

    /// Why an agent got no job (Windows; at most once).
    pub fn take_job_error(&self) -> Option<String> {
        #[cfg(windows)]
        return lock(&self.job_error).take();
        #[cfg(not(windows))]
        None
    }

    /// Closes the pseudoconsole, which ends the reader thread.
    fn close_pty(&self) {
        lock(&self.master).take();
    }
}

/// An image of a frozen screen (`generation`: as its placement says; the
/// screen and its images no longer change).
fn frozen_image(frozen: &Frozen, id: u32, generation: u64) -> ImagePng {
    match frozen.images.get(&id) {
        Some(image) => ImagePng::Ready(EncodedImage { generation, width: image.width, height: image.height, png: image.png.clone() }),
        None => ImagePng::Unavailable,
    }
}

/// Queues a client's control message without waiting: a full queue (an
/// engine that fell [`CONTROL_QUEUE`] messages behind) refuses it at once
/// with the reason, and the client's reader goes on.
fn offer(control: &Sender<Ctl>, ctl: Ctl, id: &str) -> Result<()> {
    control.try_send(ctl).map_err(|error| match error {
        TrySendError::Full(_) => anyhow::anyhow!("terminal {id} is not responding: {} inputs are waiting for it; this one was not sent", control.len()),
        TrySendError::Disconnected(_) => anyhow::anyhow!("the emulator of terminal {id} has stopped"),
    })
}

fn send_control(control: &Sender<Ctl>, ctl: Ctl, timeout: Duration) -> Result<()> {
    control.send_timeout(ctl, timeout).map_err(|error| match error {
        SendTimeoutError::Timeout(_) => anyhow::anyhow!("terminal is not responding (input backlog is full)"),
        SendTimeoutError::Disconnected(_) => anyhow::anyhow!("terminal emulator has stopped"),
    })
}

/// An image to encode for clients.
struct EncodeJob {
    terminal: Weak<Terminal>,
    id: u32,
    generation: u64,
}

/// The queue of the image encoder threads (started on first use; `None` when
/// no thread could be started).
fn image_encoder() -> Option<&'static Sender<EncodeJob>> {
    static ENCODER: OnceLock<Option<Sender<EncodeJob>>> = OnceLock::new();
    ENCODER
        .get_or_init(|| {
            let (tx, rx) = bounded::<EncodeJob>(IMAGE_JOBS);
            let mut started = 0;
            for _ in 0..IMAGE_WORKERS {
                let jobs = rx.clone();
                let worker = move || {
                    for job in jobs {
                        if let Some(terminal) = job.terminal.upgrade() {
                            terminal.encode_image(job.id, job.generation);
                        }
                    }
                };
                started += usize::from(std::thread::Builder::new().name("image-encoder".into()).spawn(worker).is_ok());
            }
            (started > 0).then_some(tx)
        })
        .as_ref()
}

/// Asks the emulator thread for its viewport. At most one request per
/// terminal is outstanding: after a timeout the unanswered request is kept
/// in `pending` and the next call waits for its answer instead of queueing
/// another; an answer that arrived meanwhile is stale and replaced by a new
/// request. A full control queue waits at most `send`. The `pending` lock is
/// not held while waiting.
fn request_snapshot(control: &Sender<Ctl>, pending: &Mutex<Option<Receiver<Snapshot>>>, send: Duration, wait: Duration) -> Option<Snapshot> {
    let reply = match lock(pending).take() {
        // Its answer, when it comes, shows the screen after this call began.
        Some(reply) if reply.is_empty() => reply,
        _ => {
            let (tx, rx) = bounded(1);
            control.send_timeout(Ctl::Snapshot(tx), send).ok()?;
            rx
        }
    };
    match reply.recv_timeout(wait) {
        Ok(snapshot) => Some(snapshot),
        Err(RecvTimeoutError::Timeout) => {
            *lock(pending) = Some(reply);
            None
        }
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// Called once with the error message when the terminal's engine panics.
type FailHook = Box<dyn FnOnce(String) + Send>;

/// Hands the terminal its frozen screen when the engine ends.
type FreezeHook = Box<dyn FnOnce(Arc<Mutex<Frozen>>) + Send>;

/// How a local terminal's engine learns its program is over.
struct ExitHook {
    /// Set once the program exited and its PTY closed ([`Ctl::Exited`] wakes the thread).
    ended: Arc<AtomicBool>,
    freeze: FreezeHook,
}

/// The emulator thread's ends of the channels and its hooks.
struct EmulatorThread {
    id: String,
    /// The current size (for the blank screen shown after a failure).
    cols: u16,
    rows: u16,
    control: Receiver<Ctl>,
    output: Receiver<Vec<u8>>,
    generation: Arc<AtomicU64>,
    state: Arc<State>,
    input: Sender<Vec<u8>>,
    clipboard: Sender<(String, String)>,
    changed: Sender<String>,
    pump: Sender<()>,
    fail: Option<FailHook>,
    /// Control messages handled in a row (see [`EmulatorThread::next`]).
    burst: u32,
    notify: Option<Sender<Notice>>,
    /// Inline images that ConPTY passes through, as Kitty graphics.
    bridge: InlineImages,
    /// The engine answers the program's queries (off for a remote terminal).
    replies: bool,
    /// Makes a fresh engine of a size (a remote terminal's gap reset).
    remake: Option<Remake>,
    /// Output bytes taken so far (before inline images are filtered).
    fed: u64,
    /// Resizes waiting for their place in the output.
    resizes: Arc<Resizes>,
    /// Output chunks taken so far (the places resizes wait for).
    chunks: u64,
    /// A local terminal's: the engine is frozen and the thread ends once the
    /// program is over and its output drained (`None`: kept until dropped).
    exit: Option<ExitHook>,
    /// The output channel closed (the reader ended): no more output comes.
    output_done: bool,
}

/// Makes a fresh engine of a size.
type Remake = Box<dyn FnMut(u16, u16) -> Box<dyn Emulator> + Send>;

/// What the emulator thread handles next.
enum Msg {
    Control(Ctl),
    Output(Vec<u8>),
    /// A synchronized update's deadline passed with nothing else to do.
    SyncTimeout,
    /// The reader ended: all output is in.
    OutputDone,
}

/// Owns the emulator until the terminal is dropped (its control channel
/// closes) or its program exited and the output is drained (it freezes).
fn spawn_emulator(t: EmulatorThread, make: impl FnOnce() -> Box<dyn Emulator> + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("emulator".into()).spawn(move || t.run(make))
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

impl EmulatorThread {
    /// Runs the engine; a panic in it ends in [`EmulatorThread::failed`]
    /// instead of silently hanging the terminal.
    fn run(mut self, make: impl FnOnce() -> Box<dyn Emulator>) {
        self.bridge.set_size(self.cols, self.rows);
        let mut emu = match catch_unwind(AssertUnwindSafe(make)) {
            Ok(emu) => emu,
            Err(panic) => return self.failed(&panic_message(panic.as_ref()), None),
        };
        while let Some(mut msg) = self.next(emu.sync_deadline()) {
            let chunk = matches!(msg, Msg::Output(_));
            if let Msg::Output(bytes) = &msg {
                self.fed += bytes.len() as u64;
            }
            // A gap reset: a fresh engine of the same size, drawn by the repaint.
            if let Msg::Control(Ctl::Reset(bytes)) = msg {
                let (cols, rows) = (self.cols, self.rows);
                if let Some(fresh) = self.remake.as_mut().and_then(|remake| catch_unwind(AssertUnwindSafe(|| remake(cols, rows))).ok()) {
                    emu = fresh;
                }
                msg = Msg::Output(bytes);
            }
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.handle(emu.as_mut(), msg))) {
                let last = catch_unwind(AssertUnwindSafe(|| emu.snapshot())).ok();
                // Dropping a broken engine may panic as well.
                let _ = catch_unwind(AssertUnwindSafe(move || drop(emu)));
                return self.failed(&panic_message(panic.as_ref()), last);
            }
            if chunk {
                self.chunks += 1;
            }
            if self.output_done && self.exit.as_ref().is_some_and(|exit| exit.ended.load(Ordering::Acquire)) {
                return self.freeze(emu);
            }
        }
    }

    /// The program is over and its last output is in: keeps the screen and
    /// scrollback as rows ([`Frozen`]) for the terminal to answer from, drops
    /// the engine (cells, images, parser) and ends this thread (S3-06).
    fn freeze(mut self, mut emu: Box<dyn Emulator>) {
        let (fed, cols, rows) = (self.fed, self.cols, self.rows);
        let frozen = catch_unwind(AssertUnwindSafe(|| {
            let mut frozen = Frozen::capture(emu.as_mut(), fed);
            // Its pixels go with the engine: the images on screen are kept as PNG files.
            frozen.keep_images(emu.as_mut(), MAX_IMAGE_MESSAGE);
            frozen
        }))
        .unwrap_or_else(|_| Frozen::blank(cols, rows));
        // Dropping an engine may panic.
        let _ = catch_unwind(AssertUnwindSafe(move || drop(emu)));
        let frozen = Arc::new(Mutex::new(frozen));
        // Its scrollback is kept while it is among the most recently exited or scrolled.
        crate::emulator::frozen::recent().touch(&frozen);
        if let Some(exit) = self.exit.take() {
            (exit.freeze)(frozen.clone());
        }
        // What was asked before the terminal had the frozen screen.
        while let Ok(ctl) = self.control.try_recv() {
            let mut screen = lock(&frozen);
            match ctl {
                Ctl::Snapshot(reply) => {
                    let _ = reply.send(screen.snapshot());
                }
                Ctl::SnapshotAt(reply) => {
                    let _ = reply.send((screen.snapshot(), screen.fed));
                }
                Ctl::Scroll(scroll) => screen.scroll(scroll),
                _ => {}
            }
        }
        self.bump();
    }

    /// The next message: control first, then output, then the sync deadline.
    /// After [`CONTROL_BURST`] control messages in a row one waiting output
    /// chunk goes first, so a stream of control messages cannot stall output.
    /// `None` once the terminal is gone.
    fn next(&mut self, deadline: Option<Instant>) -> Option<Msg> {
        if self.burst >= CONTROL_BURST
            && let Ok(bytes) = self.output.try_recv()
        {
            self.burst = 0;
            return Some(Msg::Output(bytes));
        }
        let timer = deadline.map_or_else(never, at);
        let mut output_closed = false;
        let msg = select_biased! {
            recv(self.control) -> ctl => Msg::Control(ctl.ok()?),
            recv(self.output) -> bytes => match bytes {
                Ok(bytes) => Msg::Output(bytes),
                Err(_) => {
                    output_closed = true;
                    Msg::OutputDone
                }
            },
            recv(timer) -> _ => Msg::SyncTimeout,
        };
        if output_closed {
            // The reader ended; keep serving control messages.
            self.output = never();
            self.output_done = true;
        }
        match msg {
            Msg::Control(ctl) => {
                self.burst += 1;
                Some(Msg::Control(ctl))
            }
            msg => {
                self.burst = 0;
                Some(msg)
            }
        }
    }

    fn sink(&self) -> Sink<'_> {
        Sink { id: &self.id, state: &self.state, input: &self.input, clipboard: &self.clipboard, changed: &self.changed, notify: self.notify.as_ref(), dirty: false, replies: self.replies }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pump.try_send(());
    }

    fn handle(&mut self, emu: &mut dyn Emulator, msg: Msg) {
        // A resize whose place in the output came goes before this message
        // (and before this chunk, which was read after the PTY's resize).
        if let Some((cols, rows)) = self.resizes.due(self.chunks)
            && (cols, rows) != (self.cols, self.rows)
        {
            emu.resize(cols, rows);
            (self.cols, self.rows) = (cols, rows);
            self.bridge.set_size(cols, rows);
            self.bump();
        }
        match msg {
            Msg::Output(bytes) => {
                let bytes = self.bridge.filter(&bytes);
                let mut sink = self.sink();
                emu.feed(&bytes, &mut sink);
                sink.finish();
                self.state.alt_screen.store(emu.modes() & crate::protocol::MODE_ALT_SCREEN != 0, Ordering::Release);
                self.bump();
            }
            Msg::Control(ctl) => self.control(emu, ctl),
            Msg::SyncTimeout | Msg::OutputDone => {}
        }
        // A synchronized update whose end marker never came. Checked after
        // every message, so a steady stream of messages cannot postpone it.
        if emu.sync_deadline().is_some_and(|deadline| Instant::now() >= deadline) {
            let mut sink = self.sink();
            emu.end_sync(&mut sink);
            sink.finish();
            self.bump();
        }
    }

    fn control(&mut self, emu: &mut dyn Emulator, ctl: Ctl) {
        match ctl {
            Ctl::Input(input) => {
                if !matches!(input, Input::Focus(_)) && emu.display_offset() != 0 {
                    emu.scroll(Scroll::Bottom);
                    self.bump();
                }
                let bytes = emu.encode(&input);
                if !bytes.is_empty() {
                    let _ = self.input.try_send(bytes);
                }
            }
            Ctl::Click { button, col, row, mods } => {
                if let Some(bytes) = emu.encode_click(button, col, row, mods) {
                    if emu.display_offset() != 0 {
                        emu.scroll(Scroll::Bottom);
                        self.bump();
                    }
                    let _ = self.input.try_send(bytes);
                }
            }
            Ctl::Wheel { up, col, row, mods } => match emu.encode_wheel(up, col, row, mods) {
                Some(bytes) => {
                    if emu.display_offset() != 0 {
                        emu.scroll(Scroll::Bottom);
                        self.bump();
                    }
                    let _ = self.input.try_send(bytes);
                }
                None => {
                    emu.scroll(Scroll::Delta(if up { 3 } else { -3 }));
                    self.bump();
                }
            },
            // Wakes: their work is done before each message (`handle`, `run`).
            Ctl::Resized | Ctl::Exited => {}
            Ctl::Scroll(scroll) => {
                emu.scroll(scroll);
                self.bump();
            }
            Ctl::Snapshot(reply) => {
                let _ = reply.send(emu.snapshot());
            }
            Ctl::SnapshotAt(reply) => {
                let _ = reply.send((emu.snapshot(), self.fed));
            }
            Ctl::Image(id, reply) => {
                let _ = reply.send(emu.image(id));
            }
            // Handled by `run` (it replaces the engine).
            Ctl::Reset(_) => {}
        }
    }

    /// The engine panicked: report it, kill the program (the reader stops
    /// feeding once the output channel is gone) and keep answering snapshots
    /// with the screen as the engine left it, or a blank one.
    fn failed(mut self, panic: &str, last: Option<Snapshot>) {
        let message = format!("terminal emulator failed: {panic}");
        eprintln!("terminal {}: {message}", self.id);
        self.output = never();
        if let Some(fail) = self.fail.take() {
            fail(message);
        }
        let _ = self.changed.try_send(self.id.clone());
        self.bump();
        let screen = last.unwrap_or_else(|| Snapshot {
            cols: self.cols,
            rows: self.rows,
            lines: vec![Vec::new(); self.rows as usize],
            ..Snapshot::default()
        });
        for ctl in self.control.iter() {
            match ctl {
                Ctl::Snapshot(reply) => {
                    let _ = reply.send(screen.clone());
                }
                Ctl::SnapshotAt(reply) => {
                    let _ = reply.send((screen.clone(), self.fed));
                }
                _ => {}
            }
        }
    }
}

/// Writes input to the PTY until every sender is gone (the program exited).
fn spawn_writer(mut writer: Box<dyn Write + Send>, input: Receiver<Vec<u8>>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            for bytes in input {
                if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })
        .map(|_| ())
}

fn spawn_reader(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>, output: Sender<Vec<u8>>, bytes: Arc<AtomicU64>, mut raw: Option<Sender<Vec<u8>>>, read_chunks: Arc<AtomicU64>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buffer = vec![0u8; READ_CHUNK];
            let mut tap = Tap::start(&terminal);
            let mut emulator_gone = false;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Counted before it can reach the engine: a resize
                        // that saw this count is applied after this chunk,
                        // one that did not before it, so nothing read after
                        // the PTY was resized is fed at the old size.
                        read_chunks.fetch_add(1, Ordering::AcqRel);
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                        let now = now_ms();
                        terminal.last_output_ms.store(now, Ordering::Relaxed);
                        let chunk = buffer[..n].to_vec();
                        // Before the engine: whoever keeps the raw output is never behind it.
                        if raw.as_ref().is_some_and(|raw| raw.send(chunk.clone()).is_err()) {
                            raw = None;
                        }
                        if let Some(sender) = &tap {
                            let size = terminal.size.load(Ordering::Relaxed);
                            if sender.send(TapChunk { ms: now, size, bytes: chunk.clone() }).is_err() {
                                tap = None;
                            }
                        }
                        if emulator_gone {
                            // Drained until EOF so closing the pseudoconsole cannot block.
                            continue;
                        }
                        // Blocks when the emulator falls behind: ConPTY then slows the program.
                        if output.send(chunk).is_err() {
                            // The emulator failed: nothing can show this program any more.
                            terminal.force_kill();
                            emulator_gone = true;
                        }
                    }
                }
            }
        })
        .map(|_| ())
}

/// Directory where the service records every terminal's raw output (golden corpus).
pub const RECORD_ENV: &str = "ATELIER_RECORD";

/// The readable part of a recording's file name: the id (at most 64
/// characters) with anything but ASCII letters, digits, '-' and '_' replaced.
pub fn recording_stem(id: &str) -> String {
    id.chars().take(64).map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Where the recording tap writes a terminal's output: [`recording_stem`]
/// plus a stable hash of the whole id, so distinct ids never share a file.
pub fn recording_path(dir: &Path, id: &str) -> PathBuf {
    // FNV-1a: stable across builds, unlike `DefaultHasher`.
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    dir.join(format!("{}-{:08x}.vtrec", recording_stem(id), (hash ^ (hash >> 32)) as u32))
}

/// One PTY read for the recording tap.
struct TapChunk {
    ms: u64,
    size: u32,
    bytes: Vec<u8>,
}

/// Appends each PTY read to [`recording_path`] (format: corpus/README.md;
/// chunks as hex, which `atelier record --session` normalizes). Runs on
/// its own thread with buffered writes, flushed at least every
/// [`RECORD_FLUSH`]; stops with a final note at [`RECORD_LIMIT`].
struct Tap {
    path: PathBuf,
    name: String,
    file: Option<BufWriter<std::fs::File>>,
    /// Bytes in the file.
    written: u64,
    limit: u64,
    last_ms: u64,
    size: u32,
}

impl Tap {
    /// The tap's channel when `RECORD_ENV` names a directory.
    fn start(terminal: &Terminal) -> Option<Sender<TapChunk>> {
        let dir = PathBuf::from(canvas_common::env::var_os(RECORD_ENV).filter(|dir| !dir.is_empty())?);
        let tap = Tap::new(&dir, &terminal.id, terminal.started_ms, RECORD_LIMIT);
        let (tx, rx) = bounded::<TapChunk>(OUTPUT_QUEUE);
        std::thread::Builder::new().name("pty-tap".into()).spawn(move || tap.run(rx)).ok()?;
        Some(tx)
    }

    fn new(dir: &Path, id: &str, started_ms: u64, limit: u64) -> Self {
        Self { path: recording_path(dir, id), name: recording_stem(id), file: None, written: 0, limit, last_ms: started_ms, size: 0 }
    }

    /// Until the reader ends or recording stops; dropping the writer flushes it.
    fn run(mut self, chunks: Receiver<TapChunk>) {
        loop {
            match chunks.recv_timeout(RECORD_FLUSH) {
                Ok(chunk) => {
                    if !self.write(&chunk) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.file.as_mut().is_some_and(|file| file.flush().is_err()) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// `false` when recording stops (an I/O error or the size limit); the terminal carries on.
    fn write(&mut self, chunk: &TapChunk) -> bool {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut line = String::with_capacity(chunk.bytes.len() * 2 + 160);
        if self.file.is_none() {
            let opened = self
                .path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&self.path));
            let Ok(file) = opened else { return false };
            self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
            if self.written >= self.limit {
                return false;
            }
            self.file = Some(BufWriter::with_capacity(256 * 1024, file));
            self.size = chunk.size;
            line.push_str(&format!(
                "{{\"vtrec\":1,\"cols\":{},\"rows\":{},\"program\":\"{name}\",\"description\":\"live terminal {name}\",\"source\":\"service\",\"recorded_at\":\"{}\"}}\n",
                chunk.size >> 16,
                chunk.size & 0xffff,
                crate::paths::rfc3339_now(),
                name = self.name,
            ));
        }
        let mut delta = chunk.ms.saturating_sub(self.last_ms);
        self.last_ms = chunk.ms.max(self.last_ms);
        if chunk.size != self.size {
            self.size = chunk.size;
            line.push_str(&format!("{{\"t\":{delta},\"resize\":[{},{}]}}\n", chunk.size >> 16, chunk.size & 0xffff));
            delta = 0;
        }
        line.push_str(&format!("{{\"t\":{delta},\"x\":\""));
        for &b in &chunk.bytes {
            line.push(HEX[(b >> 4) as usize] as char);
            line.push(HEX[(b & 15) as usize] as char);
        }
        line.push_str("\"}\n");
        let Some(file) = self.file.as_mut() else { return false };
        if self.written.saturating_add(line.len() as u64) > self.limit {
            // Readers skip lines of unknown kinds; this one explains the end.
            let note = format!("{{\"t\":{delta},\"note\":\"recording stopped: the file reached its {} byte limit\"}}\n", self.limit);
            let _ = file.write_all(note.as_bytes()).and_then(|_| file.flush());
            return false;
        }
        if file.write_all(line.as_bytes()).is_err() {
            return false;
        }
        self.written += line.len() as u64;
        true
    }
}

fn spawn_waiter(terminal: Arc<Terminal>, mut child: Box<dyn portable_pty::Child + Send + Sync>, exited: Sender<String>, pump: Sender<()>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-waiter".into())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code(),
                Err(error) => {
                    *lock(&terminal.error) = Some(error.to_string());
                    1
                }
            };
            *lock(&terminal.raw_exit_code) = Some(code);
            // An agent the canvas stopped is stopped, not failed, even when its
            // shutdown returns an error code (OpenCode exits with 1, or hangs,
            // when it is asked to quit while its own requests are pending).
            let requested = terminal.stopping.load(Ordering::Acquire);
            *lock(&terminal.exit_code) = Some(if requested { 0 } else { code });
            // Let the last output drain before closing the pseudoconsole.
            std::thread::sleep(Duration::from_millis(150));
            terminal.alive.store(false, Ordering::Release);
            // What an agent left (R2-B-01): its own program ends now; what is
            // still attached to its terminal gets a moment, with the
            // pseudoconsole open (closing it ends what is attached at once).
            let lingering = if terminal.copilot { terminal.end_own_leftovers() } else { Vec::new() };
            if lingering.is_empty() {
                terminal.close_pty();
            }
            terminal.generation.fetch_add(1, Ordering::AcqRel);
            let _ = pump.try_send(());
            let _ = exited.send(terminal.id.clone());
            if !lingering.is_empty() {
                end_lingering(lingering, LEFTOVER_GRACE);
                terminal.close_pty();
            }
            // Nothing more for the program: the writer thread ends, and the
            // engine freezes once the last output is in (S3-06).
            lock(&terminal.input).take();
            terminal.ended.store(true, Ordering::Release);
            let _ = terminal.control.try_send(Ctl::Exited);
        })
        .map(|_| ())
}

/// Waits up to `grace` for `processes` (pid, start time) to end by
/// themselves, then ends those still running.
fn end_lingering(processes: Vec<(u32, u64)>, grace: Duration) {
    let running = |list: &[(u32, u64)]| -> Vec<(u32, u64)> { list.iter().filter(|(pid, started)| canvas_sys::process::process_start_ms(*pid) == Some(*started)).copied().collect() };
    let deadline = Instant::now() + grace;
    let mut left = running(&processes);
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        left = running(&left);
    }
    for (pid, started) in left {
        canvas_sys::process::kill_started(pid, started);
    }
}

pub fn row_hash<T: Hash>(runs: &[T]) -> u64 {
    let mut hasher = DefaultHasher::new();
    runs.hash(&mut hasher);
    hasher.finish()
}

/// A standalone alacritty emulator (tests, the self-test and the corpus).
pub fn headless(cols: u16, rows: u16, kitty_keyboard: bool) -> emulator::alacritty::AlacrittyEmulator {
    emulator::alacritty::AlacrittyEmulator::new(&Config { cols, rows, scrollback: 1000, kitty_keyboard, pull_scrollback: emulator::PTY_PULLS_SCROLLBACK })
}

/// Plain text of the visible screen, as protocol-7 clients lay it out.
pub fn screen_text(snapshot: &Snapshot) -> String {
    (0..snapshot.lines.len())
        .map(|y| {
            let mut line = String::new();
            for run in snapshot.row_v7(y) {
                let width: usize = line.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)).sum();
                if (run.col as usize) > width {
                    line.push_str(&" ".repeat(run.col as usize - width));
                }
                line.push_str(&run.text);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Protocol-7 rows of a snapshot.
pub fn rows_v7(snapshot: &Snapshot) -> Vec<Vec<Run>> {
    (0..snapshot.lines.len()).map(|y| snapshot.row_v7(y)).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// What a [`Probe`] engine did.
    #[derive(Clone, Default)]
    struct ProbeLog {
        fed: Arc<AtomicUsize>,
        snapshots: Arc<AtomicUsize>,
        end_syncs: Arc<AtomicUsize>,
        /// Feeds (with the size they were fed at) and resizes, in order.
        events: Arc<Mutex<Vec<String>>>,
    }

    /// A test engine: slow on demand, panics on `b"panic"`, starts a
    /// synchronized update on `ESC [?2026h` and never ends it by itself.
    struct Probe {
        log: ProbeLog,
        feed_delay: Duration,
        snapshot_delay: Duration,
        cols: u16,
        rows: u16,
        sync: Option<Instant>,
        /// An image placed at the top left as image 7.
        image: Option<ImageData>,
    }

    impl Probe {
        fn new(log: &ProbeLog) -> Self {
            Self { log: log.clone(), feed_delay: Duration::ZERO, snapshot_delay: Duration::ZERO, cols: 80, rows: 24, sync: None, image: None }
        }
    }

    impl Emulator for Probe {
        fn engine(&self) -> Engine {
            Engine::Alacritty
        }
        fn feed(&mut self, bytes: &[u8], _: &mut dyn Events) {
            assert_ne!(bytes, b"panic", "injected failure");
            if bytes.starts_with(b"\x1b[?2026h") && self.sync.is_none() {
                self.sync = Some(Instant::now() + Duration::from_millis(50));
            }
            std::thread::sleep(self.feed_delay);
            lock(&self.log.events).push(format!("{} at {}x{}", String::from_utf8_lossy(bytes), self.cols, self.rows));
            self.log.fed.fetch_add(1, Ordering::SeqCst);
        }
        fn resize(&mut self, cols: u16, rows: u16) {
            (self.cols, self.rows) = (cols, rows);
            lock(&self.log.events).push(format!("resize {cols}x{rows}"));
        }
        fn snapshot(&self) -> Snapshot {
            std::thread::sleep(self.snapshot_delay);
            self.log.snapshots.fetch_add(1, Ordering::SeqCst);
            let images = self
                .image
                .iter()
                .map(|image| crate::emulator::SnapshotImage {
                    placement: crate::protocol::Placement { image_id: 7, cols: 1, rows: 1, width: image.width, height: image.height, ..Default::default() },
                    generation: image.generation,
                })
                .collect();
            Snapshot { cols: self.cols, rows: self.rows, lines: vec![Vec::new(); self.rows as usize], images, ..Snapshot::default() }
        }
        fn image(&mut self, id: u32) -> Option<ImageData> {
            self.image.clone().filter(|_| id == 7)
        }
        fn scroll(&mut self, _: Scroll) {}
        fn display_offset(&self) -> u32 {
            0
        }
        fn modes(&self) -> u32 {
            0
        }
        fn encode(&mut self, input: &Input) -> Vec<u8> {
            match input {
                Input::Raw(bytes) => bytes.clone(),
                _ => Vec::new(),
            }
        }
        fn encode_wheel(&mut self, _: bool, _: u16, _: u16, _: u8) -> Option<Vec<u8>> {
            None
        }
        fn encode_click(&mut self, _: u8, _: u16, _: u16, _: u8) -> Option<Vec<u8>> {
            None
        }
        fn sync_deadline(&self) -> Option<Instant> {
            self.sync
        }
        fn end_sync(&mut self, _: &mut dyn Events) {
            self.sync = None;
            self.log.end_syncs.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// An emulator thread without a PTY: the test plays reader, writer and clients.
    struct Harness {
        control: Sender<Ctl>,
        output: Sender<Vec<u8>>,
        /// What the thread sends to the PTY writer.
        written: Receiver<Vec<u8>>,
        generation: Arc<AtomicU64>,
        failed: Receiver<String>,
        pending: Mutex<Option<Receiver<Snapshot>>>,
        resizes: Arc<Resizes>,
        _keep: (Receiver<(String, String)>, Receiver<String>, Receiver<()>),
    }

    impl Harness {
        fn new(probe: Probe) -> Self {
            let (cols, rows) = (probe.cols, probe.rows);
            Self::with(cols, rows, move || Box::new(probe))
        }

        /// The thread with any engine of `cols`×`rows`.
        fn with(cols: u16, rows: u16, make: impl FnOnce() -> Box<dyn Emulator> + Send + 'static) -> Self {
            let resizes = Arc::new(Resizes::default());
            let (control, control_rx) = bounded(CONTROL_QUEUE);
            let (output, output_rx) = bounded(OUTPUT_QUEUE);
            let (input, written) = bounded(4096);
            let (clipboard, clipboard_rx) = bounded(16);
            let (changed, changed_rx) = bounded(1024);
            let (pump, pump_rx) = bounded(1);
            let (fail_tx, failed) = bounded(1);
            let generation = Arc::new(AtomicU64::new(1));
            let fail: FailHook = Box::new(move |message| {
                let _ = fail_tx.send(message);
            });
            let thread = EmulatorThread {
                id: "probe".into(),
                cols,
                rows,
                control: control_rx,
                output: output_rx,
                generation: generation.clone(),
                state: Arc::new(State::default()),
                input,
                clipboard,
                changed,
                pump,
                fail: Some(fail),
                burst: 0,
                notify: None,
                bridge: InlineImages::default(),
                replies: true,
                remake: None,
                fed: 0,
                resizes: resizes.clone(),
                chunks: 0,
                exit: None,
                output_done: false,
            };
            spawn_emulator(thread, make).unwrap();
            Self { control, output, written, generation, failed, pending: Mutex::new(None), resizes, _keep: (clipboard_rx, changed_rx, pump_rx) }
        }

        /// A resize applied once `after` output chunks were fed (as [`Terminal::resize`] queues it).
        fn resize(&self, after: u64, cols: u16, rows: u16) {
            self.resizes.push(after, cols, rows);
            let _ = self.control.try_send(Ctl::Resized);
        }

        fn send(&self, ctl: Ctl) {
            send_control(&self.control, ctl, CONTROL_TIMEOUT).expect("control message queued");
        }

        fn snapshot(&self, wait: Duration) -> Option<Snapshot> {
            request_snapshot(&self.control, &self.pending, CONTROL_TIMEOUT, wait)
        }

        /// Floods the output channel for `period` (blocking while it is full).
        fn flood(&self, period: Duration) -> std::thread::JoinHandle<usize> {
            let output = self.output.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut sent = 0;
                while started.elapsed() < period && output.send(vec![b'x'; READ_CHUNK]).is_ok() {
                    sent += 1;
                }
                sent
            })
        }
    }

    #[test]
    fn input_is_handled_promptly_under_an_output_flood() {
        let _serial = serial();
        let log = ProbeLog::default();
        // 64 queued chunks of 20 ms each: input behind them would wait ~1.3 s.
        let harness = Harness::new(Probe { feed_delay: Duration::from_millis(20), ..Probe::new(&log) });
        let flood = harness.flood(Duration::from_secs(4));
        // A throttled machine may take longer than usual to fill the backlog.
        let filling = Instant::now();
        while !harness.output.is_full() && filling.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(harness.output.is_full(), "the output backlog is full");
        let mut worst = Duration::ZERO;
        for i in 0..10u8 {
            let sent = Instant::now();
            harness.send(Ctl::Input(Input::Raw(vec![i])));
            assert_eq!(harness.written.recv_timeout(Duration::from_secs(5)).expect("input reaches the writer"), [i]);
            worst = worst.max(sent.elapsed());
        }
        assert!(worst < Duration::from_millis(400), "input waited {worst:?} behind output");
        let snapshot_started = Instant::now();
        assert!(harness.snapshot(Duration::from_secs(5)).is_some());
        assert!(snapshot_started.elapsed() < Duration::from_millis(400), "snapshot waited behind output");
        assert!(flood.join().unwrap() > 64);
    }

    #[test]
    fn resizes_are_applied_in_order_under_a_flood() {
        let _serial = serial();
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { feed_delay: Duration::from_millis(5), ..Probe::new(&log) });
        let flood = harness.flood(Duration::from_millis(800));
        for i in 0..40u16 {
            harness.resize(0, 40 + i, 10 + i % 7);
        }
        let snap = harness.snapshot(Duration::from_secs(5)).expect("snapshot");
        assert_eq!((snap.cols, snap.rows), (79, 10 + 39 % 7));
        flood.join().unwrap();
    }

    fn wait_fed(log: &ProbeLog, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.fed.load(Ordering::SeqCst) < count {
            assert!(Instant::now() < deadline, "{} of {count} chunks fed", log.fed.load(Ordering::SeqCst));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// S3-03: output read before the PTY was resized is fed at the old size,
    /// even when it still waits in the queue (control messages go first);
    /// output read after it at the new size.
    #[test]
    fn a_resize_takes_its_place_in_the_output_stream() {
        let _serial = serial();
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { feed_delay: Duration::from_millis(30), ..Probe::new(&log) });
        for chunk in ["one", "two", "three"] {
            harness.output.send(chunk.as_bytes().to_vec()).unwrap();
        }
        // The PTY is resized once three chunks were read; its repaint follows.
        harness.resize(3, 100, 30);
        harness.output.send(b"repaint".to_vec()).unwrap();
        wait_fed(&log, 4);
        assert_eq!(*lock(&log.events), ["one at 80x24", "two at 80x24", "three at 80x24", "resize 100x30", "repaint at 100x30"]);
        let snap = harness.snapshot(Duration::from_secs(5)).unwrap();
        assert_eq!((snap.cols, snap.rows), (100, 30));
    }

    /// The repaint is fed at the new size even when it overtakes the wake
    /// (its control message late, or the control queue full).
    #[test]
    fn output_read_after_a_resize_never_reaches_the_engine_at_the_old_size() {
        let log = ProbeLog::default();
        let harness = Harness::new(Probe::new(&log));
        harness.output.send(b"before".to_vec()).unwrap();
        wait_fed(&log, 1);
        harness.resizes.push(1, 60, 12);
        harness.output.send(b"repaint".to_vec()).unwrap();
        wait_fed(&log, 2);
        assert_eq!(*lock(&log.events), ["before at 80x24", "resize 60x12", "repaint at 60x12"]);
        // A resize undone before its place came costs nothing.
        harness.resizes.push(2, 90, 40);
        harness.resizes.push(2, 60, 12);
        harness.output.send(b"after".to_vec()).unwrap();
        wait_fed(&log, 3);
        assert_eq!(lock(&log.events).last().map(String::as_str), Some("after at 60x12"));
        assert_eq!(lock(&log.events).len(), 4, "no reflow to the size that was taken back");
    }

    /// The case S3-03 describes, with a real engine: a full-screen program
    /// (the alternate screen keeps no scrollback) grows from 10 to 20 rows
    /// and ConPTY repaints all 20 at once. Fed at the old size, the repaint's
    /// line feeds would scroll its first 10 rows away for good.
    #[test]
    fn a_growing_terminal_keeps_the_top_rows_of_its_repaint() {
        let harness = Harness::with(40, 10, || Box::new(headless(40, 10, true)));
        let old: Vec<String> = (1..=10).map(|i| format!("old {i}")).collect();
        harness.output.send(format!("\x1b[?1049h\x1b[H{}", old.join("\r\n")).into_bytes()).unwrap();
        harness.resize(1, 40, 20);
        let rows: Vec<String> = (1..=20).map(|i| format!("row {i}\x1b[K")).collect();
        harness.output.send(format!("\x1b[?25l\x1b[8;20;40t\x1b[H{}", rows.join("\r\n")).into_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let snap = loop {
            let snap = harness.snapshot(Duration::from_secs(5)).unwrap();
            if snap.text().contains("row 20") {
                break snap;
            }
            assert!(Instant::now() < deadline, "the repaint was fed: {}", snap.text());
            std::thread::sleep(Duration::from_millis(10));
        };
        let text = snap.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!((snap.cols, snap.rows), (40, 20));
        assert_eq!(lines.first().map(|l| l.trim_end()), Some("row 1"), "{text}");
        assert_eq!(lines.get(19).map(|l| l.trim_end()), Some("row 20"), "{text}");
    }

    /// A resize never waits for the engine: with its control queue full it
    /// is still applied once the engine gets to it.
    #[test]
    fn a_resize_is_queued_without_waiting_when_the_control_queue_is_full() {
        let _serial = serial();
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { snapshot_delay: Duration::from_millis(400), ..Probe::new(&log) });
        // The engine is busy with a snapshot; fill its control queue behind it.
        let (tx, _rx) = bounded(1);
        harness.send(Ctl::Snapshot(tx));
        std::thread::sleep(Duration::from_millis(50));
        while harness.control.try_send(Ctl::Scroll(Scroll::Bottom)).is_ok() {}
        let started = Instant::now();
        harness.resize(0, 70, 20);
        assert!(started.elapsed() < Duration::from_millis(50), "waited {:?}", started.elapsed());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lock(&log.events).iter().any(|e| e == "resize 70x20") {
            assert!(Instant::now() < deadline, "the resize was applied");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_synchronized_update_times_out_under_continuous_input_and_output() {
        let _serial = serial();
        let log = ProbeLog::default();
        let harness = Harness::new(Probe::new(&log));
        harness.output.send(b"\x1b[?2026h".to_vec()).unwrap();
        let flood = harness.flood(Duration::from_millis(600));
        let started = Instant::now();
        let mut i = 0u8;
        while started.elapsed() < Duration::from_millis(600) {
            harness.send(Ctl::Input(Input::Raw(vec![i])));
            i = i.wrapping_add(1);
        }
        flood.join().unwrap();
        assert_eq!(log.end_syncs.load(Ordering::SeqCst), 1, "the 50 ms hold ended while messages kept coming");
        // And with nothing else to do, the deadline alone ends a hold (waited
        // for: a busy runner may take longer than the 50 ms to get there).
        harness.output.send(b"\x1b[?2026h".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.end_syncs.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(log.end_syncs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_engine_panic_is_reported_and_snapshots_keep_working() {
        let log = ProbeLog::default();
        let harness = Harness::new(Probe::new(&log));
        harness.resize(0, 50, 12);
        harness.output.send(b"hello".to_vec()).unwrap();
        let before = harness.generation.load(Ordering::SeqCst);
        harness.output.send(b"panic".to_vec()).unwrap();
        let message = harness.failed.recv_timeout(Duration::from_secs(5)).expect("failure reported");
        assert!(message.starts_with("terminal emulator failed:") && message.contains("injected failure"), "{message}");
        assert!(harness.generation.load(Ordering::SeqCst) > before, "clients refresh");
        // The reader learns that nothing consumes output any more.
        assert!(harness.output.send(b"more".to_vec()).is_err());
        let snap = harness.snapshot(Duration::from_secs(5)).expect("snapshots are still answered");
        assert_eq!((snap.cols, snap.rows), (50, 12), "the screen as the engine left it");
        harness.send(Ctl::Input(Input::Raw(b"x".to_vec())));
        assert!(harness.snapshot(Duration::from_secs(5)).is_some());
        assert!(harness.written.try_recv().is_err(), "a failed engine sends nothing to the program");
    }

    #[test]
    fn an_engine_that_cannot_start_is_reported() {
        let (control, control_rx) = bounded(4);
        let (_output, output_rx) = bounded(4);
        let (fail_tx, failed) = bounded(1);
        let thread = EmulatorThread {
            id: "broken".into(),
            cols: 30,
            rows: 6,
            control: control_rx,
            output: output_rx,
            generation: Arc::new(AtomicU64::new(1)),
            state: Arc::new(State::default()),
            input: bounded(1).0,
            clipboard: bounded(1).0,
            changed: bounded(1).0,
            pump: bounded(1).0,
            fail: Some(Box::new(move |message| {
                let _ = fail_tx.send(message);
            })),
            burst: 0,
            notify: None,
            bridge: InlineImages::default(),
            replies: true,
            remake: None,
            fed: 0,
            resizes: Arc::default(),
            chunks: 0,
            exit: None,
            output_done: false,
        };
        spawn_emulator(thread, || panic!("no engine")).unwrap();
        assert!(failed.recv_timeout(Duration::from_secs(5)).unwrap().contains("no engine"));
        let snap = request_snapshot(&control, &Mutex::new(None), CONTROL_TIMEOUT, Duration::from_secs(5)).expect("blank screen");
        assert_eq!((snap.cols, snap.rows, snap.lines.len()), (30, 6, 6));
    }

    #[test]
    fn at_most_one_snapshot_request_is_outstanding() {
        let _serial = serial();
        let log = ProbeLog::default();
        // A slow engine (2 s per snapshot): a throttled machine may take far
        // longer than 10 ms per short wait, never 2 s for five of them.
        let harness = Harness::new(Probe { snapshot_delay: Duration::from_millis(2_000), ..Probe::new(&log) });
        for _ in 0..5 {
            assert!(harness.snapshot(Duration::from_millis(10)).is_none());
        }
        std::thread::sleep(Duration::from_millis(2_500));
        assert_eq!(log.snapshots.load(Ordering::SeqCst), 1, "timed-out requests do not pile up");
        // The stale answer is replaced by a fresh request.
        assert!(harness.snapshot(Duration::from_secs(5)).is_some());
        assert_eq!(log.snapshots.load(Ordering::SeqCst), 2);
    }

    /// Input never waits on a busy engine: a full control queue refuses a
    /// keystroke at once, saying why, instead of holding the client's reader.
    #[test]
    fn input_for_a_busy_engine_is_refused_at_once_with_the_reason() {
        let _serial = serial();
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { snapshot_delay: Duration::from_millis(600), ..Probe::new(&log) });
        let (tx, _rx) = bounded(1);
        harness.send(Ctl::Snapshot(tx));
        std::thread::sleep(Duration::from_millis(50));
        while harness.control.try_send(Ctl::Resized).is_ok() {}
        let started = Instant::now();
        let error = offer(&harness.control, Ctl::Input(Input::Raw(b"x".to_vec())), "probe").unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_millis(50), "waited {:?}", started.elapsed());
        assert!(error.contains("terminal probe is not responding") && error.contains(&CONTROL_QUEUE.to_string()), "{error}");
        // Once the engine caught up, input goes through again.
        let deadline = Instant::now() + Duration::from_secs(10);
        while offer(&harness.control, Ctl::Input(Input::Raw(b"y".to_vec())), "probe").is_err() {
            assert!(Instant::now() < deadline, "the queue drains");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(harness.written.recv_timeout(Duration::from_secs(5)).unwrap(), b"y");
    }

    /// S3-06: once the program is over and its last output is in, the engine
    /// is frozen into rows and its thread ends (its input sender goes, so the
    /// PTY writer ends too); the screen and the scrollback stay scrollable.
    #[test]
    fn an_exited_terminals_engine_is_frozen_once_its_output_is_in() {
        let (control, control_rx) = bounded(CONTROL_QUEUE);
        let (output, output_rx) = bounded(OUTPUT_QUEUE);
        let (input, written) = bounded::<Vec<u8>>(16);
        let ended = Arc::new(AtomicBool::new(false));
        let kept: Arc<Mutex<Option<Arc<Mutex<Frozen>>>>> = Arc::default();
        let keep = kept.clone();
        let thread = EmulatorThread {
            id: "exits".into(),
            cols: 30,
            rows: 5,
            control: control_rx,
            output: output_rx,
            generation: Arc::new(AtomicU64::new(1)),
            state: Arc::new(State::default()),
            input,
            clipboard: bounded(16).0,
            changed: bounded(1024).0,
            pump: bounded(1).0,
            fail: None,
            burst: 0,
            notify: None,
            bridge: InlineImages::default(),
            replies: true,
            remake: None,
            fed: 0,
            resizes: Arc::default(),
            chunks: 0,
            exit: Some(ExitHook { ended: ended.clone(), freeze: Box::new(move |frozen| *lock(&keep) = Some(frozen)) }),
            output_done: false,
        };
        spawn_emulator(thread, || Box::new(headless(30, 5, true))).unwrap();
        let lines: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        output.send(lines.join("\r\n").into_bytes()).unwrap();
        // The program exited, but its last output may still come: not yet.
        ended.store(true, Ordering::Release);
        control.send(Ctl::Exited).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(lock(&kept).is_none(), "frozen before the output was in");
        output.send(b"\r\nthe end".to_vec()).unwrap();
        drop(output);
        wait_until("frozen", || lock(&kept).is_some());
        wait_until("the thread ended", || matches!(control.try_send(Ctl::Resized), Err(TrySendError::Disconnected(_))));
        assert!(matches!(written.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Disconnected)), "the writer's input is gone");
        let frozen = lock(&kept).clone().unwrap();
        let mut frozen = lock(&frozen);
        assert!(frozen.snapshot().text().contains("the end"));
        assert_eq!(frozen.fed, lines.join("\r\n").len() as u64 + 9);
        frozen.scroll(Scroll::Top);
        assert_eq!(frozen.snapshot().text().lines().next(), Some("line 1"));
    }

    /// R2-B-06: the images on an exited terminal's screen are kept as PNG
    /// files when its engine freezes (their pixels go with it), so a client
    /// that shows it later (a reconnect) still gets them.
    #[test]
    fn an_exited_terminal_keeps_the_images_on_its_screen() {
        let (control, control_rx) = bounded(CONTROL_QUEUE);
        let (output, output_rx) = bounded::<Vec<u8>>(OUTPUT_QUEUE);
        let kept: Arc<Mutex<Option<Arc<Mutex<Frozen>>>>> = Arc::default();
        let keep = kept.clone();
        let log = ProbeLog::default();
        let probe = Probe { image: Some(ImageData { width: 2, height: 2, generation: 3, rgba: vec![255; 16] }), ..Probe::new(&log) };
        let thread = EmulatorThread {
            id: "pictures".into(),
            cols: 80,
            rows: 24,
            control: control_rx,
            output: output_rx,
            generation: Arc::new(AtomicU64::new(1)),
            state: Arc::new(State::default()),
            input: bounded(16).0,
            clipboard: bounded(16).0,
            changed: bounded(1024).0,
            pump: bounded(1).0,
            fail: None,
            burst: 0,
            notify: None,
            bridge: InlineImages::default(),
            replies: true,
            remake: None,
            fed: 0,
            resizes: Arc::default(),
            chunks: 0,
            exit: Some(ExitHook { ended: Arc::new(AtomicBool::new(true)), freeze: Box::new(move |frozen| *lock(&keep) = Some(frozen)) }),
            output_done: false,
        };
        spawn_emulator(thread, move || Box::new(probe)).unwrap();
        drop(output);
        control.send(Ctl::Exited).unwrap();
        wait_until("frozen", || lock(&kept).is_some());
        let frozen = lock(&kept).clone().unwrap();
        let frozen = lock(&frozen);
        assert_eq!(frozen.snapshot().images.len(), 1, "the placement stays");
        match frozen_image(&frozen, 7, 3) {
            ImagePng::Ready(image) => assert!(image.png.starts_with(b"\x89PNG") && (image.width, image.height, image.generation) == (2, 2, 3)),
            _ => panic!("the image is kept"),
        }
        assert!(matches!(frozen_image(&frozen, 8, 1), ImagePng::Unavailable), "one never placed is not");
    }

    #[test]
    fn the_thread_ends_with_the_terminal_and_survives_the_reader() {
        let log = ProbeLog::default();
        let harness = Harness::new(Probe::new(&log));
        harness.output.send(b"a".to_vec()).unwrap();
        let Harness { control, output, written, pending, .. } = harness;
        drop(output);
        // The reader is gone; control still works.
        assert!(request_snapshot(&control, &pending, CONTROL_TIMEOUT, Duration::from_secs(5)).is_some());
        drop(control);
        // The thread exits and drops its writer end.
        let started = Instant::now();
        while !matches!(written.recv_timeout(Duration::from_millis(50)), Err(RecvTimeoutError::Disconnected)) {
            assert!(started.elapsed() < Duration::from_secs(5), "emulator thread still running");
        }
    }

    /// Timing tests (stop timelines, output floods that keep cores busy) run
    /// one at a time: in parallel on a small CI machine they starve each
    /// other's 100 ms ticks.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hooks() -> (Hooks, Vec<Box<dyn Any>>) {
        let (pump, a) = bounded::<()>(1);
        let (exited, b) = bounded::<String>(16);
        let (clipboard, c) = bounded::<(String, String)>(16);
        let (changed, d) = bounded::<String>(1024);
        (Hooks { pump, exited, clipboard, bytes: Arc::new(AtomicU64::new(0)), changed, notify: None, raw: None }, vec![Box::new(a), Box::new(b), Box::new(c), Box::new(d)])
    }

    fn wait_exit(terminal: &Terminal, within: Duration) -> (Option<u32>, Option<u32>) {
        let started = Instant::now();
        while terminal.alive.load(Ordering::Acquire) && started.elapsed() < within {
            std::thread::sleep(Duration::from_millis(20));
        }
        (*lock(&terminal.exit_code), *lock(&terminal.raw_exit_code))
    }

    #[test]
    #[cfg(windows)]
    fn agents_the_canvas_stops_are_stopped_whatever_their_exit_code() {
        let _serial = serial();
        let cwd = std::env::temp_dir().display().to_string();
        // Quits with code 3 on the first key (the stop's Ctrl+D).
        let (hooks_a, _keep_a) = hooks();
        let args = ["/d".to_owned(), "/c".to_owned(), "pause >nul & exit 3".to_owned()];
        let spec = Spawn { id: "quits", program: "cmd.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: true, engine: Engine::Alacritty };
        let quits = Terminal::spawn(spec, hooks_a).expect("spawn cmd.exe");
        std::thread::sleep(Duration::from_millis(500));
        quits.stop(false);
        assert_eq!(wait_exit(&quits, Duration::from_secs(4)), (Some(0), Some(3)), "quit on request: reported as a clean stop");
        assert!(!quits.was_forced());
        // Stuck: ignores Ctrl+D but ends on the stop's Ctrl+C (ping handles the console event).
        let (hooks_b, _keep_b) = hooks();
        let args = ["-n".to_owned(), "30".to_owned(), "127.0.0.1".to_owned()];
        let spec = Spawn { id: "stuck", program: "ping.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: true, engine: Engine::Alacritty };
        let stuck = Terminal::spawn(spec, hooks_b).expect("spawn ping.exe");
        std::thread::sleep(Duration::from_millis(300));
        let asked = Instant::now();
        stuck.stop(false);
        let (code, raw) = wait_exit(&stuck, Duration::from_secs(15));
        assert_eq!(code, Some(0), "stopped by the canvas: not a failure ({raw:?})");
        assert!(!stuck.was_forced() && asked.elapsed() < Duration::from_millis(4_900), "Ctrl+C ended it before the kill");
        // Ignores everything: killed when the grace period runs out, still reported as stopped.
        let (hooks_c, _keep_c) = hooks();
        let args = ["-NoProfile".to_owned(), "-Command".to_owned(), "[Console]::TreatControlCAsInput = $true; Start-Sleep 30".to_owned()];
        let spec = Spawn { id: "ignores", program: "powershell.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: true, engine: Engine::Alacritty };
        let ignores = Terminal::spawn(spec, hooks_c).expect("spawn powershell.exe");
        std::thread::sleep(Duration::from_millis(1_500));
        ignores.stop(false);
        let (code, raw) = wait_exit(&ignores, Duration::from_secs(15));
        assert!(ignores.was_forced() && code == Some(0) && raw.is_some_and(|c| c != 0), "killed after the grace period: {code:?} {raw:?}");
    }

    #[test]
    #[cfg(windows)]
    fn an_agent_whose_ui_quit_but_whose_process_lingers_is_ended_promptly() {
        let _serial = serial();
        let cwd = std::env::temp_dir().display().to_string();
        let (hooks, _keep) = hooks();
        // Full-screen view; the first key leaves it, then the process lingers.
        let script = "$e=[char]27; [Console]::Write(\"$e[?1049hfull screen\"); [Console]::TreatControlCAsInput = $true; \
                      $null = [Console]::ReadKey($true); [Console]::Write(\"$e[?1049l\"); Start-Sleep 30";
        let args = ["-NoProfile".to_owned(), "-Command".to_owned(), script.to_owned()];
        let spec = Spawn { id: "lingers", program: "powershell.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: true, engine: Engine::Alacritty };
        let lingers = Terminal::spawn(spec, hooks).expect("spawn powershell.exe");
        let started = Instant::now();
        while !lingers.state.alt_screen.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(lingers.state.alt_screen.load(Ordering::Acquire), "the program entered its full-screen view");
        let asked = Instant::now();
        lingers.stop(false);
        let (code, _) = wait_exit(&lingers, Duration::from_secs(10));
        assert_eq!(code, Some(0), "stopped, not failed");
        assert!(lingers.ui_quit_lingered(), "ended because its UI had quit");
        assert!(asked.elapsed() < Duration::from_millis(4_900), "ended 3 s after its UI quit, before the grace period ran out: {:?}", asked.elapsed());
    }

    #[test]
    #[cfg(windows)]
    fn conpty_resizes_are_never_lost_while_output_floods() {
        let _serial = serial();
        let (hooks, _keep) = hooks();
        let cwd = std::env::temp_dir().display().to_string();
        let args = ["/d".to_owned(), "/c".to_owned(), "for /l %i in (1,1,200000) do @echo 0123456789abcdefghijklmnopqrstuvwxyz 0123456789".to_owned()];
        let spec = Spawn { id: "flood", program: "cmd.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: false, engine: Engine::Alacritty };
        let terminal = Terminal::spawn(spec, hooks).expect("spawn cmd.exe");
        let started = Instant::now();
        while terminal.last_output_ms.load(Ordering::Relaxed) == 0 && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        for i in 0..30u16 {
            terminal.resize(40 + i, 10 + i % 7).expect("resize under flood");
        }
        terminal.resize(69, 10 + 29 % 7).expect("same size again");
        // The engine takes the size once it reaches the resize's place in the output.
        wait_until("the last size reached the engine", || terminal.snapshot().is_some_and(|snap| (snap.cols, snap.rows) == (69, 10 + 29 % 7)));
        terminal.force_kill();
    }

    // Unix twins of the three ConPTY tests above: a PTY session, POSIX
    // programs, the same stop rules (MAC-LINUX-PLAN §5.2).
    #[cfg(unix)]
    fn sh(id: &str, script: &str, copilot: bool) -> Arc<Terminal> {
        let cwd = std::env::temp_dir().display().to_string();
        let (hooks, keep) = hooks();
        std::mem::forget(keep);
        let args = ["-c".to_owned(), script.to_owned()];
        let spec = Spawn { id, program: "/bin/sh", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot, engine: Engine::Alacritty };
        Terminal::spawn(spec, hooks).expect("spawn /bin/sh")
    }

    #[test]
    #[cfg(unix)]
    fn agents_the_canvas_stops_are_stopped_whatever_their_exit_code_on_unix() {
        let _serial = serial();
        // Quits with code 3 on the first key (the stop's Ctrl+D ends the read).
        let quits = sh("quits", "read line; exit 3", true);
        std::thread::sleep(Duration::from_millis(500));
        quits.stop(false);
        assert_eq!(wait_exit(&quits, Duration::from_secs(4)), (Some(0), Some(3)), "quit on request: reported as a clean stop");
        assert!(!quits.was_forced());
        // Stuck: ignores Ctrl+D but ends on the stop's Ctrl+C (SIGINT to the foreground group).
        let stuck = sh("stuck", "exec sleep 30", true);
        std::thread::sleep(Duration::from_millis(300));
        let asked = Instant::now();
        stuck.stop(false);
        let (code, raw) = wait_exit(&stuck, Duration::from_secs(15));
        assert_eq!(code, Some(0), "stopped by the canvas: not a failure ({raw:?})");
        assert!(!stuck.was_forced() && asked.elapsed() < Duration::from_millis(4_900), "Ctrl+C ended it before the kill");
        // Ignores everything: killed when the grace period runs out, still reported as stopped.
        let ignores = sh("ignores", "trap '' INT HUP; sleep 30", true);
        std::thread::sleep(Duration::from_millis(500));
        ignores.stop(false);
        let (code, raw) = wait_exit(&ignores, Duration::from_secs(15));
        assert!(ignores.was_forced() && code == Some(0) && raw.is_some_and(|c| c != 0), "killed after the grace period: {code:?} {raw:?}");
    }

    /// An agent that ignores Ctrl+D (its keys taken by a prompt) and quits on
    /// a second Ctrl+C (Copilot CLI's "press twice to exit"), or only on its
    /// `/exit` command: the stop reaches it before the kill.
    #[test]
    #[cfg(unix)]
    fn the_graceful_stop_reaches_an_agent_that_quits_on_two_ctrl_c_or_on_exit() {
        let _serial = serial();
        let twice = sh("twice", "stty raw -echo; n=0; while :; do c=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' '); [ \"$c\" = 03 ] && n=$((n+1)); [ $n -ge 2 ] && exit 5; done", true);
        let command = sh("command", "stty raw -echo; buf=; while :; do c=$(dd bs=1 count=1 2>/dev/null); buf=\"$buf$c\"; case \"$buf\" in *'/exit'*) exit 6;; esac; done", true);
        std::thread::sleep(Duration::from_millis(500));
        let asked = Instant::now();
        twice.stop(false);
        command.stop(false);
        let (code, raw) = wait_exit(&twice, Duration::from_secs(15));
        assert!(!twice.was_forced() && code == Some(0) && raw == Some(5), "the second Ctrl+C ended it: {code:?} {raw:?}");
        let (code, raw) = wait_exit(&command, Duration::from_secs(15));
        assert!(!command.was_forced() && code == Some(0) && raw == Some(6), "/exit ended it: {code:?} {raw:?}");
        assert!(asked.elapsed() < STOP_GRACE, "both before the kill");
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_whose_ui_quit_but_whose_process_lingers_is_ended_promptly_on_unix() {
        let _serial = serial();
        // Full-screen view; the first key leaves it, then the process lingers.
        let script = "printf '\\033[?1049hfull screen'; stty raw -echo; dd bs=1 count=1 >/dev/null 2>&1; printf '\\033[?1049l'; trap '' INT HUP; sleep 30";
        let lingers = sh("lingers", script, true);
        let started = Instant::now();
        while !lingers.state.alt_screen.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(lingers.state.alt_screen.load(Ordering::Acquire), "the program entered its full-screen view");
        std::thread::sleep(Duration::from_millis(200));
        let asked = Instant::now();
        lingers.stop(false);
        let (code, _) = wait_exit(&lingers, Duration::from_secs(10));
        assert_eq!(code, Some(0), "stopped, not failed");
        assert!(lingers.ui_quit_lingered(), "ended because its UI had quit");
        assert!(asked.elapsed() < Duration::from_millis(4_900), "ended 3 s after its UI quit, before the grace period ran out: {:?}", asked.elapsed());
    }

    #[test]
    #[cfg(unix)]
    fn pty_resizes_are_never_lost_while_output_floods() {
        let _serial = serial();
        let terminal = sh("flood", "yes 0123456789abcdefghijklmnopqrstuvwxyz 0123456789 | head -n 200000", false);
        let started = Instant::now();
        while terminal.last_output_ms.load(Ordering::Relaxed) == 0 && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        for i in 0..30u16 {
            terminal.resize(40 + i, 10 + i % 7).expect("resize under flood");
        }
        terminal.resize(69, 10 + 29 % 7).expect("same size again");
        // The engine takes the size once it reaches the resize's place in the output.
        wait_until("the last size reached the engine", || terminal.snapshot().is_some_and(|snap| (snap.cols, snap.rows) == (69, 10 + 29 % 7)));
        terminal.force_kill();
    }

    /// S3-06 with a real program: once it exited, its terminal answers from
    /// the frozen screen, the engine thread and the writer's input are gone,
    /// sizes are ignored and keys refused.
    #[test]
    fn an_exited_program_leaves_its_screen_and_no_engine() {
        let _serial = serial();
        let cwd = std::env::temp_dir().display().to_string();
        let (hooks, _keep) = hooks();
        let (program, args): (&str, Vec<String>) =
            if cfg!(windows) { ("cmd.exe", vec!["/d".into(), "/c".into(), "echo frozen-hi".into()]) } else { ("/bin/sh", vec!["-c".into(), "printf 'frozen-hi\\n'".into()]) };
        let terminal = Terminal::spawn(Spawn { id: "exits", program, args: &args, env: &[], cwd: &cwd, cols: 40, rows: 6, copilot: false, engine: Engine::Alacritty }, hooks).unwrap();
        wait_exit(&terminal, Duration::from_secs(10));
        wait_until("the engine froze", || terminal.is_frozen());
        assert!(terminal.snapshot().unwrap().text().contains("frozen-hi"));
        assert!(lock(&terminal.input).is_none(), "the writer's input is gone");
        wait_until("the engine thread ended", || matches!(terminal.control.try_send(Ctl::Resized), Err(TrySendError::Disconnected(_))));
        let screen = terminal.ask_snapshot().expect("the frame pump still gets the screen").try_recv().unwrap();
        assert!(screen.text().contains("frozen-hi"));
        terminal.resize(80, 20).unwrap();
        assert_eq!(terminal.snapshot().map(|s| (s.cols, s.rows)), Some((40, 6)), "a frozen screen keeps its size");
        let generation = terminal.generation();
        terminal.scroll(i32::MAX);
        assert!(terminal.generation() > generation, "scrolling a frozen screen shows");
        assert!(terminal.send_input(&Input::Raw(b"x".to_vec())).is_err());
        assert!(terminal.snapshot_at().is_some());
    }

    /// The numbers a test program printed after each of `labels` (`attached 12 …`).
    fn printed_pids(terminal: &Terminal, labels: &[&str]) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let text = terminal.snapshot().map(|s| s.text()).unwrap_or_default();
            let words: Vec<&str> = text.split_whitespace().collect();
            let found: Vec<u32> = labels.iter().filter_map(|label| words.windows(2).find(|w| w[0] == *label).and_then(|w| w[1].parse().ok())).collect();
            if found.len() == labels.len() {
                return found;
            }
            assert!(Instant::now() < deadline, "the helpers started: {text}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_long(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// R2-B-01 (S3-14 narrowed): an agent that exits on its own takes what
    /// is still in its terminal's session (a helper that ignores the
    /// hang-up), never what left it (a daemon in a session of its own, as a
    /// server the agent was asked to keep running); a plain terminal's are
    /// left alone.
    #[test]
    #[cfg(unix)]
    fn an_agent_that_exits_ends_what_is_attached_to_its_terminal_and_leaves_what_detached() {
        let _serial = serial();
        let script = r#"nohup sleep 30 >/dev/null 2>&1 & echo "attached $!"; if command -v setsid >/dev/null; then setsid sleep 30 </dev/null >/dev/null 2>&1 & else perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' sleep 30 </dev/null >/dev/null 2>&1 & fi; echo "detached $!"; read line"#;
        for agent in [true, false] {
            let terminal = sh(if agent { "agent-leaves" } else { "shell-leaves" }, script, agent);
            let pids = printed_pids(&terminal, &["attached", "detached"]);
            let (attached, detached) = (pids[0], pids[1]);
            wait_long("the daemon has a session of its own", || canvas_sys::process::console_of(detached) == Some(detached));
            let picked: Vec<u32> = terminal.leftovers().iter().map(|(pid, _, _)| *pid).collect();
            assert!(picked.contains(&attached) && !picked.contains(&detached), "attached {attached}, detached {detached}: {picked:?}");
            terminal.send_input(&Input::Raw(b"\r".to_vec())).unwrap();
            wait_exit(&terminal, Duration::from_secs(10));
            assert!(!terminal.was_forced(), "it exited on its own");
            if agent {
                wait_long("what stayed in the agent's session ended with it", || !canvas_sys::process::alive(attached));
            } else {
                std::thread::sleep(Duration::from_millis(300));
                assert!(canvas_sys::process::alive(attached), "a plain terminal's leftovers are left alone");
                let _ = canvas_sys::process::kill_tree(attached);
            }
            std::thread::sleep(Duration::from_millis(300));
            assert!(canvas_sys::process::alive(detached), "what left the terminal outlives it");
            let _ = canvas_sys::process::kill_tree(detached);
        }
    }

    /// R2-B-01 (S3-14 narrowed) on Windows: when an agent exits on its own,
    /// what is still attached to its pseudoconsole ends with it, and so does
    /// its own program in a console of its own (as the CLI a launcher starts
    /// holds the session's lock); a server it started with a console of its
    /// own (`Start-Process`) keeps running.
    #[test]
    #[cfg(windows)]
    fn an_agent_that_exits_ends_what_is_attached_to_its_terminal_and_leaves_what_detached() {
        let _serial = serial();
        let cwd = std::env::temp_dir().display().to_string();
        let (hooks, _keep) = hooks();
        let script = "$a = Start-Process -NoNewWindow -PassThru cmd.exe -ArgumentList '/d','/c','ping -n 30 127.0.0.1 >nul'; \
                      $d = Start-Process -WindowStyle Hidden -PassThru ping.exe -ArgumentList '-n','30','127.0.0.1'; \
                      $o = Start-Process -WindowStyle Hidden -PassThru powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep 30'; \
                      Write-Output \"attached $($a.Id) detached $($d.Id) own $($o.Id)\"; $null = [Console]::ReadKey($true)";
        let args = ["-NoProfile".to_owned(), "-Command".to_owned(), script.to_owned()];
        let spec = Spawn { id: "agent-leaves", program: "powershell.exe", args: &args, env: &[], cwd: &cwd, cols: 120, rows: 24, copilot: true, engine: Engine::Alacritty };
        let terminal = Terminal::spawn(spec, hooks).expect("spawn powershell.exe");
        let pids = printed_pids(&terminal, &["attached", "detached", "own"]);
        let (attached, detached, own) = (pids[0], pids[1], pids[2]);
        let console = canvas_sys::process::console_of(terminal.pid);
        let table = canvas_sys::process::process_entries();
        let describe = |pid: Option<u32>| table.iter().find(|(p, _, _)| Some(*p) == pid).map(|(p, parent, name)| format!("{name} {p} (parent {parent})"));
        let facts = format!(
            "this process {}; the pseudoconsole's host found at its opening: {:?}; the agent's console: {:?}; attached's: {:?}; detached's: {:?}",
            std::process::id(),
            describe(terminal.console_host.map(|(pid, _)| pid)),
            describe(console),
            describe(canvas_sys::process::console_of(attached)),
            describe(canvas_sys::process::console_of(detached)),
        );
        println!("{facts}");
        assert!(console.is_some(), "the agent has a console: {facts}");
        assert_eq!(canvas_sys::process::console_of(attached), console, "Start-Process -NoNewWindow stays in the terminal's console: {facts}");
        assert_ne!(canvas_sys::process::console_of(detached), console, "Start-Process gives a console of its own: {facts}");
        let picked = terminal.leftovers();
        assert!(picked.iter().any(|&(pid, _, mine)| pid == attached && !mine), "attached {attached}: {picked:?}");
        assert!(picked.iter().any(|&(pid, _, mine)| pid == own && mine), "its own program {own}: {picked:?}");
        assert!(!picked.iter().any(|&(pid, _, _)| pid == detached), "detached {detached}: {picked:?}");
        terminal.send_input(&Input::Raw(b"\r".to_vec())).unwrap();
        wait_exit(&terminal, Duration::from_secs(30));
        assert!(!terminal.was_forced(), "it exited on its own");
        wait_long("its own program ended with it", || !canvas_sys::process::alive(own));
        wait_long("what stayed in its console ended with it", || !canvas_sys::process::alive(attached));
        std::thread::sleep(Duration::from_millis(500));
        assert!(canvas_sys::process::alive(detached), "a server with a console of its own outlives the agent");
        let _ = canvas_sys::process::kill_tree(detached);
    }

    #[test]
    fn recording_files_are_collision_free_and_capped() {
        let dir = std::env::temp_dir().join(format!("cc-tap-{}-{}", std::process::id(), now_ms()));
        let (a, b) = (recording_path(&dir, "a/b"), recording_path(&dir, "a:b"));
        assert_ne!(a, b, "ids that sanitize alike get distinct files");
        assert_eq!(a, recording_path(&dir, "a/b"), "stable per id");
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("a_b-"));
        assert!(recording_path(&dir, &"x".repeat(500)).file_name().unwrap().len() < 100);

        let mut tap = Tap::new(&dir, "t1", 0, 4096);
        let chunk = |ms, size| TapChunk { ms, size, bytes: vec![b'z'; 500] };
        let mut accepted = 0;
        while tap.write(&chunk(accepted + 1, pack(80, 24))) {
            accepted += 1;
            assert!(accepted < 100, "the limit stops recording");
        }
        drop(tap);
        let text = std::fs::read_to_string(recording_path(&dir, "t1")).unwrap();
        assert!(text.len() as u64 <= 4096 + 200, "{} bytes", text.len());
        assert!(text.starts_with("{\"vtrec\":1,\"cols\":80,\"rows\":24,\"program\":\"t1\""));
        assert!(text.trim_end().lines().last().unwrap().contains("\"note\":\"recording stopped"));
        assert_eq!(text.lines().filter(|l| l.contains("\"x\":")).count() as u64, accepted);
        // The limit holds across runs that append to the same file.
        let mut again = Tap::new(&dir, "t1", 0, 4096);
        assert!(!again.write(&chunk(1, pack(80, 24))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A remote PTY that records what the terminal sends it.
    #[derive(Default)]
    struct FakeRemote {
        sent: Mutex<Vec<u8>>,
        sizes: Mutex<Vec<(u16, u16)>>,
        stops: Mutex<Vec<bool>>,
        offline: AtomicBool,
    }

    impl RemotePty for FakeRemote {
        fn accepting(&self) -> Result<()> {
            anyhow::ensure!(!self.offline.load(Ordering::Acquire), "VM offline: input not sent");
            Ok(())
        }
        fn input(&self, bytes: Vec<u8>) {
            lock(&self.sent).extend(bytes);
        }
        fn resize(&self, cols: u16, rows: u16) {
            lock(&self.sizes).push((cols, rows));
        }
        fn stop(&self, force: bool) {
            lock(&self.stops).push(force);
        }
    }

    fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_raw_tap_sees_every_byte_before_the_engine_and_snapshots_say_how_far_it_got() {
        let _serial = serial();
        let (pump, _pump_rx) = bounded(1);
        let (exited, exited_rx) = bounded(1);
        let (clipboard, _clip) = bounded(1);
        let (changed, _changed) = bounded(16);
        let (raw, raw_rx) = bounded::<Vec<u8>>(64);
        let hooks = Hooks { pump, exited, clipboard, bytes: Arc::default(), changed, notify: None, raw: Some(raw) };
        let (program, args): (&str, Vec<String>) = if cfg!(windows) { ("cmd", vec!["/d".into(), "/c".into(), "echo tapped-output".into()]) } else { ("sh", vec!["-c".into(), "printf 'tapped-output\\n'; sleep 1".into()]) };
        let cwd = std::env::temp_dir().display().to_string();
        let terminal = Terminal::spawn(Spawn { id: "tapped", program, args: &args, env: &[], cwd: &cwd, cols: 40, rows: 6, copilot: false, engine: Engine::Alacritty }, hooks).unwrap();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&seen).contains("tapped-output") {
            assert!(Instant::now() < deadline, "the tap saw {:?}", String::from_utf8_lossy(&seen));
            if let Ok(chunk) = raw_rx.recv_timeout(Duration::from_millis(100)) {
                seen.extend(chunk);
            }
        }
        wait_until("the engine took every tapped byte", || terminal.snapshot_at().is_some_and(|(s, fed)| fed >= seen.len() as u64 && s.text().contains("tapped-output")));
        let _ = exited_rx.recv_timeout(Duration::from_secs(10));
        // The reader may go on (ConPTY writes as it closes): every byte the engine
        // took went through the tap first, and once it is quiet they agree.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, fed) = terminal.snapshot_at().unwrap();
            while let Ok(chunk) = raw_rx.recv_timeout(Duration::from_millis(200)) {
                seen.extend(chunk);
            }
            assert!(seen.len() as u64 >= fed, "the tap is never behind the engine: {} < {fed}", seen.len());
            if terminal.snapshot_at().unwrap().1 == seen.len() as u64 {
                break;
            }
            assert!(Instant::now() < deadline, "the engine did not catch up with the tap");
        }
    }

    #[test]
    fn a_remote_terminal_draws_here_and_types_there() {
        let (pump, _pump_rx) = bounded(1);
        let (exited, exited_rx) = bounded(1);
        let (clipboard, _clip) = bounded(1);
        let (changed, _changed) = bounded(16);
        let remote = Arc::new(FakeRemote::default());
        let hooks = Hooks { pump, exited, clipboard, bytes: Arc::default(), changed, notify: None, raw: None };
        let (terminal, feed) = Terminal::spawn_remote(RemoteSpawn { id: "vm-box", cols: 40, rows: 6, copilot: true, engine: Engine::Alacritty }, hooks, remote.clone());
        assert!(terminal.is_remote() && terminal.pid == 0, "no process here: the process-tree loops skip it");
        // Output (with a query the guest answers itself: nothing goes back from here).
        assert!(feed.output(b"hello from the guest\x1b[6n\x1b[c".to_vec()));
        wait_until("the output is drawn", || terminal.snapshot().is_some_and(|s| s.text().contains("hello from the guest")));
        std::thread::sleep(Duration::from_millis(50));
        assert!(lock(&remote.sent).is_empty(), "queries are answered in the guest, not here");
        // Typing goes to the guest's PTY, encoded by this engine.
        terminal.send_input(&Input::Key { key: crate::protocol::Key::Char('x'), mods: 0 }).unwrap();
        wait_until("the key reaches the guest", || *lock(&remote.sent) == b"x");
        terminal.resize(50, 8).unwrap();
        assert_eq!(*lock(&remote.sizes), [(50, 8)]);
        // Offline: refused with the reason, never queued.
        remote.offline.store(true, Ordering::Release);
        assert!(terminal.send_input(&Input::Paste("lost".into())).unwrap_err().to_string().contains("VM offline"));
        remote.offline.store(false, Ordering::Release);
        // A gap reset: the screen the guest's engine has.
        assert!(feed.reset(b"\x1bc\x1b[2;3Hafter the gap".to_vec()));
        wait_until("the screen is reset", || terminal.snapshot().is_some_and(|s| s.text().contains("after the gap") && !s.text().contains("hello")));
        assert!(feed.note("output skipped while offline"));
        wait_until("the note is drawn", || terminal.snapshot().is_some_and(|s| s.text().contains("output skipped while offline")));
        // A forced stop goes to the guest; the guest's exit ends it here.
        terminal.force_kill();
        assert_eq!(*lock(&remote.stops), [true]);
        feed.exited(137, None);
        assert_eq!(exited_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "vm-box");
        assert!(!terminal.alive.load(Ordering::Acquire) && *lock(&terminal.raw_exit_code) == Some(137));
        feed.exited(0, None);
        assert!(exited_rx.recv_timeout(Duration::from_millis(100)).is_err(), "an exit is reported once");
    }
}
