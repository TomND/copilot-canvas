//! One hosted terminal: a ConPTY child, its emulator state, and I/O threads.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::paths::now_ms;
use crate::protocol::{
    COLOR_DEFAULT, COLOR_RGB, F_BOLD, F_DIM, F_HIDDEN, F_INVERSE, F_ITALIC, F_STRIKE, F_UNDERLINE, Input, Run, TermInfo,
};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Dims {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
pub struct Listener {
    input: Sender<Vec<u8>>,
    title: Arc<Mutex<String>>,
    clipboard: Sender<String>,
    size: Arc<AtomicU32>,
    id: String,
    bells: Arc<AtomicU32>,
    changed: Sender<String>,
}

const DEFAULT_FG: Rgb = Rgb { r: 0xcc, g: 0xcc, b: 0xcc };
const DEFAULT_BG: Rgb = Rgb { r: 0x0c, g: 0x0c, b: 0x0c };

/// xterm's default 256-color palette.
pub fn palette(index: usize) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0x0c, 0x0c, 0x0c), (0xc5, 0x0f, 0x1f), (0x13, 0xa1, 0x0e), (0xc1, 0x9c, 0x00),
        (0x00, 0x37, 0xda), (0x88, 0x17, 0x98), (0x3a, 0x96, 0xdd), (0xcc, 0xcc, 0xcc),
        (0x76, 0x76, 0x76), (0xe7, 0x48, 0x56), (0x16, 0xc6, 0x0c), (0xf9, 0xf1, 0xa5),
        (0x3b, 0x78, 0xff), (0xb4, 0x00, 0x9e), (0x61, 0xd6, 0xd6), (0xf2, 0xf2, 0xf2),
    ];
    match index {
        0..=15 => {
            let (r, g, b) = BASE[index];
            Rgb { r, g, b }
        }
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        _ if index == NamedColor::Background as usize => DEFAULT_BG,
        _ => DEFAULT_FG,
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                let _ = self.input.try_send(text.into_bytes());
            }
            Event::Title(title) => {
                *lock(&self.title) = title;
                let _ = self.changed.try_send(self.id.clone());
            }
            Event::ResetTitle => {
                lock(&self.title).clear();
                let _ = self.changed.try_send(self.id.clone());
            }
            Event::Bell => {
                self.bells.fetch_add(1, Ordering::Relaxed);
                let _ = self.changed.try_send(self.id.clone());
            }
            Event::ClipboardStore(_, text) => {
                let _ = self.clipboard.try_send(text);
            }
            Event::ColorRequest(index, format) => {
                let _ = self.input.try_send(format(palette(index)).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = self.size.load(Ordering::Relaxed);
                let window = WindowSize {
                    num_cols: (size >> 16) as u16,
                    num_lines: (size & 0xffff) as u16,
                    cell_width: 8,
                    cell_height: 16,
                };
                let _ = self.input.try_send(format(window).into_bytes());
            }
            // Clipboard reads are refused: applications must not read the user's clipboard.
            _ => {}
        }
    }
}

pub struct Inner {
    pub term: Term<Listener>,
    pub parser: Processor,
    pub generation: u64,
}

