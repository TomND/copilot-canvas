//! One hosted terminal: a ConPTY child, its emulator and its I/O threads.
//!
//! The emulator lives on a dedicated thread (engines need not be `Send`);
//! the PTY reader, clients and the frame pump reach it through commands.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::emulator::{self, Config, Emulator, Engine, Events, Scroll, Snapshot};
use crate::paths::now_ms;
use crate::protocol::{Input, Run, TermInfo};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Cmd {
    Output(Vec<u8>),
    Input(Input),
    Wheel { up: bool, col: u16, row: u16, mods: u8 },
    Click { button: u8, col: u16, row: u16, mods: u8 },
    Resize(u16, u16),
    Scroll(Scroll),
    Snapshot(Sender<Snapshot>),
}

pub struct Terminal {
    pub id: String,
    /// An agent CLI (graceful stop, history tracking, idle auto-stop).
    pub copilot: bool,
    pub engine: Engine,
    commands: Sender<Cmd>,
    /// Bumped by the emulator thread whenever the screen may have changed.
    generation: Arc<AtomicU64>,
    input: Sender<Vec<u8>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub pid: u32,
    pub alive: AtomicBool,
    pub exit_code: Mutex<Option<u32>>,
    state: Arc<State>,
    pub started_ms: u64,
    pub last_output_ms: AtomicU64,
    pub error: Mutex<Option<String>>,
    size: AtomicU32,
    stopping: AtomicBool,
}

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
    pub clipboard: Sender<String>,
    pub bytes: Arc<AtomicU64>,
    /// Title, progress or bell changed: the terminal's info is resent.
    pub changed: Sender<String>,
}

/// Delivers engine events to the terminal's state and hooks.
struct Sink<'a> {
    id: &'a str,
    state: &'a State,
    input: &'a Sender<Vec<u8>>,
    clipboard: &'a Sender<String>,
    changed: &'a Sender<String>,
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
        let _ = self.clipboard.try_send(text);
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
}

