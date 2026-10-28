//! One hosted terminal: a ConPTY child, its emulator and its I/O threads.
//!
//! The emulator lives on a dedicated thread (engines need not be `Send`).
//! Clients and the frame pump reach it through a control channel (input,
//! clicks, wheel, resize, scroll, snapshots); the PTY reader feeds it through
//! a separate output channel. The thread handles waiting control messages
//! before the next output chunk (one chunk still gets its turn after a long
//! burst), so keystrokes and snapshots never queue behind a flood of output.
//! The output channel is bounded in bytes
//! ([`OUTPUT_QUEUE`] chunks of at most [`READ_CHUNK`] bytes): when the
//! emulator falls behind, the reader blocks and ConPTY slows the program.

use std::any::Any;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, at, bounded, never, select_biased};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::emulator::{self, Config, Emulator, Engine, Events, ImageData, Scroll, Snapshot};
use crate::emulator::inline::InlineImages;
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
/// How long a client waits for room in the control queue before failing.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
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

/// Small messages the emulator thread handles before any pending output.
enum Ctl {
    Input(Input),
    Wheel { up: bool, col: u16, row: u16, mods: u8 },
    Click { button: u8, col: u16, row: u16, mods: u8 },
    Resize(u16, u16),
    Scroll(Scroll),
    Snapshot(Sender<Snapshot>),
    Image(u32, Sender<Option<ImageData>>),
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
    input: Sender<Vec<u8>>,
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
    state: Arc<State>,
    pub started_ms: u64,
    pub last_output_ms: AtomicU64,
    pub error: Mutex<Option<String>>,
    /// The size the PTY and the emulator have (or have queued); see [`Terminal::resize`].
    size: AtomicU32,
    stopping: AtomicBool,
    /// Images encoded as PNG for clients: id -> (generation, width, height, file).
    encoded: Mutex<HashMap<u32, EncodedImage>>,
}

/// An image's pixels as a PNG file, for every client that needs it.
#[derive(Clone)]
pub struct EncodedImage {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub png: Arc<Vec<u8>>,
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
}