pub struct Terminal {
    pub id: String,
    pub copilot: bool,
    pub inner: Mutex<Inner>,
    input: Sender<Vec<u8>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub pid: u32,
    pub alive: AtomicBool,
    pub exit_code: Mutex<Option<u32>>,
    title: Arc<Mutex<String>>,
    progress: AtomicU8,
    progress_seen: AtomicBool,
    progress_ms: AtomicU64,
    bells: Arc<AtomicU32>,
    changed: Sender<String>,
    pub started_ms: u64,
    pub last_output_ms: AtomicU64,
    pub error: Mutex<Option<String>>,
    size: Arc<AtomicU32>,
    stopping: AtomicBool,
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

impl Terminal {
    pub fn spawn(spec: Spawn<'_>, hooks: Hooks) -> Result<Arc<Self>> {
        let cols = spec.cols.clamp(20, 1000);
        let rows = spec.rows.clamp(5, 500);
        anyhow::ensure!(std::path::Path::new(spec.cwd).is_dir(), "working directory does not exist: {}", spec.cwd);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("cannot create a pseudoconsole")?;
        let mut command = CommandBuilder::new(spec.program);
        command.args(spec.args);
        command.cwd(spec.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (key, value) in spec.env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).with_context(|| format!("cannot start {}", spec.program))?;
        drop(pair.slave);
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;

        let (input_tx, input_rx) = bounded::<Vec<u8>>(4096);
        let title = Arc::new(Mutex::new(String::new()));
        let size = Arc::new(AtomicU32::new(((cols as u32) << 16) | rows as u32));
        let bells = Arc::new(AtomicU32::new(0));
        let listener = Listener {
            input: input_tx.clone(),
            title: title.clone(),
            clipboard: hooks.clipboard.clone(),
            size: size.clone(),
            id: spec.id.to_owned(),
            bells: bells.clone(),
            changed: hooks.changed.clone(),
        };
        let config = Config { scrolling_history: 5000, kitty_keyboard: true, ..Config::default() };
        let term = Term::new(config, &Dims { cols: cols as usize, rows: rows as usize }, listener);
        let terminal = Arc::new(Self {
            id: spec.id.to_owned(),
            copilot: spec.copilot,
            inner: Mutex::new(Inner { term, parser: Processor::new(), generation: 1 }),
            input: input_tx,
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            pid,
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            title,
            progress: AtomicU8::new(0),
            progress_seen: AtomicBool::new(false),
            progress_ms: AtomicU64::new(0),
            bells,
            changed: hooks.changed,
            started_ms: now_ms(),
            last_output_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            size,
            stopping: AtomicBool::new(false),
        });
        spawn_writer(writer, input_rx);
        spawn_reader(terminal.clone(), reader, hooks.pump.clone(), hooks.bytes);
        spawn_waiter(terminal.clone(), child, hooks.exited, hooks.pump);
        Ok(terminal)
    }

    pub fn info(&self) -> TermInfo {
        TermInfo {
            id: self.id.clone(),
            alive: self.alive.load(Ordering::Acquire),
            exit_code: *lock(&self.exit_code),
            title: lock(&self.title).clone(),
            pid: self.pid,
            started_ms: self.started_ms,
            last_output_ms: self.last_output_ms.load(Ordering::Relaxed),
            copilot: self.copilot,
            error: lock(&self.error).clone(),
            progress: self.progress.load(Ordering::Relaxed),
            bells: self.bells.load(Ordering::Relaxed),
            progress_seen: self.progress_seen.load(Ordering::Relaxed),
            progress_ms: self.progress_ms.load(Ordering::Relaxed),
        }
    }

    /// A click: forwarded only when the app asked for mouse reports.
    pub fn click(&self, button: u8, col: u16, row: u16, mods: u8) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let bytes = {
            let mut inner = lock(&self.inner);
            let Some(bytes) = super::keys::encode_click(button, col, row, mods, *inner.term.mode()) else { return Ok(()) };
            if inner.term.grid().display_offset() != 0 {
                inner.term.scroll_display(Scroll::Bottom);
                inner.generation += 1;
            }
            bytes
        };
        self.input.try_send(bytes).map_err(|_| anyhow::anyhow!("terminal input backlog is full"))?;
        Ok(())
    }

    pub fn send_input(&self, input: &Input) -> Result<()> {
        anyhow::ensure!(self.alive.load(Ordering::Acquire), "terminal has exited");
        let bytes = {
            let mut inner = lock(&self.inner);
            if !matches!(input, Input::Focus(_)) && inner.term.grid().display_offset() != 0 {
                inner.term.scroll_display(Scroll::Bottom);
                inner.generation += 1;
            }
            super::keys::encode(input, *inner.term.mode())
        };
        if !bytes.is_empty() {
            self.input.try_send(bytes).map_err(|_| anyhow::anyhow!("terminal input backlog is full"))?;
        }
        Ok(())
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
        let mut inner = lock(&self.inner);
        inner.term.resize(Dims { cols: cols as usize, rows: rows as usize });
        inner.generation += 1;
        Ok(())
    }

    /// Mouse wheel: forwarded to the app when it wants it, else scrolls history.
    pub fn wheel(&self, up: bool, col: u16, row: u16, mods: u8) -> Result<()> {
        let bytes = {
            let mut inner = lock(&self.inner);
            let forwarded = if self.alive.load(Ordering::Acquire) {
                super::keys::encode_wheel(up, col, row, mods, *inner.term.mode())
            } else {
                None
            };
            match forwarded {
                Some(bytes) => {
                    if inner.term.grid().display_offset() != 0 {
                        inner.term.scroll_display(Scroll::Bottom);
                        inner.generation += 1;
                    }
                    bytes
                }
                None => {
                    inner.term.scroll_display(Scroll::Delta(if up { 3 } else { -3 }));
                    inner.generation += 1;
                    return Ok(());
                }
            }
        };
        self.input.try_send(bytes).map_err(|_| anyhow::anyhow!("terminal input backlog is full"))?;
        Ok(())
    }