impl Sink<'_> {
    fn finish(self) {
        if self.dirty {
            let _ = self.changed.try_send(self.id.to_owned());
        }
    }
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
        let (cmd_tx, cmd_rx) = bounded::<Cmd>(4096);
        let state = Arc::new(State::default());
        let generation = Arc::new(AtomicU64::new(1));
        let config = Config { cols, rows, scrollback: 5000, kitty_keyboard: true };
        let engine = spec.engine;
        let terminal = Arc::new(Self {
            id: spec.id.to_owned(),
            copilot: spec.copilot,
            engine,
            commands: cmd_tx.clone(),
            generation: generation.clone(),
            input: input_tx.clone(),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            pid,
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            state: state.clone(),
            started_ms: now_ms(),
            last_output_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            size: AtomicU32::new(((cols as u32) << 16) | rows as u32),
            stopping: AtomicBool::new(false),
        });
        spawn_emulator(EmulatorThread {
            id: spec.id.to_owned(),
            engine,
            config,
            commands: cmd_rx,
            generation,
            state,
            input: input_tx,
            clipboard: hooks.clipboard.clone(),
            changed: hooks.changed.clone(),
            pump: hooks.pump.clone(),
        });
        spawn_writer(writer, input_rx);
        spawn_reader(terminal.clone(), reader, cmd_tx, hooks.bytes);
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
        let (tx, rx) = bounded(1);
        self.commands.send(Cmd::Snapshot(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    fn command(&self, cmd: Cmd) -> Result<()> {
        self.commands.try_send(cmd).map_err(|_| anyhow::anyhow!("terminal input backlog is full"))
    }

    /// A click: forwarded only when the app asked for mouse reports.
    pub fn click(&self, button: u8, col: u16, row: u16, mods: u8) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        self.command(Cmd::Click { button, col, row, mods })
    }

    pub fn send_input(&self, input: &Input) -> Result<()> {
        anyhow::ensure!(self.alive.load(Ordering::Acquire), "terminal has exited");
        self.command(Cmd::Input(input.clone()))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(20, 1000);
        let rows = rows.clamp(5, 500);
        let current = self.size.load(Ordering::Relaxed);
        if current == ((cols as u32) << 16) | rows as u32 {
            return Ok(());
        }
        if let Some(master) = lock(&self.master).as_ref() {
            master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        }
        self.size.store(((cols as u32) << 16) | rows as u32, Ordering::Relaxed);
        self.command(Cmd::Resize(cols, rows))
    }

    /// Mouse wheel: forwarded to the app when it wants it, else scrolls history.
    pub fn wheel(&self, up: bool, col: u16, row: u16, mods: u8) -> Result<()> {
        self.command(Cmd::Wheel { up, col, row, mods })
    }

    pub fn scroll(&self, delta: i32) {
        let scroll = match delta {
            i32::MAX => Scroll::Top,
            i32::MIN => Scroll::Bottom,
            d => Scroll::Delta(d),
        };
        let _ = self.command(Cmd::Scroll(scroll));
    }

    /// Graceful stop for agents (Ctrl+D, then force after a grace period).
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
        let _ = self.input.try_send(vec![0x04]);
        let me = self.clone();
        std::thread::spawn(move || {
            for _ in 0..50 {
                if !me.alive.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            me.force_kill();
        });
    }

    pub fn force_kill(&self) {
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

struct EmulatorThread {
    id: String,
    engine: Engine,
    config: Config,
    commands: Receiver<Cmd>,
    generation: Arc<AtomicU64>,
    state: Arc<State>,
    input: Sender<Vec<u8>>,
    clipboard: Sender<String>,
    changed: Sender<String>,
    pump: Sender<()>,
}

/// Owns the emulator until the terminal is dropped.
fn spawn_emulator(t: EmulatorThread) {
    std::thread::Builder::new()
        .name("emulator".into())
        .spawn(move || {
            let mut emu: Box<dyn Emulator> = emulator::create(t.engine, &t.config);
            let bump = |t: &EmulatorThread| {
                t.generation.fetch_add(1, Ordering::AcqRel);
                let _ = t.pump.try_send(());
            };
            loop {
                let cmd = match emu.sync_deadline() {
                    Some(deadline) => match t.commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(cmd) => cmd,
                        Err(RecvTimeoutError::Timeout) => {
                            // A synchronized update whose end marker never arrived.
                            let mut sink = Sink { id: &t.id, state: &t.state, input: &t.input, clipboard: &t.clipboard, changed: &t.changed, dirty: false };
                            emu.end_sync(&mut sink);
                            sink.finish();
                            bump(&t);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                    None => match t.commands.recv() {
                        Ok(cmd) => cmd,
                        Err(_) => return,
                    },
                };
                match cmd {
                    Cmd::Output(bytes) => {
                        let mut sink = Sink { id: &t.id, state: &t.state, input: &t.input, clipboard: &t.clipboard, changed: &t.changed, dirty: false };
                        emu.feed(&bytes, &mut sink);
                        sink.finish();
                        bump(&t);
                    }
                    Cmd::Input(input) => {
                        if !matches!(input, Input::Focus(_)) && emu.display_offset() != 0 {
                            emu.scroll(Scroll::Bottom);
                            bump(&t);
                        }
                        let bytes = emu.encode(&input);
                        if !bytes.is_empty() {
                            let _ = t.input.try_send(bytes);
                        }
                    }
                    Cmd::Click { button, col, row, mods } => {
                        if let Some(bytes) = emu.encode_click(button, col, row, mods) {
                            if emu.display_offset() != 0 {
                                emu.scroll(Scroll::Bottom);
                                bump(&t);
                            }
                            let _ = t.input.try_send(bytes);
                        }
                    }
                    Cmd::Wheel { up, col, row, mods } => match emu.encode_wheel(up, col, row, mods) {
                        Some(bytes) => {
                            if emu.display_offset() != 0 {
                                emu.scroll(Scroll::Bottom);
                                bump(&t);
                            }
                            let _ = t.input.try_send(bytes);
                        }
                        None => {
                            emu.scroll(Scroll::Delta(if up { 3 } else { -3 }));
                            bump(&t);
                        }
                    },
                    Cmd::Resize(cols, rows) => {
                        emu.resize(cols, rows);
                        bump(&t);
                    }
                    Cmd::Scroll(scroll) => {
                        emu.scroll(scroll);
                        bump(&t);
                    }
                    Cmd::Snapshot(reply) => {
                        let _ = reply.send(emu.snapshot());
                    }
                }
            }
        })
        .expect("spawn emulator thread");
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

fn spawn_reader(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>, commands: Sender<Cmd>, bytes: Arc<AtomicU64>) {
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut tap = Tap::from_env();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                        terminal.last_output_ms.store(now_ms(), Ordering::Relaxed);
                        if tap.as_mut().is_some_and(|tap| !tap.write(&terminal, &buffer[..n])) {
                            tap = None;
                        }
                        // Blocks when the emulator falls behind: ConPTY then slows the program.
                        if commands.send(Cmd::Output(buffer[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("spawn pty reader");
}

/// Directory where the service records every terminal's raw output (golden corpus).
pub const RECORD_ENV: &str = "COPILOT_CANVAS_RECORD";

/// Where the recording tap writes a terminal's output.
pub fn recording_path(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    let name: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    dir.join(format!("{name}.vtrec"))
}

/// Appends each PTY read to `<RECORD_ENV>/<id>.vtrec` (format: corpus/README.md;
/// chunks as hex, which `copilot-canvas record --session` normalizes).
struct Tap {
    dir: std::path::PathBuf,
    file: Option<std::fs::File>,
    last_ms: u64,
    size: u32,
}

impl Tap {
    fn from_env() -> Option<Self> {
        let dir = std::env::var_os(RECORD_ENV).filter(|dir| !dir.is_empty())?;
        Some(Self { dir: dir.into(), file: None, last_ms: 0, size: 0 })
    }

    /// `false` on any I/O error: recording stops, the terminal carries on.
    fn write(&mut self, terminal: &Terminal, bytes: &[u8]) -> bool {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let now = now_ms();
        let size = terminal.size.load(Ordering::Relaxed);
        let mut line = String::with_capacity(bytes.len() * 2 + 160);
        if self.file.is_none() {
            let path = recording_path(&self.dir, &terminal.id);
            let opened = std::fs::create_dir_all(&self.dir).and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path));
            let Ok(file) = opened else { return false };
            self.file = Some(file);
            self.last_ms = terminal.started_ms;
            self.size = size;
            let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            line.push_str(&format!(
                "{{\"vtrec\":1,\"cols\":{},\"rows\":{},\"program\":\"{id}\",\"description\":\"live terminal {id}\",\"source\":\"service\",\"recorded_at\":\"{}\"}}\n",
                size >> 16,
                size & 0xffff,
                crate::paths::rfc3339_now()
            ));
        }
        let mut delta = now.saturating_sub(self.last_ms);
        self.last_ms = now.max(self.last_ms);
        if size != self.size {
            self.size = size;
            line.push_str(&format!("{{\"t\":{delta},\"resize\":[{},{}]}}\n", size >> 16, size & 0xffff));
            delta = 0;
        }
        line.push_str(&format!("{{\"t\":{delta},\"x\":\""));
        for &b in bytes {
            line.push(HEX[(b >> 4) as usize] as char);
            line.push(HEX[(b & 15) as usize] as char);
        }
        line.push_str("\"}\n");
        self.file.as_mut().is_some_and(|file| file.write_all(line.as_bytes()).is_ok())
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
            *lock(&terminal.exit_code) = Some(code);
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