impl Events for Sink<'_> {
    fn reply(&mut self, bytes: Vec<u8>) {
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

impl Terminal {
    pub fn spawn(spec: Spawn<'_>, hooks: Hooks) -> Result<Arc<Self>> {
        let cols = spec.cols.clamp(20, 1000);
        let rows = spec.rows.clamp(5, 500);
        anyhow::ensure!(std::path::Path::new(spec.cwd).is_dir(), "working directory does not exist: {}", spec.cwd);
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).context("cannot create a pseudoconsole")?;
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
        let child = pair.slave.spawn_command(command).with_context(|| format!("cannot start {}", spec.program))?;
        drop(pair.slave);
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;

        let (input_tx, input_rx) = bounded::<Vec<u8>>(4096);
        let (control_tx, control_rx) = bounded::<Ctl>(CONTROL_QUEUE);
        let (output_tx, output_rx) = bounded::<Vec<u8>>(OUTPUT_QUEUE);
        let state = Arc::new(State::default());
        let generation = Arc::new(AtomicU64::new(1));
        let config = Config { cols, rows, scrollback: 5000, kitty_keyboard: true };
        let engine = spec.engine;
        let terminal = Arc::new(Self {
            id: spec.id.to_owned(),
            copilot: spec.copilot,
            engine,
            control: control_tx,
            pending_snapshot: Mutex::new(None),
            generation: generation.clone(),
            input: input_tx.clone(),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            pid,
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            raw_exit_code: Mutex::new(None),
            forced: AtomicBool::new(false),
            state: state.clone(),
            started_ms: now_ms(),
            last_output_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            size: AtomicU32::new(pack(cols, rows)),
            stopping: AtomicBool::new(false),
            encoded: Mutex::new(HashMap::new()),
        });
        // Weak: the emulator thread must not keep the terminal alive.
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
            },
            move || emulator::create(engine, &config),
        );
        spawn_writer(writer, input_rx);
        spawn_reader(terminal.clone(), reader, output_tx, hooks.bytes);
        spawn_waiter(terminal.clone(), child, hooks.exited, hooks.pump);
        Ok(terminal)
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

    /// The current viewport (a round trip to the emulator thread).
    pub fn snapshot(&self) -> Option<Snapshot> {
        request_snapshot(&self.control, &self.pending_snapshot, CONTROL_TIMEOUT, SNAPSHOT_TIMEOUT)
    }

    /// An image as a PNG file, encoded once per generation and shared by all
    /// clients; `None` when the image is gone or too large to send.
    pub fn image_png(&self, id: u32, generation: u64) -> Option<EncodedImage> {
        if let Some(cached) = lock(&self.encoded).get(&id).filter(|e| e.generation == generation) {
            return Some(cached.clone());
        }
        let (tx, rx) = bounded(1);
        self.control.send_timeout(Ctl::Image(id, tx), CONTROL_TIMEOUT).ok()?;
        let image = rx.recv_timeout(SNAPSHOT_TIMEOUT).ok()??;
        let png = emulator::encode_png(image.width, image.height, &image.rgba)?;
        if png.len() > MAX_IMAGE_MESSAGE {
            return None;
        }
        let encoded = EncodedImage { generation: image.generation, width: image.width, height: image.height, png: Arc::new(png) };
        let mut cache = lock(&self.encoded);
        if cache.len() >= MAX_ENCODED_IMAGES {
            cache.clear();
        }
        cache.insert(id, encoded.clone());
        Some(encoded)
    }

    /// Queues a control message, waiting up to [`CONTROL_TIMEOUT`] for room.
    fn command(&self, ctl: Ctl) -> Result<()> {
        send_control(&self.control, ctl, CONTROL_TIMEOUT)
    }

    /// A click: forwarded only when the app asked for mouse reports.
    pub fn click(&self, button: u8, col: u16, row: u16, mods: u8) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        self.command(Ctl::Click { button, col, row, mods })
    }

    pub fn send_input(&self, input: &Input) -> Result<()> {
        anyhow::ensure!(self.alive.load(Ordering::Acquire), "terminal has exited");
        self.command(Ctl::Input(input.clone()))
    }

    /// Resizes the PTY and the emulator together. Runs under the PTY lock so
    /// concurrent resizes reach both in the same order; the new size is
    /// stored only once the emulator has it queued, so a failed resize is
    /// retried by the next call instead of being skipped as unchanged.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(20, 1000);
        let rows = rows.clamp(5, 500);
        let packed = pack(cols, rows);
        let master = lock(&self.master);
        if self.size.load(Ordering::Acquire) == packed {
            return Ok(());
        }
        if let Some(master) = master.as_ref() {
            master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        }
        self.command(Ctl::Resize(cols, rows))?;
        self.size.store(packed, Ordering::Release);
        Ok(())
    }

    /// Mouse wheel: forwarded to the app when it wants it, else scrolls history.
    pub fn wheel(&self, up: bool, col: u16, row: u16, mods: u8) -> Result<()> {
        self.command(Ctl::Wheel { up, col, row, mods })
    }

    pub fn scroll(&self, delta: i32) {
        let scroll = match delta {
            i32::MAX => Scroll::Top,
            i32::MIN => Scroll::Bottom,
            d => Scroll::Delta(d),
        };
        let _ = self.command(Ctl::Scroll(scroll));
    }

    /// Graceful stop for agents: Ctrl+D (encoded by the emulator, so programs
    /// using the kitty keyboard protocol read it as a key), again after 2 s
    /// (some agents ask for a second one), Ctrl+C after 3.5 s (ConPTY turns it
    /// into a console Ctrl+C event, which ends a program stuck after its UI
    /// has quit), then force after 5 s.
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
        let ctrl_d = || Input::Key { key: crate::protocol::Key::Char('d'), mods: crate::protocol::MOD_CTRL };
        if self.command(Ctl::Input(ctrl_d())).is_err() {
            let _ = self.input.try_send(vec![0x04]);
        }
        let me = self.clone();
        std::thread::spawn(move || {
            for tick in 0..50 {
                if !me.alive.load(Ordering::Acquire) {
                    return;
                }
                if tick == 20 && me.command(Ctl::Input(ctrl_d())).is_err() {
                    let _ = me.input.try_send(vec![0x04]);
                }
                if tick == 35 {
                    let ctrl_c = Input::Key { key: crate::protocol::Key::Char('c'), mods: crate::protocol::MOD_CTRL };
                    if me.command(Ctl::Input(ctrl_c)).is_err() {
                        let _ = me.input.try_send(vec![0x03]);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            me.force_kill();
        });
    }

    /// The program had to be killed (a stop's grace period ran out, or a forced stop).
    pub fn was_forced(&self) -> bool {
        self.forced.load(Ordering::Acquire)
    }

    pub fn force_kill(&self) {
        self.forced.store(true, Ordering::Release);
        if self.pid != 0 {
            let _ = crate::win::kill_tree(self.pid);
        }
        let _ = lock(&self.killer).kill();
    }

    /// Closes the pseudoconsole, which ends the reader thread.
    fn close_pty(&self) {
        lock(&self.master).take();
    }
}

fn send_control(control: &Sender<Ctl>, ctl: Ctl, timeout: Duration) -> Result<()> {
    control.send_timeout(ctl, timeout).map_err(|error| match error {
        SendTimeoutError::Timeout(_) => anyhow::anyhow!("terminal is not responding (input backlog is full)"),
        SendTimeoutError::Disconnected(_) => anyhow::anyhow!("terminal emulator has stopped"),
    })
}

/// Asks the emulator thread for its viewport. At most one request per
/// terminal is outstanding: after a timeout the unanswered request is kept in
/// `pending` and the next call waits for its answer instead of queueing
/// another; an answer that arrived meanwhile is stale and replaced by a new request.
fn request_snapshot(control: &Sender<Ctl>, pending: &Mutex<Option<Receiver<Snapshot>>>, send: Duration, wait: Duration) -> Option<Snapshot> {
    let mut pending = lock(pending);
    let reply = match pending.take() {
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
            *pending = Some(reply);
            None
        }
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// Called once with the error message when the terminal's engine panics.
type FailHook = Box<dyn FnOnce(String) + Send>;

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
}

/// What the emulator thread handles next.
enum Msg {
    Control(Ctl),
    Output(Vec<u8>),
    /// A synchronized update's deadline passed with nothing else to do.
    SyncTimeout,
}

/// Owns the emulator until the terminal is dropped (its control channel closes).
fn spawn_emulator(t: EmulatorThread, make: impl FnOnce() -> Box<dyn Emulator> + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name("emulator".into()).spawn(move || t.run(make)).expect("spawn emulator thread")
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
        while let Some(msg) = self.next(emu.sync_deadline()) {
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.handle(emu.as_mut(), msg))) {
                let last = catch_unwind(AssertUnwindSafe(|| emu.snapshot())).ok();
                // Dropping a broken engine may panic as well.
                let _ = catch_unwind(AssertUnwindSafe(move || drop(emu)));
                return self.failed(&panic_message(panic.as_ref()), last);
            }
        }
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
        loop {
            let timer = deadline.map_or_else(never, at);
            let mut output_closed = false;
            let msg = select_biased! {
                recv(self.control) -> ctl => Some(Msg::Control(ctl.ok()?)),
                recv(self.output) -> bytes => match bytes {
                    Ok(bytes) => Some(Msg::Output(bytes)),
                    Err(_) => {
                        output_closed = true;
                        None
                    }
                },
                recv(timer) -> _ => Some(Msg::SyncTimeout),
            };
            if output_closed {
                // The reader ended; keep serving control messages.
                self.output = never();
            }
            match msg {
                Some(Msg::Control(ctl)) => {
                    self.burst += 1;
                    return Some(Msg::Control(ctl));
                }
                Some(msg) => {
                    self.burst = 0;
                    return Some(msg);
                }
                None => {}
            }
        }
    }

    fn sink(&self) -> Sink<'_> {
        Sink { id: &self.id, state: &self.state, input: &self.input, clipboard: &self.clipboard, changed: &self.changed, notify: self.notify.as_ref(), dirty: false }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pump.try_send(());
    }

    fn handle(&mut self, emu: &mut dyn Emulator, msg: Msg) {
        match msg {
            Msg::Output(bytes) => {
                let bytes = self.bridge.filter(&bytes);
                let mut sink = self.sink();
                emu.feed(&bytes, &mut sink);
                sink.finish();
                self.bump();
            }
            Msg::Control(ctl) => self.control(emu, ctl),
            Msg::SyncTimeout => {}
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
            Ctl::Resize(cols, rows) => {
                emu.resize(cols, rows);
                (self.cols, self.rows) = (cols, rows);
                self.bridge.set_size(cols, rows);
                self.bump();
            }
            Ctl::Scroll(scroll) => {
                emu.scroll(scroll);
                self.bump();
            }
            Ctl::Snapshot(reply) => {
                let _ = reply.send(emu.snapshot());
            }
            Ctl::Image(id, reply) => {
                let _ = reply.send(emu.image(id));
            }
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
            if let Ctl::Snapshot(reply) = ctl {
                let _ = reply.send(screen.clone());
            }
        }
    }
}