    pub fn scroll(&self, delta: i32) {
        let mut inner = lock(&self.inner);
        let scroll = match delta {
            i32::MAX => Scroll::Top,
            i32::MIN => Scroll::Bottom,
            d => Scroll::Delta(d),
        };
        inner.term.scroll_display(scroll);
        inner.generation += 1;
    }

    /// Graceful stop for Copilot (Ctrl+D, then force after a grace period).
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

fn spawn_reader(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>, pump: Sender<()>, bytes: Arc<AtomicU64>) {
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut osc = OscScan::default();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut changed = false;
                        for event in osc.feed(&buffer[..n]) {
                            match event {
                                OscEvent::Progress(state) => {
                                    let first = !terminal.progress_seen.swap(true, Ordering::Relaxed);
                                    if terminal.progress.swap(state, Ordering::Relaxed) != state || first {
                                        terminal.progress_ms.store(now_ms(), Ordering::Relaxed);
                                        changed = true;
                                    }
                                }
                                OscEvent::Notify => {
                                    terminal.bells.fetch_add(1, Ordering::Relaxed);
                                    changed = true;
                                }
                            }
                        }
                        if changed {
                            let _ = terminal.changed.try_send(terminal.id.clone());
                        }
                        {
                            let mut inner = lock(&terminal.inner);
                            let Inner { term, parser, generation } = &mut *inner;
                            parser.advance(term, &buffer[..n]);
                            *generation += 1;
                        }
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                        terminal.last_output_ms.store(now_ms(), Ordering::Relaxed);
                        let _ = pump.try_send(());
                    }
                }
            }
        })
        .expect("spawn pty reader");
}

#[derive(Debug, PartialEq)]
pub enum OscEvent {
    /// OSC 9;4 taskbar progress state (0 none, 1 value, 2 error, 3 busy, 4 paused).
    Progress(u8),
    /// OSC 777;notify desktop notification.
    Notify,
}

/// Finds the OSC sequences the terminal emulator ignores (progress and
/// notifications) in the raw output, across read boundaries.
#[derive(Default)]
pub struct OscScan {
    state: u8,
    payload: Vec<u8>,
}

impl OscScan {
    const GROUND: u8 = 0;
    const ESC: u8 = 1;
    const OSC: u8 = 2;
    const OSC_ESC: u8 = 3;

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            match self.state {
                Self::GROUND => {
                    if b == 0x1b {
                        self.state = Self::ESC;
                    }
                }
                Self::ESC => {
                    self.state = match b {
                        b']' => {
                            self.payload.clear();
                            Self::OSC
                        }
                        0x1b => Self::ESC,
                        _ => Self::GROUND,
                    }
                }
                Self::OSC => match b {
                    0x07 => {
                        events.extend(self.finish());
                        self.state = Self::GROUND;
                    }
                    0x1b => self.state = Self::OSC_ESC,
                    0x18 | 0x1a => self.state = Self::GROUND,
                    _ => {
                        if self.payload.len() < 64 {
                            self.payload.push(b);
                        }
                    }
                },
                _ => {
                    if b == b'\\' {
                        events.extend(self.finish());
                    }
                    self.state = if b == 0x1b { Self::ESC } else { Self::GROUND };
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Option<OscEvent> {
        let text = std::str::from_utf8(&self.payload).ok()?;
        if let Some(rest) = text.strip_prefix("9;4;") {
            let state = rest.split(';').next()?.parse::<u8>().ok()?;
            return (state <= 4).then_some(OscEvent::Progress(state));
        }
        text.starts_with("777;notify").then_some(OscEvent::Notify)
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
            lock(&terminal.inner).generation += 1;
            let _ = pump.try_send(());
            let _ = exited.send(terminal.id.clone());
        })
        .expect("spawn pty waiter");
}

fn color(color: Color, flags: &mut u16) -> u32 {
    match color {
        Color::Spec(rgb) => COLOR_RGB | (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32,
        Color::Indexed(index) => index as u32 + 1,
        Color::Named(named) => {
            let value = named as usize;
            if value < 16 {
                value as u32 + 1
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&value) {
                *flags |= F_DIM;
                (value - NamedColor::DimBlack as usize) as u32 + 1
            } else {
                if named == NamedColor::DimForeground {
                    *flags |= F_DIM;
                }
                COLOR_DEFAULT
            }
        }
    }
}

pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run>>,
    pub cursor: Option<(u16, u16)>,
    pub display_offset: u32,
    pub history: u32,
}

/// Converts the visible viewport into styled runs.
pub fn snapshot(inner: &Inner) -> Snapshot {
    let term = &inner.term;
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = grid.display_offset();
    let mut lines = Vec::with_capacity(rows);
    for y in 0..rows {
        let row = &grid[Line(y as i32 - offset as i32)];
        let mut runs: Vec<Run> = Vec::new();
        for x in 0..cols {
            let cell = &row[Column(x)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut flags = 0u16;
            let fg = color(cell.fg, &mut flags);
            let mut bg_flags = 0u16;
            let bg = color(cell.bg, &mut bg_flags);
            let f = cell.flags;
            if f.contains(Flags::BOLD) {
                flags |= F_BOLD;
            }
            if f.contains(Flags::ITALIC) {
                flags |= F_ITALIC;
            }
            if f.intersects(Flags::ALL_UNDERLINES) {
                flags |= F_UNDERLINE;
            }
            if f.contains(Flags::INVERSE) {
                flags |= F_INVERSE;
            }
            if f.contains(Flags::DIM) {
                flags |= F_DIM;
            }
            if f.contains(Flags::HIDDEN) {
                flags |= F_HIDDEN;
            }
            if f.contains(Flags::STRIKEOUT) {
                flags |= F_STRIKE;
            }
            let ch = if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
            let link = cell.hyperlink().map(|h| h.uri().to_owned());
            match runs.last_mut() {
                Some(run) if run.fg == fg && run.bg == bg && run.flags == flags && run.link == link => run.text.push(ch),
                _ => runs.push(Run { col: x as u16, text: ch.to_string(), fg, bg, flags, link }),
            }
            if let Some(extra) = cell.zerowidth() {
                runs.last_mut().expect("run exists").text.extend(extra);
            }
        }
        trim_trailing_blank(&mut runs);
        lines.push(runs);
    }
    let cursor_point = grid.cursor.point;
    let cursor = (offset == 0 && term.mode().contains(TermMode::SHOW_CURSOR))
        .then(|| (cursor_point.column.0 as u16, cursor_point.line.0.max(0) as u16));
    Snapshot {
        cols: cols as u16,
        rows: rows as u16,
        lines,
        cursor,
        display_offset: offset as u32,
        history: grid.history_size() as u32,
    }
}

fn trim_trailing_blank(runs: &mut Vec<Run>) {
    while let Some(run) = runs.last_mut() {
        if run.bg != COLOR_DEFAULT || run.flags & (F_INVERSE | F_UNDERLINE | F_STRIKE) != 0 || run.link.is_some() {
            return;
        }
        let trimmed = run.text.trim_end_matches(' ').len();
        if trimmed == 0 {
            runs.pop();
        } else {
            run.text.truncate(trimmed);
            return;
        }
    }
}

pub fn row_hash(runs: &[Run]) -> u64 {
    let mut hasher = DefaultHasher::new();
    runs.hash(&mut hasher);
    hasher.finish()
}

/// A standalone emulator (used by tests, the self-test and benchmarks).
/// Terminal replies (e.g. cursor reports) arrive on the returned receiver.
#[cfg(test)]
pub fn headless(cols: usize, rows: usize) -> (Inner, Receiver<Vec<u8>>) {
    headless_with(cols, rows, false)
}

/// Like `headless`, optionally answering kitty keyboard protocol queries as the service does.
pub fn headless_with(cols: usize, rows: usize, kitty_keyboard: bool) -> (Inner, Receiver<Vec<u8>>) {
    let (input, responses) = crossbeam_channel::unbounded();
    let (clipboard, _) = crossbeam_channel::unbounded();
    let (changed, _) = crossbeam_channel::unbounded();
    let listener = Listener {
        input,
        title: Arc::new(Mutex::new(String::new())),
        clipboard,
        size: Arc::new(AtomicU32::new(((cols as u32) << 16) | rows as u32)),
        id: String::new(),
        bells: Arc::new(AtomicU32::new(0)),
        changed,
    };
    let config = Config { scrolling_history: 1000, kitty_keyboard, ..Config::default() };
    let term = Term::new(config, &Dims { cols, rows }, listener);
    (Inner { term, parser: Processor::new(), generation: 0 }, responses)
}

/// Plain text of the visible screen.
pub fn screen_text(inner: &Inner) -> String {
    snapshot(inner)
        .lines
        .iter()
        .map(|runs| {
            let mut line = String::new();
            for run in runs {
                let width = super::super::client::paint::text_width(&line);
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

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn emulator(cols: usize, rows: usize) -> (Inner, Receiver<Vec<u8>>) {
        headless(cols, rows)
    }

    fn feed(inner: &mut Inner, bytes: &[u8]) {
        let Inner { term, parser, .. } = inner;
        parser.advance(term, bytes);
    }

    #[test]
    fn snapshot_preserves_colors_wide_chars_and_cursor() {
        let (mut inner, _) = emulator(20, 3);
        feed(&mut inner, "\x1b[31mred\x1b[0m 中!\r\n\x1b[48;2;1;2;3mbg\x1b[0m".as_bytes());
        let snap = snapshot(&inner);
        let first = &snap.lines[0];
        assert_eq!(first[0], Run { col: 0, text: "red".into(), fg: 2, bg: 0, flags: 0, link: None });
        assert_eq!(first[1].col, 3);
        assert_eq!(first[1].text, " 中!");
        assert_eq!(snap.lines[1][0].bg, COLOR_RGB | 0x010203);
        assert_eq!(snap.cursor, Some((2, 1)));
        assert!(snap.lines[2].is_empty(), "blank rows carry no runs");
    }

    #[test]
    fn osc_scan_finds_progress_and_notifications_across_reads() {
        let mut scan = OscScan::default();
        assert_eq!(scan.feed(b"text \x1b]9;4;3;0\x07 more"), vec![OscEvent::Progress(3)]);
        assert_eq!(scan.feed(b"\x1b]9;4;"), vec![]);
        assert_eq!(scan.feed(b"0;0\x1b\\"), vec![OscEvent::Progress(0)], "split read, ST terminator");
        assert_eq!(scan.feed(b"\x1b]777;notify;T;B\x07\x1b]0;title\x07\x1b]9;4;9\x07"), vec![OscEvent::Notify]);
    }

    #[test]
    fn osc8_hyperlinks_become_link_runs() {
        let (mut inner, _) = emulator(40, 2);
        feed(&mut inner, b"see \x1b]8;;https://github.com/pr/1\x1b\\PR 1\x1b]8;;\x1b\\ now");
        let line = &snapshot(&inner).lines[0];
        let linked: Vec<_> = line.iter().filter(|r| r.link.is_some()).collect();
        assert_eq!(linked.len(), 1);
        assert_eq!((linked[0].text.as_str(), linked[0].link.as_deref()), ("PR 1", Some("https://github.com/pr/1")));
        assert!(line.last().unwrap().text.ends_with("now"));
    }

    #[test]
    fn answers_cursor_position_queries_required_by_conpty() {
        let (mut inner, responses) = emulator(20, 5);
        feed(&mut inner, b"ab\x1b[6n");
        assert_eq!(responses.try_recv().unwrap(), b"\x1b[1;3R");
        feed(&mut inner, b"\x1b]11;?\x07");
        let reply = String::from_utf8(responses.try_recv().unwrap()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:"), "background color query answered: {reply:?}");
    }

    #[test]
    fn hidden_cursor_and_scrollback_are_reflected() {
        let (mut inner, _) = emulator(10, 3);
        for i in 0..10 {
            feed(&mut inner, format!("line{i}\r\n").as_bytes());
        }
        feed(&mut inner, b"\x1b[?25l");
        assert_eq!(snapshot(&inner).cursor, None);
        inner.term.scroll_display(Scroll::Delta(3));
        let snap = snapshot(&inner);
        assert_eq!(snap.display_offset, 3);
        assert!(snap.history >= 3);
        assert_eq!(snap.lines[0][0].text, "line5");
    }

    #[test]
    fn palette_matches_xterm_cube() {
        assert_eq!(palette(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(palette(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(palette(232), Rgb { r: 8, g: 8, b: 8 });
    }
}