fn spawn_writer(mut writer: Box<dyn Write + Send>, input: Receiver<Vec<u8>>) {
    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            for bytes in input {
                if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })
        .expect("spawn pty writer");
}

fn spawn_reader(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>, output: Sender<Vec<u8>>, bytes: Arc<AtomicU64>) {
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
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                        let now = now_ms();
                        terminal.last_output_ms.store(now, Ordering::Relaxed);
                        let chunk = buffer[..n].to_vec();
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
        .expect("spawn pty reader");
}

/// Directory where the service records every terminal's raw output (golden corpus).
pub const RECORD_ENV: &str = "COPILOT_CANVAS_RECORD";

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
/// chunks as hex, which `copilot-canvas record --session` normalizes). Runs on
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
        let dir = PathBuf::from(std::env::var_os(RECORD_ENV).filter(|dir| !dir.is_empty())?);
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

fn spawn_waiter(terminal: Arc<Terminal>, mut child: Box<dyn portable_pty::Child + Send + Sync>, exited: Sender<String>, pump: Sender<()>) {
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
            terminal.close_pty();
            terminal.generation.fetch_add(1, Ordering::AcqRel);
            let _ = pump.try_send(());
            let _ = exited.send(terminal.id.clone());
        })
        .expect("spawn pty waiter");
}

pub fn row_hash<T: Hash>(runs: &[T]) -> u64 {
    let mut hasher = DefaultHasher::new();
    runs.hash(&mut hasher);
    hasher.finish()
}

/// A standalone alacritty emulator (tests, the self-test and the corpus).
pub fn headless(cols: u16, rows: u16, kitty_keyboard: bool) -> emulator::alacritty::AlacrittyEmulator {
    emulator::alacritty::AlacrittyEmulator::new(&Config { cols, rows, scrollback: 1000, kitty_keyboard })
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
    }

    impl Probe {
        fn new(log: &ProbeLog) -> Self {
            Self { log: log.clone(), feed_delay: Duration::ZERO, snapshot_delay: Duration::ZERO, cols: 80, rows: 24, sync: None }
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
            self.log.fed.fetch_add(1, Ordering::SeqCst);
        }
        fn resize(&mut self, cols: u16, rows: u16) {
            (self.cols, self.rows) = (cols, rows);
        }
        fn snapshot(&self) -> Snapshot {
            std::thread::sleep(self.snapshot_delay);
            self.log.snapshots.fetch_add(1, Ordering::SeqCst);
            Snapshot { cols: self.cols, rows: self.rows, lines: vec![Vec::new(); self.rows as usize], ..Snapshot::default() }
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
        _keep: (Receiver<(String, String)>, Receiver<String>, Receiver<()>),
    }

    impl Harness {
        fn new(probe: Probe) -> Self {
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
                cols: probe.cols,
                rows: probe.rows,
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
            };
            spawn_emulator(thread, move || Box::new(probe));
            Self { control, output, written, generation, failed, pending: Mutex::new(None), _keep: (clipboard_rx, changed_rx, pump_rx) }
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
        let log = ProbeLog::default();
        // 64 queued chunks of 20 ms each: input behind them would wait ~1.3 s.
        let harness = Harness::new(Probe { feed_delay: Duration::from_millis(20), ..Probe::new(&log) });
        let flood = harness.flood(Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(300));
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
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { feed_delay: Duration::from_millis(5), ..Probe::new(&log) });
        let flood = harness.flood(Duration::from_millis(800));
        for i in 0..40u16 {
            harness.send(Ctl::Resize(40 + i, 10 + i % 7));
        }
        let snap = harness.snapshot(Duration::from_secs(5)).expect("snapshot");
        assert_eq!((snap.cols, snap.rows), (79, 10 + 39 % 7));
        flood.join().unwrap();
    }

    #[test]
    fn a_synchronized_update_times_out_under_continuous_input_and_output() {
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
        // And with nothing else to do, the deadline alone ends a hold.
        harness.output.send(b"\x1b[?2026h".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(log.end_syncs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_engine_panic_is_reported_and_snapshots_keep_working() {
        let log = ProbeLog::default();
        let harness = Harness::new(Probe::new(&log));
        harness.send(Ctl::Resize(50, 12));
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
        };
        spawn_emulator(thread, || panic!("no engine"));
        assert!(failed.recv_timeout(Duration::from_secs(5)).unwrap().contains("no engine"));
        let snap = request_snapshot(&control, &Mutex::new(None), CONTROL_TIMEOUT, Duration::from_secs(5)).expect("blank screen");
        assert_eq!((snap.cols, snap.rows, snap.lines.len()), (30, 6, 6));
    }

    #[test]
    fn at_most_one_snapshot_request_is_outstanding() {
        let log = ProbeLog::default();
        let harness = Harness::new(Probe { snapshot_delay: Duration::from_millis(300), ..Probe::new(&log) });
        for _ in 0..5 {
            assert!(harness.snapshot(Duration::from_millis(10)).is_none());
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(log.snapshots.load(Ordering::SeqCst), 1, "timed-out requests do not pile up");
        // The stale answer is replaced by a fresh request.
        assert!(harness.snapshot(Duration::from_secs(5)).is_some());
        assert_eq!(log.snapshots.load(Ordering::SeqCst), 2);
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

    fn hooks() -> (Hooks, Vec<Box<dyn Any>>) {
        let (pump, a) = bounded::<()>(1);
        let (exited, b) = bounded::<String>(16);
        let (clipboard, c) = bounded::<(String, String)>(16);
        let (changed, d) = bounded::<String>(1024);
        (Hooks { pump, exited, clipboard, bytes: Arc::new(AtomicU64::new(0)), changed, notify: None }, vec![Box::new(a), Box::new(b), Box::new(c), Box::new(d)])
    }

    fn wait_exit(terminal: &Terminal, within: Duration) -> (Option<u32>, Option<u32>) {
        let started = Instant::now();
        while terminal.alive.load(Ordering::Acquire) && started.elapsed() < within {
            std::thread::sleep(Duration::from_millis(20));
        }
        (*lock(&terminal.exit_code), *lock(&terminal.raw_exit_code))
    }

    #[test]
    fn agents_the_canvas_stops_are_stopped_whatever_their_exit_code() {
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
        let (code, raw) = wait_exit(&stuck, Duration::from_secs(10));
        assert_eq!(code, Some(0), "stopped by the canvas: not a failure ({raw:?})");
        assert!(!stuck.was_forced() && asked.elapsed() < Duration::from_millis(4_900), "Ctrl+C ended it before the kill");
        // Ignores everything: killed when the grace period runs out, still reported as stopped.
        let (hooks_c, _keep_c) = hooks();
        let args = ["-NoProfile".to_owned(), "-Command".to_owned(), "[Console]::TreatControlCAsInput = $true; Start-Sleep 30".to_owned()];
        let spec = Spawn { id: "ignores", program: "powershell.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: true, engine: Engine::Alacritty };
        let ignores = Terminal::spawn(spec, hooks_c).expect("spawn powershell.exe");
        std::thread::sleep(Duration::from_millis(1_500));
        ignores.stop(false);
        let (code, raw) = wait_exit(&ignores, Duration::from_secs(10));
        assert!(ignores.was_forced() && code == Some(0) && raw.is_some_and(|c| c != 0), "killed after the grace period: {code:?} {raw:?}");
    }

    #[test]
    fn conpty_resizes_are_never_lost_while_output_floods() {
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
        let snap = terminal.snapshot().expect("snapshot under flood");
        assert_eq!((snap.cols, snap.rows), (69, 10 + 29 % 7));
        terminal.force_kill();
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
}
