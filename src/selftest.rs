//! `doctor` (environment checks) and `selftest` (end-to-end UI test that drives
//! the real client inside a hidden pseudoconsole, using demo shells only).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::client::doc::Doc;
use crate::paths::Instance;
use canvas_agents::agents::{self, AgentKind, Provide};
use canvas_host::emulator::alacritty::AlacrittyEmulator;
use canvas_host::emulator::{Emulator, feed_collect};
use crate::service::term::{headless, lock, screen_text};

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A pseudoconsole whose output feeds an emulator; emulator replies (cursor
/// position reports that ConPTY waits for) are written back automatically.
pub struct Console {
    pub screen: Arc<Mutex<AlacrittyEmulator>>,
    writer: SharedWriter,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Console {
    pub fn start(program: &Path, args: &[String], cols: u16, rows: u16) -> Result<Self> {
        Self::start_with_env(program, args, &[], cols, rows)
    }

    pub fn start_with_env(program: &Path, args: &[String], env: &[(&str, &Path)], cols: u16, rows: u16) -> Result<Self> {
        Self::start_full(program, args, env, cols, rows, false)
    }

    pub fn start_full(program: &Path, args: &[String], env: &[(&str, &Path)], cols: u16, rows: u16, kitty: bool) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Ok(cwd) = std::env::current_dir() {
            command.cwd(cwd);
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(headless(cols, rows, kitty)));
        let mut reader = pair.master.try_clone_reader()?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let (shared, reply_writer) = (screen.clone(), writer.clone());
        // Probes: CC_RAW_CAPTURE=<file> records the program's raw output.
        let mut capture = std::env::var_os("CC_RAW_CAPTURE").and_then(|p| std::fs::File::create(p).ok());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Some(file) = capture.as_mut() {
                    let _ = file.write_all(&buf[..n]);
                }
                let replies = feed_collect(&mut *lock(&shared), &buf[..n]).replies;
                for reply in replies {
                    let mut w = lock(&reply_writer);
                    let _ = w.write_all(&reply).and_then(|_| w.flush());
                }
            }
        });
        Ok(Self { screen, writer, child, _master: pair.master })
    }

    pub fn text(&self) -> String {
        screen_text(&lock(&self.screen).snapshot())
    }

    /// The program's process id.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn mode(&self) -> String {
        format!("{:?}", lock(&self.screen).term.mode())
    }

    /// Writes input at once, without the settle delay.
    pub fn send_now(&mut self, bytes: &[u8]) -> Result<()> {
        let mut w = lock(&self.writer);
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    /// Writes input as typing: a trailing Enter goes in a write of its own,
    /// as a person presses it after the text (one write of text and Enter
    /// is a paste to the TUI: `input::coalesce`).
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let (text, enter) = match bytes.split_last() {
            Some((b'\r', text)) if !text.is_empty() => (text, true),
            _ => (bytes, false),
        };
        self.send_now(text)?;
        if enter {
            // Long enough for the client to have read the text: an Enter that
            // follows it at once would still be part of the burst.
            std::thread::sleep(Duration::from_millis(150 + text.len() as u64));
            self.send_now(b"\r")?;
        }
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }

    /// Pastes `text` as Windows Terminal does: line breaks as CR, inside
    /// paste markers when the client asked for bracketed paste (the console
    /// hands the client key presses either way).
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = lock(&self.screen).modes() & crate::protocol::MODE_BRACKETED_PASTE != 0;
        let bytes = if bracketed { format!("\x1b[200~{text}\x1b[201~") } else { text };
        self.send_now(bytes.as_bytes())?;
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    pub fn wait(&self, what: &str, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(&self.text()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        bail!("timed out waiting for {what}\n--- screen ---\n{}", self.text())
    }

    fn click(&mut self, x: u16, y: u16) -> Result<()> {
        self.send(format!("\x1b[<0;{};{}M", x + 1, y + 1).as_bytes())?;
        self.send(format!("\x1b[<0;{};{}m", x + 1, y + 1).as_bytes())
    }

    fn drag(&mut self, button: u8, from: (u16, u16), to: (u16, u16)) -> Result<()> {
        self.send(format!("\x1b[<{button};{};{}M", from.0 + 1, from.1 + 1).as_bytes())?;
        let steps = 4;
        for i in 1..=steps {
            let x = from.0 as i32 + (to.0 as i32 - from.0 as i32) * i / steps;
            let y = from.1 as i32 + (to.1 as i32 - from.1 as i32) * i / steps;
            self.send(format!("\x1b[<{};{};{}M", button + 32, x + 1, y + 1).as_bytes())?;
        }
        self.send(format!("\x1b[<{button};{};{}m", to.0 + 1, to.1 + 1).as_bytes())
    }

    /// Screen cell of the first occurrence of `needle` starting within columns `[min_x, max_x)`.
    pub fn find_in(&self, needle: &str, min_x: u16, max_x: u16) -> Option<(u16, u16)> {
        self.text().lines().enumerate().find_map(|(y, line)| {
            line.match_indices(needle).find_map(|(byte, _)| {
                let x = crate::client::paint::text_width(&line[..byte]) as u16;
                (x >= min_x && x < max_x).then_some((x, y as u16))
            })
        })
    }

    /// Like `find_in`, but an error includes the screen for diagnosis.
    pub fn locate(&self, needle: &str, range: (u16, u16)) -> Result<(u16, u16)> {
        self.find_in(needle, range.0, range.1).with_context(|| format!("{needle:?} not visible in columns {range:?}\n--- screen ---\n{}", self.text()))
    }

    /// Screen cell of the first occurrence of `needle`.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.find_in(needle, 0, u16::MAX)
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Result<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status.exit_code());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        bail!("the process did not exit\n--- screen ---\n{}", self.text())
    }
}

/// What the end-to-end scenario drives: the TUI in a hidden pseudoconsole
/// ([`Console`]) or the GUI over its test hooks ([`GuiDriver`]). Input is
/// what a terminal sends: keys, text and SGR mouse reports.
pub trait Ui {
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Pastes text the way the front end receives a paste (the TUI: from its
    /// console; the GUI: Ctrl+V with the text on its clipboard).
    fn paste(&mut self, text: &str) -> Result<()>;
    /// The text the client put on the clipboard.
    fn clipboard(&self) -> Option<String> {
        crate::client::clipboard::get_text().ok()
    }
    /// The screen as text, one line per row.
    fn text(&self) -> String;
    fn wait_exit(&mut self, timeout: Duration) -> Result<u32>;

    /// Terminal modes the client asked its console for (the TUI only).
    fn mode(&self) -> String {
        String::new()
    }

    /// Saves the screen as a PNG when `CANVAS_GUI_SCREENS` names a folder (the GUI only).
    fn screenshot(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn wait(&self, what: &str, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(&self.text()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        bail!("timed out waiting for {what}\n--- screen ---\n{}", self.text())
    }

    fn click(&mut self, x: u16, y: u16) -> Result<()> {
        self.send(format!("\x1b[<0;{};{}M", x + 1, y + 1).as_bytes())?;
        self.send(format!("\x1b[<0;{};{}m", x + 1, y + 1).as_bytes())
    }

    /// Two clicks at one cell, sent together (the explicit start of a box or row).
    fn double_click(&mut self, x: u16, y: u16) -> Result<()> {
        let (down, up) = (format!("\x1b[<0;{};{}M", x + 1, y + 1), format!("\x1b[<0;{};{}m", x + 1, y + 1));
        self.send(format!("{down}{up}{down}{up}").as_bytes())
    }

    fn drag(&mut self, button: u8, from: (u16, u16), to: (u16, u16)) -> Result<()> {
        self.send(format!("\x1b[<{button};{};{}M", from.0 + 1, from.1 + 1).as_bytes())?;
        let steps = 4;
        for i in 1..=steps {
            let x = from.0 as i32 + (to.0 as i32 - from.0 as i32) * i / steps;
            let y = from.1 as i32 + (to.1 as i32 - from.1 as i32) * i / steps;
            self.send(format!("\x1b[<{};{};{}M", button + 32, x + 1, y + 1).as_bytes())?;
        }
        self.send(format!("\x1b[<{button};{};{}m", to.0 + 1, to.1 + 1).as_bytes())
    }

    /// Screen cell of the first occurrence of `needle` starting within columns `[min_x, max_x)`.
    fn find_in(&self, needle: &str, min_x: u16, max_x: u16) -> Option<(u16, u16)> {
        self.text().lines().enumerate().find_map(|(y, line)| {
            line.match_indices(needle).find_map(|(byte, _)| {
                let x = crate::client::paint::text_width(&line[..byte]) as u16;
                (x >= min_x && x < max_x).then_some((x, y as u16))
            })
        })
    }

    /// Like `find_in`, but an error includes the screen for diagnosis.
    fn locate(&self, needle: &str, range: (u16, u16)) -> Result<(u16, u16)> {
        self.find_in(needle, range.0, range.1).with_context(|| format!("{needle:?} not visible in columns {range:?}\n--- screen ---\n{}", self.text()))
    }

    fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.find_in(needle, 0, u16::MAX)
    }
}

impl Ui for Console {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        Console::send(self, bytes)
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        Console::paste(self, text)
    }

    fn text(&self) -> String {
        Console::text(self)
    }

    fn wait_exit(&mut self, timeout: Duration) -> Result<u32> {
        Console::wait_exit(self, timeout)
    }

    fn mode(&self) -> String {
        Console::mode(self)
    }

    fn wait(&self, what: &str, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Result<()> {
        Console::wait(self, what, timeout, predicate)
    }

    fn click(&mut self, x: u16, y: u16) -> Result<()> {
        Console::click(self, x, y)
    }

    fn drag(&mut self, button: u8, from: (u16, u16), to: (u16, u16)) -> Result<()> {
        Console::drag(self, button, from, to)
    }

    fn find_in(&self, needle: &str, min_x: u16, max_x: u16) -> Option<(u16, u16)> {
        Console::find_in(self, needle, min_x, max_x)
    }
}

/// The GUI client (`copilot-canvas-gui.exe --test-hooks`) driven over its
/// pipe: terminal input bytes become key and mouse events (mouse at cell
/// centres), the screen is the GUI's text rendering of its frame.
pub struct GuiDriver {
    child: std::process::Child,
    pipe: Mutex<(std::io::BufReader<canvas_common::pipe::PipeReader>, canvas_common::pipe::PipeWriter)>,
    _closer: canvas_common::pipe::PipeCloser,
    _stdout: std::process::ChildStdout,
}

impl GuiDriver {
    /// Starts the GUI next to this executable with `args`, a fixed cell grid
    /// and its stderr in `log`.
    pub fn start(args: &[String], env: &[(&str, &Path)], size: (u16, u16), log: &Path) -> Result<Self> {
        Self::start_sized(args, env, Some(size), log)
    }

    /// The same; `None`: the window's own size decides the grid (placements).
    pub fn start_sized(args: &[String], env: &[(&str, &Path)], size: Option<(u16, u16)>, log: &Path) -> Result<Self> {
        use std::io::BufRead;
        let exe = std::env::current_exe()?.with_file_name("copilot-canvas-gui.exe");
        anyhow::ensure!(exe.is_file(), "{} is missing (cargo build --release builds it)", exe.display());
        let mut command = std::process::Command::new(&exe);
        command.args(args).arg("--test-hooks");
        if let Some((cols, rows)) = size {
            command.args(["--size".to_owned(), format!("{cols}x{rows}")]);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::fs::File::create(log)?);
        let mut child = command.spawn().with_context(|| format!("cannot start {}", exe.display()))?;
        let stdout = child.stdout.take().context("GUI stdout")?;
        let mut reader = std::io::BufReader::new(stdout);
        let mut name = String::new();
        reader.read_line(&mut name)?;
        let name = name.trim().to_owned();
        if !name.starts_with(r"\\.\pipe\") {
            let _ = child.kill();
            bail!("the GUI did not start its test hooks; see {}", log.display());
        }
        let pipe = canvas_common::pipe::connect(&name, Duration::from_secs(10)).with_context(|| format!("cannot connect to {name}"))?;
        let (pipe_reader, writer, closer) = pipe.split()?;
        Ok(Self { child, pipe: Mutex::new((std::io::BufReader::new(pipe_reader), writer)), _closer: closer, _stdout: reader.into_inner() })
    }

    /// One command and its answer.
    pub fn request(&self, command: serde_json::Value) -> Result<serde_json::Value> {
        let reply = self.ask(command.clone())?;
        anyhow::ensure!(reply["ok"] == true, "{command} failed: {reply}");
        Ok(reply)
    }

    /// One command and its answer, whether it succeeded or not.
    pub fn ask(&self, command: serde_json::Value) -> Result<serde_json::Value> {
        use std::io::BufRead;
        let mut pipe = lock(&self.pipe);
        writeln!(pipe.1, "{command}")?;
        pipe.1.flush()?;
        let mut line = String::new();
        anyhow::ensure!(pipe.0.read_line(&mut line)? > 0, "the GUI closed its test hooks");
        Ok(serde_json::from_str(&line)?)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the process is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for GuiDriver {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
    }
}

impl Ui for GuiDriver {
    fn screenshot(&self, name: &str) -> Result<()> {
        if let Some(dir) = std::env::var_os("CANVAS_GUI_SCREENS") {
            let path = Path::new(&dir).join(format!("{name}.png"));
            self.request(serde_json::json!({ "cmd": "png", "path": path }))?;
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let mut keys = Vec::new();
        for event in vt_events(bytes) {
            if event.get("kind").is_some() {
                if !keys.is_empty() {
                    self.request(serde_json::json!({ "cmd": "keys", "keys": std::mem::take(&mut keys) }))?;
                }
                self.request(event)?;
            } else {
                keys.push(event);
            }
        }
        if !keys.is_empty() {
            self.request(serde_json::json!({ "cmd": "keys", "keys": keys }))?;
        }
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        self.request(serde_json::json!({ "cmd": "clipboard", "text": text }))?;
        self.request(serde_json::json!({ "cmd": "key", "char": "v", "mods": ["ctrl"] }))?;
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    fn clipboard(&self) -> Option<String> {
        // The hooks keep the GUI's last copy (they stand in when Windows' clipboard is locked).
        let platform = self.request(serde_json::json!({ "cmd": "platform" })).ok()?;
        platform["clipboard"]["text"].as_str().map(str::to_owned).or_else(|| crate::client::clipboard::get_text().ok())
    }

    fn text(&self) -> String {
        match self.request(serde_json::json!({ "cmd": "screen" })) {
            Ok(reply) => reply["lines"].as_array().map(|lines| lines.iter().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Result<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status.code().unwrap_or(-1) as u32);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let screen = self.text();
        let _ = self.child.kill();
        bail!("the GUI did not exit\n--- screen ---\n{screen}")
    }
}

/// Terminal input bytes as test-hook events: keys (`{"key"|"char", "mods"}`)
/// and mouse/wheel commands (SGR reports, at cell centres).
fn vt_events(bytes: &[u8]) -> Vec<serde_json::Value> {
    use serde_json::json;
    let chars: Vec<char> = String::from_utf8_lossy(bytes).chars().collect();
    let key = |name: &str, mods: &[&str]| json!({ "key": name, "mods": mods });
    let char_key = |c: char, mods: &[&str]| {
        let shift = c.is_ascii_uppercase() && !mods.contains(&"shift");
        let mut all: Vec<&str> = mods.to_vec();
        if shift {
            all.push("shift");
        }
        json!({ "char": c.to_string(), "mods": all })
    };
    let control = |c: char| -> Option<serde_json::Value> {
        Some(match c {
            '\r' | '\n' => key("Enter", &[]),
            '\t' => key("Tab", &[]),
            '\x7f' | '\x08' => key("Backspace", &[]),
            '\x1d' => char_key(']', &["ctrl"]),
            '\x1c' => char_key('\\', &["ctrl"]),
            '\x1f' => char_key('_', &["ctrl"]),
            '\0' => char_key(' ', &["ctrl"]),
            '\x01'..='\x1a' => char_key((c as u8 - 1 + b'a') as char, &["ctrl"]),
            _ => return None,
        })
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\x1b' {
            out.push(control(c).unwrap_or_else(|| char_key(c, &[])));
            i += 1;
            continue;
        }
        match chars.get(i + 1).copied() {
            None | Some('\x1b') => {
                out.push(key("Esc", &[]));
                i += 1;
            }
            Some('[') | Some('O') => {
                let ss3 = chars[i + 1] == 'O';
                let start = i + 2;
                let mut end = start;
                while end < chars.len() && !('\x40'..='\x7e').contains(&chars[end]) {
                    end += 1;
                }
                let params: String = chars[start..end.min(chars.len())].iter().collect();
                let fin = chars.get(end).copied().unwrap_or('~');
                i = end + 1;
                if let Some(sgr) = params.strip_prefix('<') {
                    let n: Vec<i64> = sgr.split(';').filter_map(|p| p.parse().ok()).collect();
                    let (b, x, y) = (n.first().copied().unwrap_or(0), n.get(1).copied().unwrap_or(1), n.get(2).copied().unwrap_or(1));
                    let mut mods = Vec::new();
                    for (bit, name) in [(4, "shift"), (8, "alt"), (16, "ctrl")] {
                        if b & bit != 0 {
                            mods.push(name);
                        }
                    }
                    let (px, py) = ((x - 1) as f64 + 0.5, (y - 1) as f64 + 0.5);
                    let button = ["left", "middle", "right", "none"][(b & 3) as usize];
                    if b & 64 != 0 {
                        let dir = ["up", "down", "left", "right"][(b & 3) as usize];
                        out.push(json!({ "cmd": "wheel", "kind": "wheel", "dir": dir, "x": px, "y": py, "mods": mods }));
                        continue;
                    }
                    let kind = match (b & 32 != 0, fin) {
                        (true, _) if button == "none" => "move",
                        (true, _) => "drag",
                        (false, 'M') => "down",
                        _ => "up",
                    };
                    out.push(json!({ "cmd": "mouse", "kind": kind, "button": button, "x": px, "y": py, "mods": mods }));
                    continue;
                }
                let mut parts = params.split(';');
                let number: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
                let modifier: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1).max(1) - 1;
                let mut mods = Vec::new();
                for (bit, name) in [(1, "shift"), (2, "alt"), (4, "ctrl")] {
                    if modifier & bit != 0 {
                        mods.push(name);
                    }
                }
                let name = match (fin, number) {
                    ('A', _) => "Up",
                    ('B', _) => "Down",
                    ('C', _) => "Right",
                    ('D', _) => "Left",
                    ('H', _) => "Home",
                    ('F', _) => "End",
                    ('Z', _) => "BackTab",
                    ('P', _) if ss3 => "F1",
                    ('Q', _) if ss3 => "F2",
                    ('R', _) if ss3 => "F3",
                    ('S', _) if ss3 => "F4",
                    ('~', 2) => "Insert",
                    ('~', 3) => "Delete",
                    ('~', 5) => "PageUp",
                    ('~', 6) => "PageDown",
                    ('~', 15) => "F5",
                    ('~', 17) => "F6",
                    ('~', 18) => "F7",
                    ('~', 19) => "F8",
                    ('~', 20) => "F9",
                    ('~', 21) => "F10",
                    ('~', 23) => "F11",
                    ('~', 24) => "F12",
                    _ => continue,
                };
                out.push(key(name, &mods));
            }
            Some(next) => {
                // Alt + key.
                let mut event = control(next).unwrap_or_else(|| char_key(next, &[]));
                if let Some(mods) = event["mods"].as_array_mut() {
                    mods.push("alt".into());
                }
                out.push(event);
                i += 2;
            }
        }
    }
    out
}

/// Sessions in an agent's history, where counting is cheap.
fn session_count(kind: AgentKind, instance: &Instance) -> Option<u64> {
    match kind {
        AgentKind::Copilot => std::fs::read_dir(&instance.sessions_dir)
            .ok()
            .map(|d| d.filter_map(|e| e.ok()).filter(|e| crate::index::is_session_id(&e.file_name().to_string_lossy())).count() as u64),
        AgentKind::OpenCode => canvas_agents::agents::opencode_history::root_session_count(&kind.provider().history_root()),
        AgentKind::Claude => {
            let projects = std::fs::read_dir(kind.provider().history_root()).ok()?;
            let transcripts = |dir: &Path| std::fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "jsonl")).count()).unwrap_or(0);
            Some(projects.filter_map(|e| e.ok()).map(|e| transcripts(&e.path()) as u64).sum())
        }
    }
}

pub fn doctor(instance: &Instance) -> Result<()> {
    let mut ok = true;
    let mut line = |good: bool, label: &str, detail: String| {
        ok &= good;
        println!("{} {label:<20} {detail}", if good { "✓" } else { "✗" });
    };
    let mut installed = 0;
    for kind in agents::ALL {
        let provider = kind.provider();
        let label = kind.label();
        match provider.program() {
            Ok(program) => {
                installed += 1;
                let root = if kind == AgentKind::Copilot { instance.sessions_dir.clone() } else { provider.history_root() };
                let count = session_count(kind, instance).map(|n| format!("{n} sessions in ")).unwrap_or_default();
                line(true, label, format!("{program} · history: {count}{}", root.display()));
            }
            Err(_) => println!("- {label:<20} not installed"),
        }
    }
    if installed == 0 {
        line(false, "Agents", "no agent CLI found: install GitHub Copilot CLI, OpenCode or Claude Code".into());
    }
    let configured = canvas_common::settings::Settings::load(&instance.home).agent;
    let source = if std::env::var("COPILOT_CANVAS_AGENT").ok().and_then(|v| AgentKind::parse(&v)).is_some() {
        "COPILOT_CANVAS_AGENT".to_owned()
    } else if configured.as_deref().and_then(AgentKind::parse).is_some() {
        "settings.json".to_owned()
    } else {
        "auto-detected: first installed".to_owned()
    };
    let default = agents::machine_default(&instance.home);
    line(default.provider().program().is_ok(), "Default agent", format!("{} ({source}; a canvas can choose another in Settings)", default.label()));
    line(true, "Terminal engine", canvas_host::server::configured_engine(instance).name().to_owned());
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) => line(out.status.success(), "Git", String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        Err(error) => line(false, "Git", error.to_string()),
    }
    let started = Instant::now();
    let probe = (|| -> Result<String> {
        let mut console = Console::start(Path::new("cmd.exe"), &["/d".into(), "/c".into(), "echo CANVAS_CONPTY_OK".into()], 60, 10)?;
        console.wait("ConPTY output", Duration::from_secs(10), |t| t.contains("CANVAS_CONPTY_OK"))?;
        let elapsed = started.elapsed().as_millis();
        let _ = console.wait_exit(Duration::from_secs(5));
        Ok(format!("output rendered in {elapsed} ms"))
    })();
    match probe {
        Ok(detail) => line(true, "ConPTY + emulator", detail),
        Err(error) => line(false, "ConPTY + emulator", format!("{error:#}")),
    }
    match crate::client::conn::try_connect(instance) {
        Ok(conn) => {
            let same = conn.service_build == crate::paths::build_id();
            line(true, "Terminal service", format!("running ({})", if same { "same build" } else { "different build; restart when convenient" }));
        }
        Err(_) => line(true, "Terminal service", "not running (starts automatically)".into()),
    }
    let (workspaces, warnings) = crate::vscode::scan(&crate::vscode::default_roots());
    let copilot: usize = workspaces.iter().map(|w| w.sessions.len()).sum();
    let unreadable = if warnings.is_empty() { String::new() } else { format!(", {} unreadable", warnings.len()) };
    line(true, "VS Code agents", format!("{} workspaces, {copilot} Copilot CLI agent entries{unreadable}", workspaces.len()));
    line(true, "Data directory", instance.home.display().to_string());
    let automations = crate::factory_cli::configured_dir(&instance.home, None);
    for (mark, label, detail) in crate::remote_cli::doctor_lines(instance, &automations) {
        match mark {
            crate::remote_cli::Mark::Ok => line(true, label, detail),
            crate::remote_cli::Mark::Fail => line(false, label, detail),
            crate::remote_cli::Mark::Info => println!("- {label:<20} {detail}"),
        }
    }
    if !ok {
        bail!("some checks failed");
    }
    Ok(())
}

pub(crate) fn load(path: &Path) -> Result<Doc> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub(crate) fn wait_doc(path: &Path, what: &str, predicate: impl Fn(&Doc) -> bool) -> Result<Doc> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(doc) = load(path)
            && predicate(&doc)
        {
            return Ok(doc);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("canvas never satisfied: {what}\n{}", std::fs::read_to_string(path).unwrap_or_default())
}

/// OpenCode's recent-models file (`$XDG_STATE_HOME/opencode/model.json`).
fn opencode_recent_models() -> std::path::PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local").join("state"));
    state.join("opencode").join("model.json")
}

/// The flag that names an agent's model.
fn model_flag(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::OpenCode => "-m",
        AgentKind::Copilot | AgentKind::Claude => "--model",
    }
}

/// The agent the new-session dialog on screen has chosen (`◉ <agent>`).
fn dialog_agent(screen: &str) -> Result<AgentKind> {
    crate::client::agents::ALL.into_iter().find(|k| screen.contains(&format!("◉ {}", k.label()))).with_context(|| format!("no agent chosen in the dialog\n--- screen ---\n{screen}"))
}

/// Name of the session the real-agent check creates.
const CHECK_NAME: &str = "Canvas agent check";

/// Real-agent check (no prompt is ever submitted, so nothing is spent): on a
/// fresh canvas the agent's recent sessions are imported, its configuration
/// views load, a new session starts the real TUI with allow-all and takes
/// typing, everything stops gracefully, and a session the test created is
/// removed when it holds no prompt. `agent`: copilot, opencode or claude
/// (default: the detected default agent).
pub fn agent(agent: Option<&str>) -> Result<()> {
    let kind = match agent {
        Some(name) => AgentKind::parse(name).with_context(|| format!("unknown agent {name:?} (copilot, opencode or claude)"))?,
        None => agents::default_agent(),
    };
    let program = kind.provider().program().with_context(|| format!("{} is not installed on this machine", kind.label()))?;
    let root = std::env::temp_dir().join(format!("copilot-canvas-check-{}", uuid::Uuid::new_v4()));
    let work = root.join("work");
    let home = root.join("home");
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(&home)?;
    // The machine setting picks the agent for new sessions on the fresh canvas.
    canvas_common::settings::Settings { agent: Some(kind.id().to_owned()), ..Default::default() }.save(&home)?;
    let mut env: Vec<(&str, std::path::PathBuf)> = Vec::new();
    if kind == AgentKind::Claude {
        // Claude Code stops at its folder-trust and bypass-mode dialogs in a new folder: it runs
        // against a throw-away configuration that trusts the work folder, so ~/.claude stays untouched.
        let config = root.join("claude-config");
        std::fs::create_dir_all(&config)?;
        let trusted = work.display().to_string().replace('\\', "/");
        let seed = serde_json::json!({ "hasCompletedOnboarding": true, "theme": "dark", "bypassPermissionsModeAccepted": true, "projects": { trusted: { "hasTrustDialogAccepted": true } } });
        std::fs::write(config.join(".claude.json"), seed.to_string())?;
        env.push(("CLAUDE_CONFIG_DIR", config));
    }
    if kind == AgentKind::OpenCode {
        // OpenCode 1.19 refetches its model list at start-up when its cache is
        // stale and can hang if asked to quit while that request is pending (the
        // canvas then kills it after 5 s and still reports a stop; unit-tested in
        // canvas-host). The check of the graceful stop must not depend on the network.
        env.push(("OPENCODE_DISABLE_MODELS_FETCH", std::path::PathBuf::from("1")));
    }
    let instance = Instance::new(Some(format!("check{}", std::process::id())), Some(home.clone()), None)?;
    println!("Copilot Canvas real-agent check: {} ({program}); no prompt is submitted", kind.label());
    let started_ms = crate::paths::now_ms();
    // OpenCode may add a model given with -m to its recent models (its default
    // outside the canvas): that list is put back afterwards if it changed.
    let recent = (kind == AgentKind::OpenCode).then(opencode_recent_models).map(|path| (std::fs::read(&path).ok(), path));
    let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    let result = agent_scenario(&instance, &work, kind, &env);
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(7));
    }
    if let Some((Some(before), path)) = recent
        && std::fs::read(&path).ok().as_ref() != Some(&before)
    {
        match std::fs::write(&path, before) {
            Ok(()) => println!("  · put OpenCode's recent models back ({})", path.display()),
            Err(error) => eprintln!("  · could not put OpenCode's recent models back ({}): {error}", path.display()),
        }
    }
    let cleanup = cleanup_generated(kind, &instance, &home.join("canvas.json"), &work, started_ms);
    if result.is_ok() || std::env::var_os("CANVAS_SELFTEST_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        eprintln!("artifacts kept in {}", root.display());
    }
    result.and(cleanup)
}

/// Removes the session the check created when it holds no prompt.
fn cleanup_generated(kind: AgentKind, instance: &Instance, canvas: &Path, work: &Path, started_ms: u64) -> Result<()> {
    match kind {
        AgentKind::OpenCode => {
            // OpenCode creates a session on the first prompt only.
            let created = agents::opencode::discover_new_sessions(&work.display().to_string(), started_ms.saturating_sub(2_000));
            anyhow::ensure!(created.is_empty(), "OpenCode recorded session(s) {created:?} in the test folder; left in place");
            println!("  · no OpenCode session was created (none is until the first prompt)");
            return Ok(());
        }
        // Claude Code writes no transcript before the first prompt (and ran in a throw-away configuration).
        AgentKind::Claude => return Ok(()),
        AgentKind::Copilot => {}
    }
    let Ok(doc) = load(canvas) else { return Ok(()) };
    let Some(node) = doc.nodes.iter().find(|n| n.name == CHECK_NAME) else { return Ok(()) };
    let dir = instance.sessions_dir.join(&node.id);
    if !dir.is_dir() {
        return Ok(());
    }
    let meta = std::fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default();
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    anyhow::ensure!(meta.contains(&node.id), "refusing to delete {}: metadata does not match", dir.display());
    anyhow::ensure!(!events.contains("\"user.message\""), "a prompt was recorded in {}; not deleting", dir.display());
    for _ in 0..20 {
        if !crate::service::session_live(&instance.sessions_dir, &node.id) && std::fs::remove_dir_all(&dir).is_ok() {
            println!("  · removed the generated test session {}", &node.id[..8]);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("could not remove the generated test session {}", dir.display())
}

/// Words an agent's TUI shows once it is up (or a dialog that stops it).
pub(crate) fn agent_ui_up(kind: AgentKind, text: &str) -> bool {
    match kind {
        AgentKind::Copilot => text.contains("Copilot v") || text.contains("trust") || text.contains("Trust") || text.contains("❯"),
        AgentKind::OpenCode => text.contains("ctrl+p") || text.contains("Ask anything") || text.contains("tab agents"),
        AgentKind::Claude => text.contains("Claude Code v") || text.contains("bypass permissions") || text.contains("? for shortcuts") || text.contains("Select login method"),
    }
}

/// A screen that waits for an answer before the agent takes input (a trust or sign-in dialog).
fn agent_blocked(kind: AgentKind, text: &str) -> Option<&'static str> {
    match kind {
        AgentKind::Copilot if text.contains("trust") || text.contains("Trust") => Some("Copilot asks to trust the folder"),
        // Not being signed in only stops prompts; its input box still takes typing.
        AgentKind::Claude if text.contains("Select login method") || text.contains("Do you trust") => Some("Claude Code shows a sign-in or trust dialog"),
        _ => None,
    }
}

/// The flag that makes an agent run tools without asking.
fn allow_all_flag(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Copilot => "--allow-all",
        AgentKind::OpenCode => "--auto",
        AgentKind::Claude => "--dangerously-skip-permissions",
    }
}

fn agent_scenario(instance: &Instance, work: &Path, kind: AgentKind, env: &[(&str, &Path)]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = vec![
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--cwd".into(),
        work.to_string_lossy().into_owned(),
    ];
    let label = kind.label();
    let canvas = instance.home.join("canvas.json");
    let mut ui = Console::start_with_env(&exe, &args, env, 170, 46)?;
    ui.wait("the canvas", Duration::from_secs(30), |t| t.contains("Copilot Canvas"))?;
    step(&format!("recent {label} sessions are indexed and placed on a fresh canvas"));
    let throwaway = env.iter().any(|(k, _)| *k == "CLAUDE_CONFIG_DIR");
    let deadline = Instant::now() + Duration::from_secs(if throwaway { 5 } else { 60 });
    let mut imported = None;
    while Instant::now() < deadline {
        let sessions = load(&canvas).ok().map(|d| d.nodes.into_iter().filter(|n| n.kind == crate::client::doc::NodeKind::Session).collect::<Vec<_>>());
        if let Some(doc) = sessions.filter(|n| !n.is_empty()) {
            imported = Some(doc);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    match imported {
        Some(nodes) => {
            anyhow::ensure!(nodes.len() <= 8, "auto-import added {} boxes", nodes.len());
            anyhow::ensure!(nodes.iter().all(|n| agents::agent_of(n) == kind), "auto-import took another agent's sessions: {:?}", nodes.iter().map(|n| &n.agent).collect::<Vec<_>>());
            println!("    {} most recent {label} sessions imported", nodes.len());
        }
        None if throwaway => println!("    (the throw-away {label} configuration has no history; skipped)"),
        None => println!("    (no {label} sessions in its history; skipped)"),
    }
    step("a session that is live elsewhere is shown as such and never duplicated");
    let deadline = Instant::now() + Duration::from_secs(10);
    while ui.find_in("↗", SIDEBAR.0, SIDEBAR.1).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((x, y)) = ui.find_in("↗", SIDEBAR.0, SIDEBAR.1) {
        // A click only shows it; the explicit start (double-click) is refused.
        ui.click(x + 2, y)?;
        ui.wait("its details panel", Duration::from_secs(5), |t| t.contains("open elsewhere") && !t.contains("▸ Resume"))?;
        ui.double_click(x + 2, y)?;
        ui.wait("refusal notice", Duration::from_secs(5), |t| t.contains("never starts a second copy"))?;
    } else {
        println!("    (no session is currently open elsewhere; skipped)");
    }
    if kind == AgentKind::Copilot {
        vs_code_steps(&mut ui, instance)?;
    }
    step(&format!("plugins / MCP / skills views load {label}'s configuration"));
    let header = format!("Agent: {label}");
    let loaded = |t: &str| t.contains(&header) && !t.contains("Loading from");
    ui.send(b"\x1b3")?;
    ui.wait("plugins view", Duration::from_secs(90), |t| loaded(t) && t.contains("Install plugin"))?;
    let rows = |ui: &Console| {
        let sidebar = |l: &str| l.chars().skip(SIDEBAR.0 as usize).take((SIDEBAR.1 - SIDEBAR.0) as usize).collect::<String>();
        ui.text().lines().map(sidebar).filter(|l| l.contains('●') || l.contains('○')).count()
    };
    let mut counts = vec![format!("{} plugins", rows(&ui))];
    ui.send(b"\x1b4")?;
    ui.wait("mcp view", Duration::from_secs(30), |t| loaded(t) && t.contains("Add MCP server"))?;
    counts.push(format!("{} MCP servers", rows(&ui)));
    ui.send(b"\x1b5")?;
    ui.wait("skills view", Duration::from_secs(30), |t| loaded(t) && t.contains("SKILLS") && t.contains("Add skill"))?;
    counts.push(format!("{} skills", rows(&ui)));
    println!("    {} (first screen of each)", counts.join(", "));
    for warning in ui.text().lines().filter_map(|l| l.split('⚠').nth(1)) {
        println!("    note: {}", warning.trim());
    }
    if kind == AgentKind::Copilot {
        migration_preview_step(&mut ui)?;
    }
    ui.send(b"\x1b1")?;
    step(&format!("new {label} session from the dialog renders the real {label} UI"));
    let id = launch_check_session(&mut ui, kind, &canvas)?;
    if kind == AgentKind::OpenCode {
        step("the service follows the new OpenCode TUI through its server (its session id arrives with the first prompt)");
        let following = format!("terminal {id} works in ");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !std::fs::read_to_string(instance.log_path()).unwrap_or_default().contains(&following) {
            anyhow::ensure!(Instant::now() < deadline, "the service never reached the OpenCode TUI's server");
            std::thread::sleep(Duration::from_millis(200));
        }
        let node = load(&canvas)?.nodes.into_iter().find(|n| n.id == id).context("box")?;
        anyhow::ensure!(node.agent_session.is_none(), "no session may exist before a prompt: {:?}", node.agent_session);
    }
    let flag = allow_all_flag(kind);
    step(&format!("new sessions start with {flag} (Settings → Permissions default)"));
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    let pid = log.lines().rev().find_map(|l| l.split(" pid ").nth(1)).and_then(|p| p.trim().parse::<u32>().ok()).context("launched pid in the service log")?;
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
    let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &query]).output()?;
    let command_line = String::from_utf8_lossy(&out.stdout).to_string();
    anyhow::ensure!(command_line.split_whitespace().any(|a| a == flag), "{label} was launched without {flag}: {command_line}");
    let (model_flag, initial) = (model_flag(kind), canvas_model::models::initial(kind));
    step(&format!("new sessions start with the initial default model ({model_flag} {initial}: Claude Opus 5.5), recorded on the box"));
    let words: Vec<&str> = command_line.split_whitespace().map(|w| w.trim_matches('"')).collect();
    anyhow::ensure!(words.windows(2).any(|w| w[0] == model_flag && w[1] == initial), "{label} was launched without {model_flag} {initial}: {command_line}");
    let node = load(&canvas)?.nodes.into_iter().find(|n| n.id == id).context("the check's box")?;
    anyhow::ensure!(node.model.as_deref() == Some(initial), "the box records its model: {:?}", node.model);
    println!("    {model_flag} {initial} on the command line and on the box");
    step(&format!("typing reaches {label}'s input (not submitted)"));
    let blocked = agent_blocked(kind, &ui.text());
    if let Some(reason) = blocked {
        println!("    ({reason}; skipped)");
    } else {
        let probe = "zqx typing probe";
        ui.send(probe.as_bytes())?;
        ui.wait(&format!("typed text echoed by {label}"), Duration::from_secs(15), |t| t.contains(probe))?;
        // Erase it again: never Enter (no prompt is ever submitted). Every
        // character must go: agents only exit on Ctrl+D with an empty input.
        ui.send(&vec![0x7f; probe.len()])?;
        ui.wait("typed text erased", Duration::from_secs(10), |t| !t.contains("zqx"))?;
    }
    if kind == AgentKind::Copilot && blocked.is_none() {
        step("mouse wheel over the terminal scrolls Copilot's timeline (local ! command, no prompt)");
        ui.send(b"!1..120 | % ToString 'canvasrow000'")?;
        std::thread::sleep(Duration::from_millis(400));
        ui.send(b"\r")?;
        ui.wait("command output", Duration::from_secs(30), |t| t.contains("canvasrow120"))?;
        std::thread::sleep(Duration::from_millis(800));
        let lowest = |text: &str| -> Option<u32> {
            text.lines().filter_map(|l| l.split("canvasrow").nth(1)).filter_map(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).min()
        };
        let before = lowest(&ui.text()).context("visible output")?;
        let (x, y) = ui.find("canvasrow120").context("output position")?;
        for notch in 0..3 {
            ui.send(format!("\x1b[<64;{};{}M", x + 2, y + 1).as_bytes())?;
            std::thread::sleep(Duration::from_millis(500));
            println!("    after notch {}: first visible line {:?}", notch + 1, lowest(&ui.text()));
        }
        ui.wait("earlier lines after wheel up", Duration::from_secs(10), |t| lowest(t).is_some_and(|n| n < before))?;
        println!("    first visible line went from {before} to {}", lowest(&ui.text()).unwrap_or(0));
    } else {
        step("mouse wheel over the terminal");
        println!("    (needs output that only a prompt would produce in {label}; skipped)");
    }
    step(&format!("stop all and quit ({label} exits gracefully)"));
    quit_and_check_exit(&mut ui, instance, kind, &id, blocked.is_some())?;
    println!("PASS");
    Ok(())
}

/// A new session of the check from the new-session dialog (keyboard only),
/// with the agent's MCP servers off; returns its box id once the agent's UI is up.
fn launch_check_session(ui: &mut Console, kind: AgentKind, canvas: &Path) -> Result<String> {
    let label = kind.label();
    ui.send(b"\x1d")?;
    let focus = ui.text();
    if focus.contains("Typing goes to the terminal") {
        ui.send(b"\x1d")?;
    }
    ui.send(b"n")?;
    let chosen = format!("◉ {label}");
    ui.wait("new session dialog with the agent chosen", Duration::from_secs(5), |t| t.contains("New session") && t.contains(&chosen))?;
    // The model it starts with: a remembered default (the paste check's free
    // model) or else the initial one.
    let expected = match load(canvas).ok().and_then(|d| d.defaults.model(kind).map(str::to_owned)) {
        Some(model) => format!("{model}  ▾ · remembered default"),
        None => format!("{}  ▾ · initial default", canvas_model::models::initial(kind)),
    };
    ui.wait("the model it starts with, before starting", Duration::from_secs(5), |t| t.contains(&expected))?;
    ui.send(CHECK_NAME.as_bytes())?;
    // The session starts with the agent's MCP servers off (the dialog's MCP
    // picker: N = none), so the check does not depend on them: OpenCode 1.19
    // can hang when asked to quit while it keeps reconnecting to an MCP server
    // that is not running (the canvas then kills it after 5 s and still
    // reports a stop; that path is unit-tested in canvas-host).
    // Name → Folder → Browse… → Recent → Agent → Model → MCP servers.
    for _ in 0..6 {
        ui.send(b"\t")?;
    }
    ui.send(b"\r")?;
    ui.wait("MCP picker", Duration::from_secs(30), |t| t.contains("MCP servers for the new session") && !t.contains("MCP servers.\u{2026}") && !t.contains("Loading"))?;
    ui.send(b"n")?;
    ui.send(b"\r")?;
    ui.wait("back in the new session dialog", Duration::from_secs(5), |t| t.contains("New session") && !t.contains("MCP servers for the new session"))?;
    ui.send(b"\t\t")?;
    ui.send(b"\r")?;
    let header = format!("{CHECK_NAME} · ");
    ui.wait(&format!("{label} UI"), Duration::from_secs(90), |t| t.contains(&header) && agent_ui_up(kind, t))?;
    // Let the TUI settle (OpenCode draws its home screen in steps).
    std::thread::sleep(Duration::from_secs(2));
    if std::env::var_os("CANVAS_SELFTEST_DEBUG").is_some() {
        println!("--- screen ---\n{}", ui.text());
    }
    load(canvas)?.nodes.iter().find(|n| n.name == CHECK_NAME).map(|n| n.id.clone()).context("the test session's box")
}

/// Stop all and quit; the agent of box `id` must exit gracefully (unless it
/// was `blocked` by a dialog).
fn quit_and_check_exit(ui: &mut Console, instance: &Instance, kind: AgentKind, id: &str, blocked: bool) -> Result<()> {
    let label = kind.label();
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
    ui.send(b"\x1b[C\r")?;
    ui.wait_exit(Duration::from_secs(15))?;
    // The service logs the exit; a forced stop would end it after 5 s with another code.
    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = format!("terminal {id} exited with ");
    let code = loop {
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        if let Some(code) = log.lines().rev().find_map(|l| l.split(&exited).nth(1).map(|c| c.trim().to_owned())) {
            break code;
        }
        anyhow::ensure!(Instant::now() < deadline, "{label} did not exit after the stop");
        std::thread::sleep(Duration::from_millis(200));
    };
    println!("    {label} exited with {code}");
    // The canvas reports a stop as a clean exit; the log says whether the
    // agent had to be killed when the grace period ran out.
    anyhow::ensure!(blocked || (code.starts_with("Some(0)") && !code.contains("killed")), "{label} did not exit gracefully ({code})");
    Ok(())
}

/// What `selftest agent --paste` pastes: a line, a blank line, an indented
/// line, a last line and a trailing line break (which must not send it).
const AGENT_PASTE: [&str; 4] = ["canvas paste line one", "", "    indented line three", "line four end"];

/// `selftest agent --paste`: multi-line text pasted into the real agent's
/// input box through the canvas (the TUI, as Windows Terminal delivers a
/// paste) arrives as lines and sends nothing. OpenCode runs with a free
/// model as every default (in case anything were sent) and must create no
/// session (it does with the first prompt); Claude Code runs in a throw-away
/// configuration and must write no transcript. The input is erased again and
/// the agent stopped; no prompt is ever submitted.
pub fn agent_paste(agent: Option<&str>) -> Result<()> {
    let kind = match agent {
        Some(name) => AgentKind::parse(name).with_context(|| format!("unknown agent {name:?} (copilot, opencode or claude)"))?,
        None => agents::default_agent(),
    };
    let program = kind.provider().program().with_context(|| format!("{} is not installed on this machine", kind.label()))?;
    let root = std::env::temp_dir().join(format!("copilot-canvas-paste-{}", uuid::Uuid::new_v4()));
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(&home)?;
    canvas_common::settings::Settings { agent: Some(kind.id().to_owned()), ..Default::default() }.save(&home)?;
    let model = crate::agent_prompt::DEFAULT_MODEL;
    let mut env: Vec<(&str, std::path::PathBuf)> = Vec::new();
    let mut doc = Doc::default();
    let claude_config = root.join("claude-config");
    match kind {
        AgentKind::Claude => {
            std::fs::create_dir_all(&claude_config)?;
            let trusted = work.display().to_string().replace('\\', "/");
            let seed = serde_json::json!({ "hasCompletedOnboarding": true, "theme": "dark", "bypassPermissionsModeAccepted": true, "projects": { trusted: { "hasTrustDialogAccepted": true } } });
            std::fs::write(claude_config.join(".claude.json"), seed.to_string())?;
            env.push(("CLAUDE_CONFIG_DIR", claude_config.clone()));
        }
        AgentKind::OpenCode => {
            // Were anything sent, it would go to a free model: `-m` on the command line and every default.
            doc.defaults.set_model(AgentKind::OpenCode, Some(model));
            env.extend(crate::agent_prompt::opencode_env(model).into_iter().map(|(k, v)| (k, std::path::PathBuf::from(v))));
        }
        AgentKind::Copilot => {}
    }
    std::fs::write(home.join("canvas.json"), serde_json::to_string_pretty(&doc)?)?;
    let instance = Instance::new(Some(format!("paste{}", std::process::id())), Some(home.clone()), None)?;
    println!("Copilot Canvas real-agent paste check: {} ({program}); nothing is submitted", kind.label());
    let started_ms = crate::paths::now_ms();
    let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    let result = agent_paste_scenario(&instance, &work, kind, &env, model, &claude_config);
    if let Err(error) = &result {
        eprintln!("FAILED: {error:#}");
    }
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(7));
    }
    // A session would mean a prompt was sent: it is reported (models, cost) and deleted.
    let cleanup = match kind {
        AgentKind::OpenCode => {
            let cli_env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.to_str().unwrap_or_default())).collect();
            let created = agents::opencode::discover_new_sessions(&work.display().to_string(), started_ms.saturating_sub(2_000));
            let deleted = crate::agent_prompt::cleanup(&program, &cli_env, model, &[&work], started_ms);
            if created.is_empty() { deleted } else { Err(anyhow::anyhow!("OpenCode recorded session(s) {created:?}: the paste was sent")) }
        }
        _ => Ok(()),
    };
    if (result.is_ok() && cleanup.is_ok()) || std::env::var_os("CANVAS_SELFTEST_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        eprintln!("artifacts kept in {}", root.display());
    }
    result.and(cleanup)?;
    println!("PASS");
    Ok(())
}

fn agent_paste_scenario(instance: &Instance, work: &Path, kind: AgentKind, env: &[(&str, &Path)], model: &str, claude_config: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), instance.home.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
    let label = kind.label();
    let canvas = instance.home.join("canvas.json");
    let mut ui = Console::start_with_env(&exe, &args, env, 170, 46)?;
    ui.wait("the canvas", Duration::from_secs(30), |t| t.contains("Copilot Canvas"))?;
    step(&format!("a new {label} session from the dialog (Enter), its input box focused"));
    let id = launch_check_session(&mut ui, kind, &canvas)?;
    if kind == AgentKind::OpenCode {
        let (_, command_line) = crate::agent_prompt::launched_command_line(instance, &id)?;
        crate::agent_prompt::may_prompt(&command_line, model).context("the paste goes only into an OpenCode that runs with a free model")?;
        println!("    OpenCode runs with -m {model}");
    }
    if let Some(reason) = agent_blocked(kind, &ui.text()) {
        bail!("{reason}: the input box cannot be reached\n--- screen ---\n{}", ui.text());
    }
    step(&format!("a multi-line paste (blank line, indentation, trailing line break) lands in {label}'s input as lines"));
    let modes = service_modes(instance, &id);
    println!("    the agent's terminal modes: {modes}");
    paste_into_agent(&mut ui, kind, &AGENT_PASTE)?;
    step("nothing was sent: the input box still holds the text and no conversation started");
    nothing_sent(&ui, instance, kind, &id, work, claude_config)?;
    erase_paste(&mut ui, &AGENT_PASTE)?;
    step("a short two-line paste with indentation (agents collapse longer ones); nothing sent");
    paste_into_agent(&mut ui, kind, &AGENT_SHORT_PASTE)?;
    nothing_sent(&ui, instance, kind, &id, work, claude_config)?;
    erase_paste(&mut ui, &AGENT_SHORT_PASTE)?;
    step(&format!("stop all and quit ({label} exits gracefully)"));
    quit_and_check_exit(&mut ui, instance, kind, &id, false)
}

/// A shorter paste: two lines, the second indented.
const AGENT_SHORT_PASTE: [&str; 2] = ["short paste a", "  short paste b"];

/// The line count of an agent's paste placeholder: OpenCode shows
/// `[Pasted ~N lines]`, Claude Code `[Pasted text #1 +M lines]` (M = N - 1).
fn placeholder_lines(text: &str) -> Option<usize> {
    let rest = &text[text.find("[Pasted")?..];
    let rest = &rest[..rest.find(']')?];
    let number = |marker: char| rest.split(marker).nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse::<usize>().ok();
    number('~').or_else(|| number('+').map(|extra| extra + 1))
}

/// Pastes `lines` (and a trailing line break) and checks they arrived: each
/// line on its own row, in order, with the blank lines between them, or the
/// agent's placeholder with the right line count.
fn paste_into_agent(ui: &mut Console, kind: AgentKind, lines: &[&str]) -> Result<()> {
    let label = kind.label();
    let (first, last) = (lines[0], lines[lines.len() - 1].trim());
    ui.paste(&(lines.join("\r\n") + "\r\n"))?;
    let shown = |t: &str| (t.contains(first) && t.contains(last)) || t.contains("[Pasted");
    ui.wait(&format!("the pasted text in {label}'s input"), Duration::from_secs(15), shown)?;
    std::thread::sleep(Duration::from_secs(2));
    let text = ui.text();
    let pane = |l: &str| l.chars().skip(RIGHT_PANE).collect::<String>().trim_end().to_owned();
    for (y, line) in text.lines().enumerate().filter(|(_, l)| lines.iter().any(|p| !p.is_empty() && l.contains(p.trim())) || l.contains("[Pasted")) {
        println!("    row {y:>2}: {}", pane(line));
    }
    if let Some(count) = placeholder_lines(&text) {
        anyhow::ensure!(count == lines.len(), "{label} shows a paste of {count} lines, not {}", lines.len());
        println!("    {label} shows the paste as a placeholder of {count} lines");
        return Ok(());
    }
    // Each line on its own row (blank lines are rows too), indentation kept.
    let rows: Vec<String> = text.lines().map(pane).collect();
    let col = |row: &str, needle: &str| row.find(needle).map(|b| row[..b].chars().count());
    let first_row = rows.iter().position(|l| l.contains(first)).context("the first pasted line")?;
    let base = col(&rows[first_row], first).unwrap_or(0);
    for (i, line) in lines.iter().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let row = rows.get(first_row + i).map(String::as_str).unwrap_or_default();
        let indent = line.len() - line.trim_start().len();
        let at = col(row, line.trim_start()).with_context(|| format!("pasted line {i} ({line:?}) is not on row {}\n{text}", first_row + i))?;
        anyhow::ensure!(at == base + indent, "pasted line {i} ({line:?}) starts at column {at}, not {}: {row:?}", base + indent);
    }
    println!("    every pasted line is on its own row, in order, indentation and blank lines kept");
    Ok(())
}

/// Nothing reached the agent as a message: the paste is still in the input
/// box and the agent recorded no conversation.
fn nothing_sent(ui: &Console, instance: &Instance, kind: AgentKind, id: &str, work: &Path, claude_config: &Path) -> Result<()> {
    std::thread::sleep(Duration::from_secs(2));
    let text = ui.text();
    anyhow::ensure!(text.contains("[Pasted") || text.contains(AGENT_PASTE[0]) || text.contains(AGENT_SHORT_PASTE[0]), "the pasted text left the input box\n{text}");
    match kind {
        AgentKind::OpenCode => {
            let created = agents::opencode::discover_new_sessions(&work.display().to_string(), crate::paths::now_ms().saturating_sub(600_000));
            anyhow::ensure!(created.is_empty(), "OpenCode created session(s) {created:?}: the paste was sent");
            println!("    no OpenCode session exists (one would with the first prompt)");
        }
        AgentKind::Claude => {
            let mut transcripts = Vec::new();
            collect_files(&claude_config.join("projects"), "jsonl", &mut transcripts);
            anyhow::ensure!(transcripts.is_empty(), "Claude Code wrote transcript(s) {transcripts:?}: the paste was sent");
            println!("    no Claude Code transcript exists (one would with the first prompt)");
        }
        AgentKind::Copilot => {
            let events = instance.sessions_dir.join(id).join("events.jsonl");
            anyhow::ensure!(!std::fs::read_to_string(&events).unwrap_or_default().contains("\"user.message\""), "Copilot recorded a message: the paste was sent");
            println!("    Copilot recorded no message");
        }
    }
    Ok(())
}

/// Erases a paste with Backspace (never Enter).
fn erase_paste(ui: &mut Console, lines: &[&str]) -> Result<()> {
    let count: usize = lines.iter().map(|l| l.chars().count() + 1).sum::<usize>() + 4;
    ui.send(&vec![0x7f; count])?;
    let (first, last) = (lines[0], lines[lines.len() - 1].trim());
    ui.wait("the input box empty again", Duration::from_secs(15), |t| !t.contains(first) && !t.contains(last) && !t.contains("[Pasted"))
}

/// Columns of the TUI's terminal pane at 170 columns (for printing its rows).
const RIGHT_PANE: usize = 95;

/// The terminal modes the service's engine reports for terminal `id` (bracketed paste, kitty keyboard).
fn service_modes(instance: &Instance, id: &str) -> String {
    let Ok(conn) = crate::client::conn::try_connect(instance) else { return "unknown (no service connection)".into() };
    let _ = conn.tx.send(crate::protocol::ClientMsg::Subscribe8 { ids: vec![id.to_owned()], focused: None });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(crate::protocol::ServiceMsg::Frame8(frame)) = conn.rx.recv_timeout(Duration::from_millis(200))
            && frame.id == id
        {
            let modes = frame.modes;
            let kitty = (modes >> crate::protocol::MODE_KITTY_SHIFT) & 0x1f;
            return format!(
                "bracketed paste {}, kitty keyboard {}",
                if modes & crate::protocol::MODE_BRACKETED_PASTE != 0 { "on" } else { "off" },
                if kitty != 0 { format!("on (flags {kitty})") } else { "off".into() }
            );
        }
    }
    "unknown (no frame)".into()
}

/// Files with extension `ext` below `dir`.
fn collect_files(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

/// VS Code sync and its idempotence (Copilot sessions only).
fn vs_code_steps(ui: &mut Console, instance: &Instance) -> Result<()> {
    step("VS Code sync adds agent sessions active in the last 48 h, grouped like the Agents window (read-only)");
    ui.send(b"v")?;
    ui.wait("sync summary", Duration::from_secs(60), |t| t.contains("VS Code sync: added") || t.contains("No VS Code"))?;
    let summary = ui.text().lines().find(|l| l.contains("VS Code sync: added")).map(|l| l.split("  ").next().unwrap_or(l).trim().to_owned());
    let added: usize = summary.as_deref().and_then(|s| s.split_whitespace().nth(4)).and_then(|n| n.parse().ok()).unwrap_or(0);
    if let Some(summary) = &summary {
        println!("    {summary}");
    }
    let canvas = instance.home.join("canvas.json");
    let doc = wait_doc(&canvas, "synced sessions saved", |d| d.nodes.iter().filter(|n| n.source.as_deref() == Some("vscode")).count() >= added)?;
    let groups = doc.groups.iter().filter(|g| g.source.as_deref().is_some_and(|s| s.starts_with("vscode-"))).count();
    let synced = doc.nodes.iter().filter(|n| n.source.as_deref() == Some("vscode")).count();
    println!("    {synced} VS Code sessions on the canvas in {groups} VS Code group(s)");
    for group in doc.groups.iter().filter(|g| g.source.as_deref().is_some_and(|s| s.starts_with("vscode-"))) {
        let members = doc.members(&group.id);
        let subs = members.iter().filter_map(|m| doc.node(m)).filter(|n| n.kind == crate::client::doc::NodeKind::Session && n.parent.is_some()).count();
        let agents = group.source.as_deref().is_some_and(|s| s.starts_with("vscode-group:"));
        println!("      {}{} · {} sessions ({subs} sub-sessions)", group.name, if agents { " [Agents-window group]" } else { "" }, members.iter().filter_map(|m| doc.node(m)).filter(|n| n.kind == crate::client::doc::NodeKind::Session).count());
    }
    step("a second sync is idempotent (nothing added, renamed, linked or moved)");
    ui.send(b"v")?;
    ui.wait("second sync", Duration::from_secs(60), |t| t.contains("VS Code sync: added 0 session(s)"))?;
    let again = ui.text().lines().find(|l| l.contains("VS Code sync: added 0")).unwrap_or_default().to_owned();
    anyhow::ensure!(!again.contains("renamed") && !again.contains("links") && !again.contains("moved"), "second sync changed boxes: {again}");
    Ok(())
}

/// The VS Code → Copilot migration preview (read-only).
fn migration_preview_step(ui: &mut Console) -> Result<()> {
    ui.send(b"\x1b7")?;
    ui.wait("vs code view", Duration::from_secs(5), |t| t.contains("Migrate skills"))?;
    step("migration preview lists VS Code items without changing anything");
    let (mx, my) = ui.locate("Migrate skills", SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("migration preview", Duration::from_secs(60), |t| t.contains("selected · Space toggle") || t.contains("Nothing to migrate"))?;
    let preview = ui.text();
    let count = |mark: &str| preview.lines().filter(|l| l.contains(mark)).count();
    println!("    preview: {} new, {} already in Copilot, {} need attention", count("[x]") + count("[ ]"), count("[=]"), count("[!]") + count("[?]"));
    ui.send(b"\x1b")?;
    ui.wait("preview closed", Duration::from_secs(5), |t| !t.contains("Space toggle · Enter apply"))?;
    Ok(())
}

/// Canvas columns at 170 columns with the default 30-column sidebar and 55% terminals.
const CANVAS: (u16, u16) = (34, 95);
/// Sidebar columns (right of the 3-column activity bar).
pub(crate) const SIDEBAR: (u16, u16) = (3, 33);

pub(crate) fn step(name: &str) {
    println!("  · {name}");
}

/// The copy-and-paste check, in a PowerShell terminal that has keyboard
/// focus (the self-test's scenario and the GUI parity suite): a drag over a
/// soft-wrapped line, a hard break, a blank line and indentation copies
/// exactly that text; a two-line paste into PowerShell runs nothing; a
/// four-line paste with a blank line, indentation and a trailing line break
/// reaches a program (the paste probe) as one bracketed paste without Enter.
pub(crate) fn copy_paste_check<U: Ui>(ui: &mut U) -> Result<String> {
    // A short prompt keeps the pasted lines on one row ("PS " as checks expect).
    ui.send(b"function prompt { 'PS > ' }\r")?;
    std::thread::sleep(Duration::from_millis(800));
    ui.send(b"Write-Output (('COPY' + 'TOP ') + ('wrap me ' * 20).Trim()), '  hard line', '', '    indented line', ('COPY' + 'END')\r")?;
    ui.wait("the block to copy", Duration::from_secs(10), |t| t.contains("COPYTOP") && t.contains("COPYEND"))?;
    // The prompt after the block may still scroll it: wait until it shows.
    ui.wait("the prompt after the block", Duration::from_secs(10), |t| {
        let lines: Vec<&str> = t.lines().collect();
        lines.iter().position(|l| l.contains("COPYEND")).is_some_and(|y| lines.get(y + 1).is_some_and(|l| l.contains("PS >")))
    })?;
    std::thread::sleep(Duration::from_millis(300));
    let (x0, y0) = ui.find("COPYTOP").context("COPYTOP")?;
    let (x1, y1) = ui.find("COPYEND").context("COPYEND")?;
    anyhow::ensure!(y1 >= y0 + 5, "the long line wraps over two rows or more (rows {y0}..{y1})\n{}", ui.text());
    let expected = format!("COPYTOP {}\r\n  hard line\r\n\r\n    indented line\r\nCOPYEND", "wrap me ".repeat(20).trim());
    ui.drag(0, (x0, y0), (x1 + 6, y1))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let copied = ui.clipboard();
        if copied.as_deref() == Some(expected.as_str()) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "copied {copied:?}\n  expected {expected:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // PowerShell (PSReadLine) has no bracketed paste: each line break arrives
    // as Alt+Enter, which it ignores; nothing runs.
    ui.paste("echo PASTE_A\r\necho PASTE_B\r\n")?;
    ui.wait("both pasted lines in PowerShell's input", Duration::from_secs(10), |t| t.contains("echo PASTE_Aecho PASTE_B"))?;
    std::thread::sleep(Duration::from_millis(1500));
    let text = ui.text();
    anyhow::ensure!(text.matches("PASTE_A").count() == 1 && text.matches("PASTE_B").count() == 1, "PowerShell ran a pasted line\n{text}");
    ui.send(b"\x1b")?;
    ui.wait("PowerShell's input cleared (Esc)", Duration::from_secs(5), |t| !t.contains("PASTE_B"))?;
    // A program with bracketed paste.
    let exe = std::env::current_exe()?;
    ui.send(format!("& '{}' paste-probe\r", exe.display()).as_bytes())?;
    ui.wait("the paste probe", Duration::from_secs(20), |t| t.contains(crate::paste_probe::READY))?;
    ui.paste("a1\r\n\r\n  b2\r\nc3\r\n")?;
    let pasted = "PASTE 4 lines: a1\\r\\r  b2\\rc3";
    ui.wait("one bracketed paste in the program", Duration::from_secs(10), |t| t.contains(pasted))?;
    std::thread::sleep(Duration::from_millis(500));
    let text = ui.text();
    anyhow::ensure!(!text.contains(crate::paste_probe::ENTER), "the program received an Enter\n{text}");
    ui.send(b"\x04")?;
    ui.wait("the probe ended (Ctrl+D)", Duration::from_secs(10), |t| t.contains("BYE"))?;
    Ok(format!("copied {} lines exactly; PowerShell ran no pasted line; the program got one bracketed paste of 4 lines and no Enter", expected.lines().count()))
}

/// The front end the self-test drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The TUI in a hidden pseudoconsole.
    Tui,
    /// `copilot-canvas-gui.exe` over its test hooks.
    Gui,
}

/// `remote`: `Some("loopback")` runs the same scenario with the client
/// connected over TCP (`--remote loopback`).
pub fn run(remote: Option<&str>, client: Client) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-selftest-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let sessions = root.join("sessions");
    let work = root.join("work");
    std::fs::create_dir_all(&sessions)?;
    std::fs::create_dir_all(&work)?;
    let instance = Instance::new(Some(format!("selftest{}", std::process::id())), Some(home), Some(sessions))?;
    // The copy-and-paste step uses the clipboard: yours comes back afterwards.
    let saved_clipboard = crate::client::clipboard::get_text().ok();
    let result = match client {
        Client::Tui => scenario(&instance, &work, remote, "tui", |args, env| Console::start_with_env(&std::env::current_exe()?, args, env, 170, 46)),
        Client::Gui => {
            let log = root.join("gui-stderr.log");
            scenario(&instance, &work, remote, "gui", |args, env| GuiDriver::start(args, env, (170, 46), &log))
        }
    };
    // Always stop the isolated test service and its shells.
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_millis(2500));
    }
    if let Some(text) = saved_clipboard {
        let _ = crate::client::clipboard::set_text(&text);
    }
    match &result {
        Ok(()) => {
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err(_) => eprintln!("artifacts kept in {}", root.display()),
    }
    result
}

/// The 28 steps, against any front end (client: its name in the service log;
/// start: launches it with the given arguments and environment).
fn scenario<U: Ui>(instance: &Instance, work: &Path, remote: Option<&str>, client: &str, start: impl Fn(&[String], &[(&str, &Path)]) -> Result<U>) -> Result<()> {
    let canvas = instance.home.join("demo-canvas.json");
    let mut args: Vec<String> = vec![
        "--demo".into(),
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--sessions-dir".into(),
        instance.sessions_dir.to_string_lossy().into_owned(),
        "--cwd".into(),
        work.to_string_lossy().into_owned(),
    ];
    if let Some(remote) = remote {
        args.extend(["--remote".into(), remote.to_owned()]);
    }
    let over = remote.map(|r| format!(", client over --remote {r}")).unwrap_or_default();
    let of = if client == "tui" { String::new() } else { format!(" of the {client}") };
    println!("Copilot Canvas self-test{of} (demo shells only; isolated data in {}{over})", instance.home.display());
    let copilot_home = instance.home.join("copilot-home");
    std::fs::create_dir_all(&copilot_home)?;
    let automations_db = copilot_home.join("data.db");
    crate::automations::write_sample_db(&automations_db)?;
    // `O` (open the other client) writes its command line here instead of starting it.
    let other_client = instance.home.join("other-client.txt");
    // Stand-in gh / az for the pull request steps (fixtures in fake-prs; polled every second),
    // and stand-in agent CLIs: a canvas-wide agent's run in the self-test never reaches a real agent.
    let fake_cli = instance.home.join("fake-cli");
    std::fs::create_dir_all(&fake_cli)?;
    for name in ["gh.exe", "az.exe", "copilot.exe", "opencode.exe", "claude.exe"] {
        std::fs::copy(std::env::current_exe()?, fake_cli.join(name))?;
    }
    let (fake_gh, fake_az, fake_prs) = (fake_cli.join("gh.exe"), fake_cli.join("az.exe"), instance.home.join("fake-prs"));
    let (fake_copilot, fake_opencode, fake_claude) = (fake_cli.join("copilot.exe"), fake_cli.join("opencode.exe"), fake_cli.join("claude.exe"));
    std::fs::create_dir_all(&fake_prs)?;
    let env = [
        ("COPILOT_HOME", copilot_home.as_path()),
        ("COPILOT_CANVAS_OTHER_CLIENT_DRYRUN", other_client.as_path()),
        // The dry run is only honoured in self-tests.
        ("COPILOT_CANVAS_SELFTEST", Path::new("1")),
        // Loop dots move whatever this machine's animation setting is.
        ("COPILOT_CANVAS_EFFECTS", Path::new("on")),
        ("COPILOT_CANVAS_GH", fake_gh.as_path()),
        ("COPILOT_CANVAS_AZ", fake_az.as_path()),
        (crate::fake_gh::ENV, fake_prs.as_path()),
        (crate::fake_copilot::ENV, Path::new("1")),
        ("COPILOT_CANVAS_PR_POLL_SECS", Path::new("1")),
        ("COPILOT_CANVAS_COPILOT", fake_copilot.as_path()),
        ("COPILOT_CANVAS_OPENCODE", fake_opencode.as_path()),
        ("COPILOT_CANVAS_CLAUDE", fake_claude.as_path()),
        ("COPILOT_CANVAS_FAKE_RUN_MS", Path::new("300")),
    ];
    let started = Instant::now();
    let mut ui = start(&args, &env)?;
    step("client starts and auto-starts the terminal service");
    ui.wait("the canvas", Duration::from_secs(20), |t| t.contains("Copilot Canvas") && t.contains("Build session canvas"))?;
    let startup = started.elapsed();
    if remote.is_some() {
        // The client really talks TCP: the service published its port and logged the connection.
        anyhow::ensure!(crate::client::conn::listening_port(instance).is_some(), "the service publishes its TCP port");
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        anyhow::ensure!(log.contains(&format!("connected ({client}, protocol 8")), "the service saw a remote {client} client:\n{log}");
    }
    if std::env::var_os("CANVAS_SELFTEST_DEBUG").is_some() {
        println!("terminal modes requested by the client console: {}", ui.mode());
        let (hx, hy) = ui.find(" ? ").context("help button")?;
        ui.click(hx + 1, hy)?;
        std::thread::sleep(Duration::from_millis(500));
        println!("after clicking help: help visible = {}", ui.text().contains("Copilot Canvas — help"));
        ui.send(b"\x1b")?;
    }

    step("sidebar lists the group and its sessions");
    ui.wait("sidebar tree", Duration::from_secs(5), |t| t.contains("Canvas project") && t.contains("Review implementation"))?;

    step("Enter opens a real native terminal for the selected session");
    ui.send(b"\r")?;
    ui.wait("PowerShell prompt", Duration::from_secs(20), |t| t.contains("PS "))?;
    ui.send(b"Write-Output ('CANVAS_' + 'TERMINAL_OK')\r")?;
    ui.wait("command output", Duration::from_secs(10), |t| t.contains("CANVAS_TERMINAL_OK"))?;
    step("the active terminal's box gets a halo on the canvas (heavy while typing)");
    let (bx, by) = ui.locate("Build session canvas", CANVAS)?;
    let halo_row = ui.text().lines().nth(by as usize - 1).unwrap_or_default().chars().skip(bx.saturating_sub(6) as usize).take(12).collect::<String>();
    anyhow::ensure!(halo_row.contains('━') || halo_row.contains('┏'), "no halo above the active box: {halo_row:?}");
    ui.screenshot("terminal-and-halo")?;

    step("Ctrl+] returns keyboard focus to the canvas");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus hints", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;

    step("O opens this canvas in the other client (same service and canvas; dry run)");
    ui.send(b"o")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let line = loop {
        if let Ok(line) = std::fs::read_to_string(&other_client) {
            break line;
        }
        anyhow::ensure!(Instant::now() < deadline, "O did not open the other client");
        std::thread::sleep(Duration::from_millis(50));
    };
    let parts: Vec<&str> = line.split('\u{1f}').collect();
    let (exe, rest) = parts.split_first().context("command line")?;
    let wants = if client == "tui" { "copilot-canvas-gui.exe" } else { "copilot-canvas.exe" };
    anyhow::ensure!(exe.ends_with(wants), "the {client} opened {exe}, not {wants}");
    anyhow::ensure!(client == "tui" || rest.first() == Some(&"--tui"), "the GUI opens the TUI with --tui: {rest:?}");
    let after = |flag: &str| rest.iter().position(|a| *a == flag).and_then(|i| rest.get(i + 1)).copied();
    anyhow::ensure!(after("--instance") == Some(instance.name.as_str()) && rest.contains(&"--demo"), "same instance and demo canvas: {rest:?}");
    anyhow::ensure!(after("--remote") == remote, "same connection ({remote:?}): {rest:?}");

    step("mouse drag moves a session together with its subagent");
    let before = wait_doc(&canvas, "initial save", |d| d.node("demo-main").is_some())?;
    let main = before.node("demo-main").context("demo node")?.clone();
    let agent = before.node("demo-main:explore").context("demo agent")?.clone();
    let (x, y) = ui.locate("Build session canvas", CANVAS)?;
    ui.drag(0, (x + 2, y + 1), (x + 8, y + 3))?;
    wait_doc(&canvas, "node and subagent moved by (6,2)", |d| {
        let (n, a) = (d.node("demo-main").unwrap(), d.node("demo-main:explore").unwrap());
        (n.x - main.x, n.y - main.y) == (6, 2) && (a.x - agent.x, a.y - agent.y) == (6, 2)
    })?;

    step("rename from the keyboard");
    ui.send(b"r")?;
    ui.wait("rename dialog", Duration::from_secs(5), |t| t.contains("Rename (canvas label)"))?;
    ui.send(b"\x15Renamed by selftest\r")?;
    wait_doc(&canvas, "rename saved", |d| d.node("demo-main").is_some_and(|n| n.name == "Renamed by selftest"))?;

    step("right-click opens a context menu; toolbar zoom buttons change zoom");
    let (x, y) = ui.locate("Renamed", CANVAS)?;
    ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", x + 3, y + 2, x + 3, y + 2).as_bytes())?;
    ui.wait("node context menu", Duration::from_secs(5), |t| t.contains("New subsession") && t.contains("Remove from canvas"))?;
    ui.screenshot("context-menu")?;
    ui.send(b"\x1b")?;
    ui.wait("menu closed", Duration::from_secs(5), |t| !t.contains("Remove from canvas"))?;
    let (zx, zy) = ui.locate(" − ", (60, 120))?;
    ui.click(zx + 1, zy)?;
    ui.wait("zoomed out", Duration::from_secs(5), |t| t.contains("zoom 50%"))?;
    ui.screenshot("zoom-50")?;
    let (zx, zy) = ui.locate(" + ", (60, 120))?;
    ui.click(zx + 1, zy)?;
    ui.wait("zoomed in", Duration::from_secs(5), |t| t.contains("zoom 100%"))?;

    step("new session dialog: folder prefilled, its model shown (a choice is remembered), Enter creates and launches");
    ui.send(b"n")?;
    ui.wait("new session dialog", Duration::from_secs(5), |t| t.contains("New session") && t.contains("Folder"))?;
    ui.wait("git probe", Duration::from_secs(10), |t| t.contains("Not a Git repository") || t.contains("Git:"))?;
    let kind = dialog_agent(&ui.text())?;
    let initial = canvas_model::models::initial(kind);
    ui.wait("the model it would start with", Duration::from_secs(5), |t| t.contains(&format!("{initial}  ▾ · initial default")))?;
    ui.screenshot("new-session-dialog")?;
    ui.send(b"Spawned session")?;
    // Another model, typed into the dialog's picker: it applies to this session and becomes the agent's default.
    let (mx, my) = ui.locate("▾ · initial default", (0, u16::MAX))?;
    ui.click(mx, my)?;
    ui.wait("the model picker", Duration::from_secs(5), |t| t.contains("Model of the new") && t.contains("Type a model id"))?;
    ui.send(b"selftest-model\r")?;
    ui.wait("the choice in the dialog", Duration::from_secs(5), |t| t.contains("selftest-model  ▾ · your choice"))?;
    // Model → MCP servers → First prompt, where Enter creates.
    ui.send(b"\t\t\r")?;
    let doc = wait_doc(&canvas, "spawned node, the model remembered", |d| d.nodes.iter().any(|n| n.name == "Spawned session") && d.defaults.model(kind) == Some("selftest-model"))?;
    let spawned = doc.nodes.iter().find(|n| n.name == "Spawned session").context("spawned")?.id.clone();
    ui.wait("spawned terminal", Duration::from_secs(20), |t| t.contains("Spawned session · ") && t.contains("PS "))?;

    step("subsession linked to its parent");
    ui.send(b"\x1d")?;
    ui.send(b"s")?;
    ui.wait("subsession dialog", Duration::from_secs(5), |t| t.contains("New subsession of"))?;
    ui.send(b"Child task\r")?;
    wait_doc(&canvas, "subsession parent link", |d| d.nodes.iter().any(|n| n.name == "Child task" && n.parent.as_deref() == Some(spawned.as_str())))?;

    step("right-drag rubber-band selection, then G groups the selection");
    ui.send(b"\x1d")?;
    ui.send(b"\x1b")?;
    ui.send(b"f")?;
    ui.wait("child visible", Duration::from_secs(5), |t| t.contains("Child task"))?;
    let (sx, sy) = ui.locate("Spawn", CANVAS)?;
    let (cx, cy) = ui.locate("Chil", CANVAS)?;
    let from =  (sx.min(cx).saturating_sub(3).max(CANVAS.0), sy.min(cy).saturating_sub(1));
    let to = (sx.max(cx) + 8, sy.max(cy) + 2);
    ui.drag(2, from, to)?;
    ui.wait("multi-selection notice", Duration::from_secs(5), |t| t.contains("sessions selected") || t.contains("boxes selected"))?;
    ui.send(b"g")?;
    ui.wait("group rename", Duration::from_secs(5), |t| t.contains("Rename group"))?;
    ui.send(b"\x15Selftest group\r")?;
    wait_doc(&canvas, "group around both sessions", |d| d.groups.iter().any(|g| g.name == "Selftest group" && d.members(&g.id).len() >= 2))?;

    step("a new group asks for its folder; Enter creates ai-docs, human-outputs and group instructions");
    ui.wait("group folder prompt", Duration::from_secs(5), |t| t.contains("Folder for “Selftest group”"))?;
    ui.send(b"\r")?;
    let doc = wait_doc(&canvas, "group folder saved", |d| d.groups.iter().any(|g| g.name == "Selftest group" && g.folder.is_some()))?;
    let group = doc.groups.iter().find(|g| g.name == "Selftest group").context("group")?;
    let folder = std::path::PathBuf::from(group.folder.clone().unwrap_or_default());
    anyhow::ensure!(folder.starts_with(instance.home.join("groups")), "suggested folder under the canvas home: {}", folder.display());
    for sub in ["ai-docs", "human-outputs", r".canvas\canvas-group.instructions.md"] {
        anyhow::ensure!(folder.join(sub).exists(), "{} exists", folder.join(sub).display());
    }
    ui.wait("group panel folder actions", Duration::from_secs(5), |t| t.contains("Open AI docs") && t.contains("Open human outputs"))?;

    step("tiled view shows both live shells");
    ui.send(b"t")?;
    ui.wait("tiled", Duration::from_secs(5), |t| t.contains("Tiled") && t.contains("CANVAS_TERMINAL_OK") && t.contains("Spawned session · "))?;
    ui.screenshot("tiled")?;

    step("sidebar click reveals and views a session (its live terminal gets the keyboard)");
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("focus on terminal", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("links in terminals: plain click focuses, Ctrl+click opens (missing path → notice, nothing launched)");
    ui.send(b"Write-Output ('probe ' + 'C:\\canvas-link-probe\\missing.txt' + ' end')\r")?;
    ui.wait("path printed", Duration::from_secs(10), |t| t.contains("probe C:\\canvas-link"))?;
    let (lx, ly) = ui.find("probe C:\\canvas-link-probe").context("printed path")?;
    let (lx, ly) = (lx + 10, ly + 1);
    ui.send(format!("\x1b[<0;{lx};{ly}M\x1b[<0;{lx};{ly}m").as_bytes())?;
    std::thread::sleep(Duration::from_millis(600));
    anyhow::ensure!(!ui.text().contains("does not exist"), "plain click must not open terminal links");
    ui.send(format!("\x1b[<16;{lx};{ly}M\x1b[<16;{lx};{ly}m").as_bytes())?;
    ui.wait("Ctrl+click resolved the link", Duration::from_secs(5), |t| t.contains("missing.txt does not exist"))?;

    step("multi-line copy keeps line breaks, blank lines and indentation and rejoins soft wraps; a multi-line paste is one paste (PowerShell runs nothing; a program sees no Enter)");
    let summary = copy_paste_check(&mut ui)?;
    println!("    {summary}");
    // The reattach step looks for this terminal's earlier output.
    ui.send(b"Clear-Host; Write-Output ('CANVAS_' + 'TERMINAL_OK')\r")?;
    ui.wait("command output", Duration::from_secs(10), |t| t.contains("CANVAS_TERMINAL_OK"))?;

    step("activity bar: Automations view lists Copilot automations and adds them to an Automations group");
    ui.send(b"\x1d")?;
    ui.send(b"\x1b2")?;
    ui.wait("automations list", Duration::from_secs(10), |t| t.contains("Morning triage") && t.contains("Cron job"))?;
    wait_doc(&canvas, "automation boxes", |d| {
        d.groups.iter().any(|g| g.is_automations()) && d.nodes.iter().filter(|n| n.kind == crate::client::doc::NodeKind::Automation).count() == 3
    })?;

    step("repeating automations get the animated loopback (enabled ones only)");
    ui.send(b"\x1b1")?;
    ui.wait("loop markers in the sidebar", Duration::from_secs(10), |t| t.contains("↻ Morning triage") && t.contains("↻ Watch builds"))?;
    anyhow::ensure!(!ui.text().contains("↻ Cron job"), "a disabled automation does not loop");
    anyhow::ensure!(ui.text().contains("↻"), "zoomed out: the ↻ icon marks looping boxes");
    // Zoom in until 100 % (the TUI steps 25 → 50 → 100 %; the GUI from wherever fitting left it).
    for _ in 0..4 {
        if ui.text().contains("zoom 100%") {
            break;
        }
        let (zx, zy) = ui.find(" −   +   ?").context("zoom buttons")?;
        ui.click(zx + 5, zy)?;
        std::thread::sleep(Duration::from_millis(150));
    }
    ui.wait("zoom 100%", Duration::from_secs(5), |t| t.contains("zoom 100%"))?;
    let (mx, my) = ui.locate("Morning triage", SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("loopback drawn beside the box", Duration::from_secs(5), |t| t.contains("↻ daily") && t.contains("◂"))?;
    ui.screenshot("automations-loops")?;
    // The lit dot moves along the arc between frames.
    // The 4×3 cells of the arc next to the box (bottom row ends in ◂).
    let arc = |text: &str| -> String {
        let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let Some((row, col)) = lines.iter().enumerate().find_map(|(r, l)| l.iter().position(|c| *c == '◂').map(|c| (r, c))) else { return String::new() };
        (row.saturating_sub(3)..=row).map(|r| lines[r].iter().skip(col).take(3).collect::<String>()).collect::<Vec<_>>().join("|")
    };
    let first = arc(&ui.text());
    let mut moved = false;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if arc(&ui.text()) != first {
            moved = true;
            break;
        }
    }
    anyhow::ensure!(moved, "the loop dot did not move: {first:?}");
    ui.send(b"\x1b2")?;
    ui.wait("automations list again", Duration::from_secs(10), |t| t.contains("Morning triage") && t.contains("Cron job"))?;

    step("clicking an automation shows its prompt, schedule and runs; Space toggles it in Copilot's database");
    let (ax, ay) = ui.locate("Morning triage", SIDEBAR)?;
    ui.click(ax, ay)?;
    ui.wait("automation details", Duration::from_secs(5), |t| t.contains("Summarize new bugs") && t.contains("daily at 08:30") && t.contains("boom"))?;
    let enabled = |key: &str| -> Result<bool> {
        Ok(crate::automations::load(&automations_db)?.iter().find(|a| a.key == key).context("automation")?.enabled)
    };
    ui.send(b" ")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while enabled("wf:w1")? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(!enabled("wf:w1")?, "Space disabled the automation");
    ui.send(b" ")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !enabled("wf:w1")? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(enabled("wf:w1")?, "Space re-enabled the automation\n{}", ui.text());

    step("Settings view edits the launch defaults (each agent's model)");
    ui.send(b"\x1b8")?;
    ui.wait("settings view", Duration::from_secs(5), |t| t.contains("New-session defaults") && t.contains("Reasoning effort"))?;
    // The model chosen in the dialog is this agent's default now; Settings changes it again.
    let row = crate::client::side::DefaultField::Model(kind).label();
    ui.wait("the remembered model under its agent", Duration::from_secs(5), |t| t.contains(row) && t.contains("selftest-model"))?;
    let (mx, my) = ui.locate(row, SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("model picker", Duration::from_secs(5), |t| t.contains(&format!("{} model for new sessions", kind.label())) && t.contains("Type a model id"))?;
    ui.send(b"claude-selftest\r")?;
    wait_doc(&canvas, "default model saved", |d| d.defaults.model(kind) == Some("claude-selftest"))?;

    step("VS Code view offers sync and migration");
    ui.send(b"\x1b7")?;
    ui.wait("vs code view", Duration::from_secs(5), |t| t.contains("Sync sessions now") && t.contains("Migrate skills"))?;
    ui.send(b"\x1b1")?;
    ui.wait("sessions view", Duration::from_secs(5), |t| t.contains("Canvas project"))?;
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("focus on terminal", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("detach: the client exits while sessions keep running");
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
    ui.send(b"\r")?;
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");

    step("reattach: a new client shows the same live terminal and its output");
    drop(ui);
    let mut ui = start(&args, &env)?;
    ui.wait("canvas again", Duration::from_secs(20), |t| t.contains("Renamed by selftest"))?;
    ui.wait("restored tiles with previous output", Duration::from_secs(10), |t| t.contains("CANVAS_TERMINAL_OK") && t.contains("Spawned session · "))?;
    // The model chosen before is still the default in the new client (it lives in the canvas).
    if ui.text().contains("Typing goes to the terminal") {
        ui.send(b"\x1d")?;
    }
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    ui.send(b"n")?;
    ui.wait("the remembered model in the dialog", Duration::from_secs(5), |t| t.contains(&format!("claude-selftest  ▾ · remembered default for {}", kind.label())))?;
    ui.send(b"\x1b")?;
    ui.wait("dialog closed", Duration::from_secs(5), |t| !t.contains("First prompt"))?;
    let (x, y) = ui.locate("Renam", CANVAS)?;
    ui.click(x, y)?;
    ui.wait("terminal focus after canvas click", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("a session's progress (OSC 9;4), title and bell reach the canvas");
    ui.send(b"$e=[char]27; Write-Host -NoNewline \"$e]9;4;3;0$([char]7)$e]0;Canvas intent$([char]7)\"\r")?;
    ui.wait("working from progress + intent in the pane header", Duration::from_secs(10), |t| {
        t.lines().any(|l| l.contains("Renamed by selftest · working · Canvas intent"))
    })?;
    ui.send(b"Write-Host -NoNewline \"$e]9;4;0;0$([char]7)\"\r")?;
    ui.wait("progress cleared", Duration::from_secs(10), |t| t.lines().any(|l| l.contains("Renamed by selftest · running · Canvas intent")))?;
    step("a Copilot session started inside a session's terminal appears as its linked subsession");
    {
        // A stand-in copilot.exe (a copy of ping) holding a session's in-use lock.
        let fake = work.join("fake-copilot");
        std::fs::create_dir_all(&fake)?;
        std::fs::copy(r"C:\Windows\System32\PING.EXE", fake.join("copilot.exe"))?;
        let child = uuid::Uuid::new_v4().to_string();
        let dir = instance.sessions_dir.join(&child);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("workspace.yaml"), format!("id: {child}\nname: Spawned tracker\ncwd: {}\n", work.display()))?;
        let command = format!(
            "$p = Start-Process -PassThru -NoNewWindow '{}' -ArgumentList '-n','40','127.0.0.1' -RedirectStandardOutput '{}'; New-Item -ItemType File \"{}\\inuse.$($p.Id).lock\" | Out-Null\r",
            fake.join("copilot.exe").display(),
            fake.join("ping.txt").display(),
            dir.display()
        );
        ui.send(command.as_bytes())?;
        let doc = wait_doc(&canvas, "spawned subsession linked to its parent", |d| {
            d.nodes.iter().any(|n| n.id == child && n.parent.as_deref() == Some("demo-main") && n.name == "Spawned tracker")
        })
        .with_context(|| format!("--- screen ---\n{}", ui.text()))?;
        anyhow::ensure!(doc.nodes.iter().filter(|n| n.id == child).count() == 1, "added once");
        ui.wait("subsession box on the canvas", Duration::from_secs(5), |t| t.contains("Spawned tracker") || t.contains("Spawned"))?;
    }

    step("folder browser + Git detection + new worktree session");
    let repo = work.join("repo");
    std::fs::create_dir_all(&repo)?;
    let git = |args: &[&str]| -> Result<()> {
        let status = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output()?;
        anyhow::ensure!(status.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&status.stderr));
        Ok(())
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&["-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid", "commit", "-q", "--allow-empty", "-m", "init"])?;
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    ui.send(b"n")?;
    ui.wait("new session dialog", Duration::from_secs(5), |t| t.contains("New session"))?;
    ui.send(b"Worktree session")?;
    ui.send(b"\t")?;
    ui.send(b"\x15")?;
    ui.send(work.to_string_lossy().as_bytes())?;
    ui.send(b"\t")?;
    ui.send(b"\r")?;
    ui.wait("folder browser", Duration::from_secs(5), |t| t.contains("Choose a folder") && t.contains("repo"))?;
    ui.send(b"repo")?;
    ui.send(b"\r")?;
    ui.wait("inside repo", Duration::from_secs(5), |t| t.contains("\\repo") && t.contains("↑ .."))?;
    ui.send(b"\x13")?;
    ui.wait("Git detected", Duration::from_secs(10), |t| t.contains("Git:") && t.contains("on main"))?;
    for _ in 0..4 {
        ui.send(b"\t")?;
    }
    ui.send(b" ")?;
    ui.wait("worktree options", Duration::from_secs(5), |t| t.contains("Base branch") && t.contains("worktree-session"))?;
    ui.send(b"\t")?;
    ui.send(b"\t")?;
    ui.send(b"\r")?;
    let doc = wait_doc(&canvas, "worktree session node", |d| {
        d.nodes.iter().any(|n| n.name == "Worktree session" && n.cwd.as_deref().is_some_and(|c| c.ends_with(r"repo.worktrees\worktree-session")))
    })?;
    let expected = std::path::PathBuf::from(doc.nodes.iter().find(|n| n.name == "Worktree session").and_then(|n| n.cwd.clone()).unwrap_or_default());
    // The prompt's end is enough: a narrow tile wraps the long temp path
    // (its length depends on the user name), so the full path may span rows.
    ui.wait("shell running inside the worktree", Duration::from_secs(30), |t| {
        t.contains(r"repo.worktrees\worktree-session") || t.contains(r"\worktree-session>")
    })?;
    let branches = std::process::Command::new("git").arg("-C").arg(&expected).args(["branch", "--show-current"]).output()?;
    anyhow::ensure!(String::from_utf8_lossy(&branches.stdout).trim() == "worktree-session", "worktree is on the new branch");

    view_and_start_steps(&mut ui, instance)?;
    recent_steps(&mut ui, instance)?;
    group_click_steps(&mut ui)?;
    report_steps(&mut ui, instance, work)?;
    layout_undo_steps(&mut ui, &canvas)?;
    area_selection_steps(&mut ui, &canvas)?;

    step("stop all and quit");
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
    ui.send(b"\x1b[C\r")?;
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");
    let doc = load(&canvas)?;
    anyhow::ensure!(doc.groups.len() >= 2, "both groups persisted");
    canvas_agent_steps(instance, work, &args, &env, &start)?;
    println!(
        "PASS in {:.1}s (first paint incl. service start: {} ms)",
        started.elapsed().as_secs_f64(),
        startup.as_millis()
    );
    Ok(())
}

/// Canvas-wide agents, on a canvas of its own (the demo has no agents): the
/// top bar's ◎ Agents opens their panel; a new Custom agent is one box in the
/// Canvas agents frame (the sidebar and the panel show the same one), its
/// files under the canvas's id in this instance's home; viewing it, even a
/// double-click, starts nothing. A Custom agent has no triggers: nothing runs.
fn canvas_agent_steps<U: Ui>(instance: &Instance, work: &Path, args: &[String], env: &[(&str, &Path)], start: &impl Fn(&[String], &[(&str, &Path)]) -> Result<U>) -> Result<()> {
    use crate::client::doc::{Group, Node};
    step("canvas-wide agents: ◎ Agents opens their panel; a new agent is one box in the Canvas agents frame with its files under the canvas's id; viewing it starts nothing");
    let canvas = instance.home.join("agents-canvas.json");
    let mut shell: Node = serde_json::from_value(serde_json::json!({ "id": "agents-shell", "kind": "session", "name": "Team shell", "x": 2, "y": 2 }))?;
    shell.cwd = Some(work.display().to_string());
    shell.command = Some(vec!["powershell.exe".into(), "-NoLogo".into(), "-NoProfile".into()]);
    shell.fresh = true;
    let mut doc = Doc::default();
    doc.nodes.push(shell);
    // A session that owns a pull request (never started here: no agent runs in the self-test).
    let mut owner: Node = serde_json::from_value(serde_json::json!({ "id": PR_OWNER_ID, "kind": "session", "name": "PR owner", "x": 2, "y": 30, "agent": "copilot" }))?;
    owner.cwd = Some(work.display().to_string());
    owner.fresh = true;
    doc.nodes.push(owner);
    doc.groups.push(Group { id: "g-team".into(), name: "Team".into(), x: 0, y: 0, w: 36, h: 8, ..Default::default() });
    std::fs::write(&canvas, serde_json::to_string_pretty(&doc)?)?;
    let mut args: Vec<String> = args.iter().filter(|a| *a != "--demo").cloned().collect();
    args.extend(["--canvas".into(), canvas.display().to_string()]);
    let mut ui = start(&args, env)?;
    ui.wait("the agents canvas", Duration::from_secs(20), |t| t.contains("Team shell") && t.contains("◎ Agents"))?;
    let (x, y) = ui.find("◎ Agents").context("the top bar's ◎ Agents")?;
    ui.click(x + 1, y)?;
    ui.wait("the canvas agents panel", Duration::from_secs(5), |t| t.contains("Canvas agents · 0") && t.contains("+ New canvas agent"))?;
    let (x, y) = ui.find("+ New canvas agent").context("its New button")?;
    ui.click(x + 1, y)?;
    ui.wait("the templates", Duration::from_secs(5), |t| t.contains("New canvas-wide agent: template") && t.contains("PR coordinator") && t.contains("Custom"))?;
    ui.send(b"Custom")?;
    ui.send(b"\r")?;
    ui.wait("the name prompt", Duration::from_secs(5), |t| t.contains("Name the Custom canvas-wide agent"))?;
    ui.send(b"\x15Selftest agent\r")?;
    ui.wait("its panel", Duration::from_secs(10), |t| t.contains("◎ Selftest agent") && t.contains("canvas-wide agent ·"))?;
    let doc = wait_doc(&canvas, "the agent in its frame", |d| d.nodes.iter().any(|n| n.name == "Selftest agent" && n.is_canvas_agent()) && d.canvas_frame().is_some())?;
    let node = doc.nodes.iter().find(|n| n.name == "Selftest agent").context("the agent")?.clone();
    let frame = doc.canvas_frame().context("the frame")?;
    let passive = node.passive.clone().context("passive")?;
    anyhow::ensure!(passive.group == frame.id, "the agent belongs to the Canvas agents frame");
    let canvas_id = doc.canvas_id.clone().context("the service gave the canvas an id")?;
    let dir = canvas_agents::canvas_wide::canvas_dir(&instance.home, &canvas_id);
    anyhow::ensure!(canvas_agents::canvas_wide::agent_paths(&dir, &passive.slug).rules.is_file(), "its rules under {}", dir.display());
    // The sidebar row and the box are the same agent; a double-click on the box starts nothing.
    let (sx, sy) = ui.locate("Selftest agent", SIDEBAR)?;
    ui.click(sx, sy)?;
    ui.wait("its panel from the sidebar", Duration::from_secs(5), |t| t.contains("◎ Selftest agent") && t.contains("Open chat"))?;
    let (bx, by) = ui.locate("Selftest agent", CANVAS)?;
    ui.double_click(bx, by)?;
    std::thread::sleep(Duration::from_millis(800));
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    anyhow::ensure!(!log.contains(&format!("launched {} ", node.id)), "viewing the agent launched it");
    anyhow::ensure!(ui.text().contains("◎ Selftest agent"), "its panel stays\n{}", ui.text());
    pr_steps(&mut ui, instance)?;
    review_steps(&mut ui, instance)?;
    ui.send(b"q")?;
    std::thread::sleep(Duration::from_millis(300));
    if ui.text().contains("Detach (keep running)") {
        ui.send(b"\r")?;
    }
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");
    Ok(())
}

/// The box that owns the self-test's pull request (never started).
const PR_OWNER_ID: &str = "5e1f7e57-0000-4000-8000-0000000000a1";

/// `copilot-canvas ARGS` in this instance's home, as you (no canvas session).
fn canvas_cli(home: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(std::env::current_exe()?).args(args).args(["--home", &home.display().to_string()]).env_remove("COPILOT_CANVAS_SESSION").output()?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    anyhow::ensure!(output.status.success(), "copilot-canvas {} failed: {text}", args.join(" "));
    Ok(text)
}

/// Pull request coordination without any agent run (WI-014): PR
/// coordination turned on from an agent's panel; a hand-off (as you, for the
/// "PR owner" box) is tracked and polled with the stand-in gh (the first poll
/// is the baseline); three new comments become a `pr.comment.new` event; a
/// follow-up for the owner, which is not running, is blocked with what to do
/// (Resume owner, Reassign, Dismiss) and reported; Dismiss works; a merged
/// pull request is never polled again; nothing starts.
fn pr_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    use canvas_agents::prs::{DispatchState, MonitorStatus, PrRef, PrState, Registry};
    step("pull requests: a hand-off is tracked and polled (stand-in gh); new comments become an event; a follow-up for a stopped owner is blocked (Resume owner, Reassign, Dismiss) and reported; merged stops polling; nothing starts");
    let home = &instance.home;
    let fixtures = home.join("fake-prs");
    const URL: &str = "https://github.com/selftest/app/pull/7";
    let (x, y) = ui.find("PRs…").context("the agent's PRs… button")?;
    ui.click(x + 1, y)?;
    ui.wait("the PR coordination prompt", Duration::from_secs(5), |t| t.contains("PR coordination"))?;
    ui.send(b"\x15on every 5m manual\r")?;
    ui.wait("its PR settings", Duration::from_secs(10), |t| t.contains("on: every 5 min"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let canvas = loop {
        let routing = canvas_agents::cli::routing(home);
        if routing.passive.iter().any(|p| p.prs.is_some()) {
            break routing.canvas;
        }
        anyhow::ensure!(Instant::now() < deadline, "PR coordination never reached the service");
        std::thread::sleep(Duration::from_millis(200));
    };
    let wait_registry = |what: &str, ok: &dyn Fn(&Registry) -> bool| -> Result<Registry> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let registry = canvas_agents::prs_cli::load(home, &canvas);
            if ok(&registry) {
                return Ok(registry);
            }
            anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}: {}", serde_json::to_string(&registry).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let pr = PrRef::parse(URL).map_err(|e| anyhow::anyhow!(e))?;
    let key = pr.key();
    let mut fixture = crate::fake_gh::Fixture::open("Selftest: retry uploads", "selftest-dev");
    fixture.comment(1, "ana", "an old comment");
    crate::fake_gh::write_fixture(&fixtures, &pr, &fixture)?;
    let out = canvas_cli(home, &["pr", "track", URL, "--session", "PR owner"])?;
    anyhow::ensure!(out.contains("Tracking selftest/app#7 for “PR owner”"), "{out}");
    wait_registry("the baseline poll", &|r| r.get(&key).is_some_and(|p| p.monitor.status == MonitorStatus::Ok && p.comments == 1))?;
    fixture.comment(3, "ben", "please handle the 429 retry");
    crate::fake_gh::write_fixture(&fixtures, &pr, &fixture)?;
    wait_registry("three new comments", &|r| r.get(&key).is_some_and(|p| p.new_comments == 3))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(home.join("bus").join("log.jsonl")).unwrap_or_default().contains("\"pr.comment.new\"") {
        anyhow::ensure!(Instant::now() < deadline, "no pr.comment.new event on the bus");
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = canvas_cli(home, &["pr", "dispatch", URL, "--key", "comments:selftest", "--event", "pr.comment.new", "Review and address the 3 new review comments"])?;
    anyhow::ensure!(out.contains("blocked") && out.contains("not running"), "a stopped owner blocks the follow-up: {out}");
    let out = canvas_cli(home, &["pr", "dispatch", URL, "--key", "comments:selftest", "the same again"])?;
    anyhow::ensure!(out.contains("Already dispatched"), "one follow-up per key: {out}");
    // ◎ Agents › Pull requests: the blocked follow-up and what to do about it.
    let (x, y) = ui.find("◎ Agents").context("the top bar's ◎ Agents")?;
    ui.click(x + 1, y)?;
    ui.wait("the PR section", Duration::from_secs(10), |t| t.contains("PULL REQUESTS ·") && t.contains("Tracked pull requests"))?;
    let (x, y) = ui.find("Tracked pull requests").context("its link")?;
    ui.click(x + 1, y)?;
    ui.wait("the pull requests panel", Duration::from_secs(10), |t| t.contains("selftest/app#7") && t.contains("blocked") && t.contains("Resume owner") && t.contains("Dismiss"))?;
    let (x, y) = ui.find("Dismiss").context("Dismiss")?;
    ui.click(x + 1, y)?;
    wait_registry("the follow-up dismissed", &|r| r.get(&key).is_some_and(|p| p.dispatches.iter().all(|d| d.state == DispatchState::Dismissed)))?;
    let thread = canvas_agents::prs::thread_key(&key);
    let reported = std::fs::read_dir(home.join("reports").join("items")).into_iter().flatten().flatten().filter_map(|e| std::fs::read_to_string(e.path().join("meta.json")).ok()).any(|m| m.contains(&format!("\"thread\": \"{thread}\"")) && m.contains("\"kind\": \"warn\""));
    anyhow::ensure!(reported, "the blocked follow-up is reported in its thread");
    // Merged: never polled again.
    fixture.state = "MERGED".into();
    crate::fake_gh::write_fixture(&fixtures, &pr, &fixture)?;
    wait_registry("merged", &|r| r.get(&key).is_some_and(|p| p.state == PrState::Merged && p.monitor.status == MonitorStatus::Stopped))?;
    let polls = crate::fake_gh::calls(&fixtures).len();
    std::thread::sleep(Duration::from_secs(3));
    let calls = crate::fake_gh::calls(&fixtures);
    anyhow::ensure!(calls.len() == polls && !calls.iter().any(|c| c.starts_with("REFUSED")), "polled after it merged, or a write was tried: {calls:?}");
    ui.wait("merged in the panel", Duration::from_secs(10), |t| t.contains("· merged"))?;
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    anyhow::ensure!(!log.contains(&format!("launched {PR_OWNER_ID} ")), "the owner was started");
    ui.send(b"\x1b")?;
    Ok(())
}

/// Review follow-ups (WI-016), with the stand-in agent: its Follow-ups…
/// prompt sets the `outbox` channel (a folder); a review of "PR owner" is
/// handed off (as you, with private notes); a draft quoting the notes is
/// refused, a proper one waits for your approval (Reports says so);
/// *Approve & send* in the panel makes the agent send it into the outbox and
/// record it (the author's conversation is watched); the author's reply
/// asks for a re-review: *Check now* has the agent read it, the request is
/// recorded once, tied to the pull request and routed to the review session
/// (stopped: blocked); a second check adds nothing; *Pause checks*.
fn review_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    use canvas_agents::reviews::{Book, MessageState, RequestState};
    step("review follow-ups: a hand-off; a draft quoting private notes is refused, one waits for approval; Approve & send → the agent sends it through the outbox; a re-review request is read on Check now, recorded once and routed to the review session (stopped: blocked); Pause checks");
    let home = &instance.home;
    const URL: &str = "https://github.com/selftest/lib/pull/9";
    let outbox = home.join("selftest-outbox");
    std::fs::create_dir_all(&outbox)?;
    ui.send(b"\x1b")?;
    let (bx, by) = ui.locate("Selftest agent", CANVAS)?;
    ui.click(bx, by)?;
    ui.wait("the agent's panel", Duration::from_secs(5), |t| t.contains("◎ Selftest agent") && t.contains("Follow-ups…"))?;
    let (x, y) = ui.find("Follow-ups…").context("Follow-ups…")?;
    ui.click(x + 1, y)?;
    ui.wait("the follow-ups prompt", Duration::from_secs(5), |t| t.contains("Review follow-ups:"))?;
    ui.send(format!("\x15outbox {} approval every 60m\r", outbox.display()).as_bytes())?;
    ui.wait("its follow-ups", Duration::from_secs(10), |t| t.contains("Follow-ups") && t.contains("you approve messages"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let canvas = loop {
        let routing = canvas_agents::cli::routing(home);
        if routing.passive.iter().any(|p| p.reviews.as_ref().is_some_and(|r| r.channel == "outbox")) {
            break routing.canvas;
        }
        anyhow::ensure!(Instant::now() < deadline, "review follow-ups never reached the service");
        std::thread::sleep(Duration::from_millis(200));
    };
    let wait_book = |what: &str, secs: u64, ok: &dyn Fn(&Book) -> bool| -> Result<Book> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let book = canvas_agents::reviews_cli::load(home, &canvas);
            if ok(&book) {
                return Ok(book);
            }
            anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}: {}", serde_json::to_string(&book).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let (results, notes) = (home.join("review-results.md"), home.join("review-notes.md"));
    std::fs::write(&results, "Changes requested: the retry loop never stops; please add a test for the 429 path.")?;
    std::fs::write(&notes, "Ben thinks this repeats the March outage")?;
    let out = canvas_cli(home, &["review", "handoff", "--pr", URL, "--author", "Ana Selftest", "--author-id", "ana", "--outcome", "changes-requested", "--results", &results.display().to_string(), "--notes", &notes.display().to_string(), "--session", "PR owner"])?;
    anyhow::ensure!(out.contains("recorded for Ana Selftest") && out.contains("you approve it"), "{out}");
    let book = wait_book("the review", 5, &|b| b.reviews.len() == 1)?;
    let review = book.reviews[0].id.clone();
    // A draft that quotes the private notes is refused; a proper one waits for you.
    let leaking = home.join("draft-leak.md");
    std::fs::write(&leaking, format!("Hi Ana, {URL}: Ben thinks this repeats the March outage."))?;
    let refused = std::process::Command::new(std::env::current_exe()?).args(["review", "message", &review, "--draft", &leaking.display().to_string(), "--home", &home.display().to_string()]).env_remove("COPILOT_CANVAS_SESSION").output()?;
    anyhow::ensure!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("private review notes"), "a draft with private notes was taken: {}", String::from_utf8_lossy(&refused.stdout));
    let draft = home.join("draft.md");
    std::fs::write(&draft, format!("Hi Ana, I reviewed {URL}: changes requested. The retry loop never stops; a test for the 429 path is missing."))?;
    let out = canvas_cli(home, &["review", "message", &review, "--draft", &draft.display().to_string()])?;
    anyhow::ensure!(out.contains("awaiting the user's approval"), "{out}");
    // ◎ Agents › Review follow-ups: Approve & send.
    let (x, y) = ui.find("◎ Agents").context("the top bar's ◎ Agents")?;
    ui.click(x + 1, y)?;
    ui.wait("the review follow-ups section", Duration::from_secs(10), |t| t.contains("REVIEW FOLLOW-UPS ·") && t.contains("Review follow-ups:"))?;
    let (x, y) = ui.find("Review follow-ups:").context("its link")?;
    ui.click(x + 1, y)?;
    ui.wait("the draft awaiting approval", Duration::from_secs(10), |t| t.contains("awaiting your approval") && t.contains("Approve & send"))?;
    let (x, y) = ui.find("Approve & send").context("Approve & send")?;
    ui.click(x + 1, y)?;
    // The agent (the stand-in) sends it through the outbox and records it.
    let book = wait_book("the message sent", 60, &|b| b.reviews[0].message.state == MessageState::Sent)?;
    let conversation = book.reviews[0].message.conversation.clone();
    anyhow::ensure!(conversation == "outbox:ana" && book.conversation(&conversation).is_some_and(|c| c.watching), "{conversation:?}");
    let sent = std::fs::read_to_string(outbox.join("outbox-ana.out.jsonl")).unwrap_or_default();
    anyhow::ensure!(sent.contains(URL) && sent.contains("changes requested") && !sent.contains("March outage"), "the outbox: {sent}");
    // The author asks for a re-review; Check now has the agent read it.
    std::fs::write(outbox.join("outbox-ana.in.jsonl"), "{\"id\":\"m1\",\"text\":\"Thanks! I pushed fixes, could you re-review?\"}\n")?;
    ui.wait("watched, with a next check", Duration::from_secs(10), |t| t.contains("WATCHED CONVERSATIONS") && t.contains("Check now"))?;
    let (x, y) = ui.find("Check now").context("Check now")?;
    ui.click(x + 1, y)?;
    let book = wait_book("the request routed", 60, &|b| b.requests.len() == 1 && b.requests[0].state == RequestState::Blocked && b.checks.last_ms > 0)?;
    let request = &book.requests[0];
    anyhow::ensure!(request.review == review && request.to == PR_OWNER_ID && request.kind == "re-review" && request.reason.contains("not running"), "{request:?}");
    ui.wait("the blocked request with Resume reviewer", Duration::from_secs(10), |t| t.contains("blocked") && t.contains("Resume reviewer"))?;
    // A second check: the same message is never a second request.
    let checked = book.checks.last_ms;
    let (x, y) = ui.find("Check now").context("Check now")?;
    ui.click(x + 1, y)?;
    let book = wait_book("a second check", 60, &|b| b.checks.last_ms > checked)?;
    anyhow::ensure!(book.requests.len() == 1, "one request: {:?}", book.requests);
    // Pause checks: the agent's setting.
    let (x, y) = ui.find("Pause checks").context("Pause checks")?;
    ui.click(x + 1, y)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !canvas_agents::cli::routing(home).passive.iter().any(|p| p.reviews.as_ref().is_some_and(|r| r.paused)) {
        anyhow::ensure!(Instant::now() < deadline, "the checks were not paused");
        std::thread::sleep(Duration::from_millis(200));
    }
    ui.wait("paused in the panel", Duration::from_secs(10), |t| t.contains("checks are paused") && t.contains("Resume checks"))?;
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    anyhow::ensure!(!log.contains(&format!("launched {PR_OWNER_ID} ")), "the review session was started");
    ui.send(b"\x1b")?;
    Ok(())
}

/// Viewing never starts a session; double-click, Enter and the details
/// panel's Start button do (the demo's two sessions nobody started yet).
fn view_and_start_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    let launched = |id: &str| std::fs::read_to_string(instance.log_path()).unwrap_or_default().contains(&format!("launched {id} "));
    step("a click views a stopped session without launching it (details panel with Start)");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    let (rx, ry) = ui.locate("Review implementation", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("its details panel", Duration::from_secs(5), |t| t.contains("▸ Start") && t.contains("Double-click the box"))?;
    let (bx, by) = ui.locate("Review implementation", CANVAS)?;
    ui.click(bx + 1, by + 1)?;
    std::thread::sleep(Duration::from_millis(800));
    anyhow::ensure!(!launched("demo-review"), "a click launched the session");
    anyhow::ensure!(ui.text().contains("▸ Start"), "the details panel stays after a click on the box\n{}", ui.text());

    step("a double-click on the box starts it and its terminal gets the keyboard");
    ui.double_click(bx + 1, by + 1)?;
    ui.wait("its shell", Duration::from_secs(20), |t| t.contains("Review implementation · ") && t.contains("PS ") && t.contains("Typing goes to the terminal"))?;
    anyhow::ensure!(launched("demo-review"), "the double-click launched it");

    step("the details panel's Start button starts a session");
    ui.send(b"\x1d")?;
    let (wx, wy) = ui.locate("Write tests", SIDEBAR)?;
    ui.click(wx, wy)?;
    ui.wait("its details panel", Duration::from_secs(5), |t| t.contains("Write tests") && t.contains("▸ Start"))?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(!launched("demo-sub"), "a sidebar click launched the session");
    let (sx, sy) = ui.locate("▸ Start", (CANVAS.1, 170))?;
    ui.click(sx + 1, sy)?;
    ui.wait("its shell", Duration::from_secs(20), |t| t.contains("Write tests · ") && t.contains("PS "))?;
    anyhow::ensure!(launched("demo-sub"), "Start launched it");
    Ok(())
}

/// The sessions in the sidebar's RECENT section, newest first (the start of
/// each name: long ones are shortened there).
pub(crate) fn recent_names(text: &str) -> Vec<String> {
    let rows: Vec<String> = text.lines().map(|l| l.chars().skip(SIDEBAR.0 as usize).take((SIDEBAR.1 - SIDEBAR.0) as usize).collect()).collect();
    let Some(start) = rows.iter().position(|r| r.contains("RECENT")) else { return Vec::new() };
    rows[start + 1..]
        .iter()
        .take_while(|r| !r.contains("All sessions"))
        .filter_map(|r| {
            // "■ Review implement… shell 2m": past the status icon, the first word or two.
            let row = r.trim().trim_start_matches('▎').trim();
            let name: String = row.chars().skip(2).take(10).collect();
            (!name.trim().is_empty()).then(|| name.trim().to_owned())
        })
        .collect()
}

/// The RECENT section and its keys: Ctrl+F6 from a terminal views the
/// session used before (its terminal or its details panel), Ctrl+Shift+F6
/// goes back, nothing starts, and once the walk settles the list puts the
/// session it reached first.
fn recent_steps<U: Ui>(ui: &mut U, instance: &Instance) -> Result<()> {
    let launches = || std::fs::read_to_string(instance.log_path()).unwrap_or_default().matches("launched ").count();
    step("recent sessions: Ctrl+F6 / Ctrl+Shift+F6 view recent sessions without starting them; the RECENT list reorders when the walk ends");
    ui.wait("the RECENT section with its keys", Duration::from_secs(5), |t| t.contains("RECENT") && t.contains("Ctrl+F6 · [ ]") && recent_names(t).len() >= 3)?;
    let names = recent_names(&ui.text());
    ui.wait("a terminal with the keyboard", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
    let before = launches();
    // From the terminal: the session used before it, then the one before that.
    ui.send(b"\x1b[17;5~")?;
    ui.wait("the second recent session", Duration::from_secs(5), |t| t.contains("Recent 2 of") && t.contains(names[1].as_str()))?;
    ui.send(b"\x1b[17;5~")?;
    ui.wait("the third recent session", Duration::from_secs(5), |t| t.contains("Recent 3 of"))?;
    ui.send(b"\x1b[17;6~")?;
    ui.wait("back to the second", Duration::from_secs(5), |t| t.contains("Recent 2 of"))?;
    anyhow::ensure!(recent_names(&ui.text()).first() == names.first(), "the list keeps its order during the walk: {:?}", recent_names(&ui.text()));
    // A second later the session the walk reached is the most recent.
    ui.wait("the walk settled", Duration::from_secs(5), |t| recent_names(t).first() == names.get(1))?;
    std::thread::sleep(Duration::from_millis(300));
    anyhow::ensure!(launches() == before, "walking the recent sessions started one");
    Ok(())
}

/// Reports: a session's terminal publishes a Markdown report and a message
/// with the `copilot-canvas` command it has on its PATH; the activity bar
/// counts two unread; a click opens one formatted and marks it read at the
/// service; nothing is launched.
fn report_steps<U: Ui>(ui: &mut U, instance: &Instance, work: &Path) -> Result<()> {
    let launches = || std::fs::read_to_string(instance.log_path()).unwrap_or_default().matches("launched ").count();
    step("reports: a session publishes a Markdown report and a message (copilot-canvas report publish); ✉2 unread; a click opens one formatted and marks it read; nothing launched");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("its terminal with the keyboard", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
    let brief = work.join("selftest-brief.md");
    std::fs::write(&brief, "# Selftest brief\n\n## Needs you\n\n- **one** pull request\n- build `main` green\n\n| Area | State |\n|---|---|\n| Tests | pass |\n")?;
    let before = launches();
    let command = format!("copilot-canvas report publish --markdown '{}'; copilot-canvas report publish --message 'Selftest message: build green' --kind ok\r", brief.display());
    ui.send(command.as_bytes())?;
    ui.wait("two unread reports on the activity bar", Duration::from_secs(20), |t| t.contains("✉2"))?;
    ui.send(b"\x1d")?;
    ui.send(b"\x1b9")?;
    ui.wait("the inbox", Duration::from_secs(5), |t| t.contains("REPORTS") && t.contains("2 unread") && t.contains("Selftest brief") && t.contains("Selftest message"))?;
    let (bx, by) = ui.locate("Selftest brief", (SIDEBAR.0, SIDEBAR.1 + 20))?;
    ui.click(bx, by)?;
    ui.wait("the brief, formatted", Duration::from_secs(10), |t| t.contains("Report · Selftest brief") && t.contains("Needs you") && t.contains("• one pull request") && t.contains("│ State"))?;
    ui.wait("one unread left", Duration::from_secs(5), |t| t.contains("✉1"))?;
    let read = || {
        std::fs::read_to_string(instance.home.join("reports").join("state.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| v["read"].as_object().map(|o| o.len())).unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while read() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(read() == 1, "the service keeps the brief read (and only it): {} read", read());
    ui.send(b"\x1b")?;
    ui.wait("the viewer closed", Duration::from_secs(5), |t| !t.contains("Report · Selftest brief"))?;
    ui.send(b"\x1b1")?;
    ui.wait("the sessions view", Duration::from_secs(5), |t| t.contains("Canvas project"))?;
    anyhow::ensure!(launches() == before, "reading a report launched something");
    // Back in a terminal, as the steps before left it.
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
    Ok(())
}

/// A click on a group's border selects the group (its panel, a heavy outline).
fn group_click_steps<U: Ui>(ui: &mut U) -> Result<()> {
    step("a click on a group's border selects the group");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    // The sidebar's group row flies to it (and tiles its running terminals);
    // back to the single view, where a selected group shows its panel.
    let (gx, gy) = ui.locate("Canvas project", SIDEBAR)?;
    ui.click(gx + 1, gy)?;
    ui.send(b"\x1b")?;
    if ui.text().contains("▦ Tiled") {
        ui.send(b"t")?;
    }
    ui.wait("no group selected", Duration::from_secs(5), |t| !t.contains("Del: delete group") && t.contains("▢ Single"))?;
    // The title is drawn two cells right of the group's left edge (" ▣ name").
    let (tx, ty) = ui.locate("▣ Canvas project", CANVAS)?;
    let (bx, by) = (tx.saturating_sub(3), ty + 2);
    ui.click(bx, by)?;
    ui.wait("the group's panel", Duration::from_secs(5), |t| t.contains("Del: delete group") && t.contains("Canvas project"))?;
    let at_column = |line: &str, column: usize| {
        let mut x = 0;
        line.chars().find(|c| {
            let here = x == column;
            x += crate::client::paint::text_width(&c.to_string());
            here
        })
    };
    let corner = ui.text().lines().nth(ty as usize).and_then(|l| at_column(l, bx as usize));
    anyhow::ensure!(corner == Some('┏'), "a selected group has a heavy outline, found {corner:?} at ({bx}, {ty})\n{}", ui.text());
    ui.send(b"\x1b")?;
    // Back in a terminal, as the steps before left it.
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
    Ok(())
}

/// Every box's place and every group's frame, by id.
type Geometry = (Vec<(String, i32, i32)>, Vec<(String, i32, i32, i32, i32)>);

fn geometry(doc: &Doc) -> Geometry {
    let mut nodes: Vec<(String, i32, i32)> = doc.nodes.iter().map(|n| (n.id.clone(), n.x, n.y)).collect();
    let mut groups: Vec<(String, i32, i32, i32, i32)> = doc.groups.iter().map(|g| (g.id.clone(), g.x, g.y, g.w, g.h)).collect();
    nodes.sort();
    groups.sort();
    (nodes, groups)
}

/// Steps the canvas zoom with `+` / `-` until the status bar shows `pct` %.
fn zoom_to<U: Ui>(ui: &mut U, pct: u32) -> Result<()> {
    for _ in 0..8 {
        let text = ui.text();
        let now = text.rsplit("zoom ").next().and_then(|t| t.split('%').next()).and_then(|n| n.trim().parse::<u32>().ok());
        match now {
            Some(n) if n == pct => return Ok(()),
            Some(n) => ui.send(if n < pct { b"+" } else { b"-" })?,
            None => bail!("no zoom in the status bar\n{text}"),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!("the canvas never reached {pct} %\n{}", ui.text())
}

/// From a session's terminal to the canvas, centred on that session (its
/// box selected) at `pct` %. (Keys arrive in order: Ctrl+] reaches the
/// terminal before `c` reaches the canvas; a notice may hide the hints.)
fn canvas_at<U: Ui>(ui: &mut U, pct: u32) -> Result<()> {
    ui.send(b"\x1d")?;
    std::thread::sleep(Duration::from_millis(200));
    ui.send(b"c")?;
    zoom_to(ui, pct)
}

/// A sidebar click on the demo's renamed session: its terminal gets the
/// keyboard again (the status hint shows once a notice has gone).
fn back_to_terminal<U: Ui>(ui: &mut U) -> Result<()> {
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("terminal focus", Duration::from_secs(12), |t| t.contains("Typing goes to the terminal"))
}

/// Ctrl+Z on the canvas takes back a group drag exactly: the group and
/// everything that moved with it (its sessions, the subagent box) go back to
/// the very same places in the canvas file. In a terminal Ctrl+Z belongs to
/// the program: the paste probe receives it and the canvas keeps its move.
fn layout_undo_steps<U: Ui>(ui: &mut U, canvas: &Path) -> Result<()> {
    step("Ctrl+Z undoes a group drag exactly (its boxes and subagent box too); in a terminal Ctrl+Z reaches the program");
    canvas_at(ui, 100)?;
    let before = load(canvas)?;
    let group = before.groups.iter().find(|g| g.name == "Canvas project").context("the demo group")?.clone();
    let inside: Vec<String> = before
        .nodes
        .iter()
        .filter(|n| {
            let (cx, cy) = n.rect().center();
            group.rect().contains(cx, cy)
        })
        .map(|n| n.id.clone())
        .collect();
    anyhow::ensure!(inside.iter().any(|id| id == "demo-main:explore"), "the group holds a subagent box: {inside:?}");
    let (tx, ty) = ui.locate("▣ Canvas project", CANVAS)?;
    ui.drag(0, (tx + 2, ty), (tx + 6, ty + 2))?;
    let moved = wait_doc(canvas, "the group and everything in it moved by (4, 2)", |d| {
        d.group(&group.id).is_some_and(|g| (g.x - group.x, g.y - group.y) == (4, 2))
            && inside.iter().all(|id| matches!((d.node(id), before.node(id)), (Some(a), Some(b)) if (a.x - b.x, a.y - b.y) == (4, 2)))
    })?;
    ui.wait("the undo hint", Duration::from_secs(5), |t| t.contains("Ctrl+Z undo move"))?;
    // A terminal with the keyboard: Ctrl+Z is the program's (the probe prints it).
    back_to_terminal(ui)?;
    ui.send(b"Clear-Host\r")?;
    ui.wait("a cleared terminal", Duration::from_secs(10), |t| !t.contains(crate::paste_probe::READY))?;
    let exe = std::env::current_exe()?;
    ui.send(format!("& '{}' paste-probe\r", exe.display()).as_bytes())?;
    ui.wait("the paste probe", Duration::from_secs(20), |t| t.contains(crate::paste_probe::READY))?;
    ui.send(b"\x1a")?;
    ui.wait("Ctrl+Z reached the program", Duration::from_secs(10), |t| t.contains("KEY \\x1a"))?;
    std::thread::sleep(Duration::from_millis(400));
    anyhow::ensure!(geometry(&load(canvas)?) == geometry(&moved), "Ctrl+Z in a terminal changed the canvas");
    ui.send(b"\x04")?;
    ui.wait("the probe ended (Ctrl+D)", Duration::from_secs(10), |t| t.contains("BYE"))?;
    // On the canvas, Ctrl+Z puts everything back.
    ui.send(b"\x1d")?;
    std::thread::sleep(Duration::from_millis(200));
    ui.send(b"\x1a")?;
    wait_doc(canvas, "every box and group exactly where it was", |d| geometry(d) == geometry(&before))?;
    ui.wait("the undo notice", Duration::from_secs(5), |t| t.contains("Undid: moved group"))?;
    back_to_terminal(ui)
}

/// A right-drag (the TUI's area selection; the GUI's Shift-drag is the
/// same gesture) that starts on a session and reaches its subagent box
/// selects both, at 100 % and at 50 %; dragging the session then moves the
/// subagent box once (not twice), and Ctrl+Z puts both back.
fn area_selection_steps<U: Ui>(ui: &mut U, canvas: &Path) -> Result<()> {
    step("an area selection over a session and its subagent box selects both at 100 % and 50 %; moving them moves the subagent once");
    for pct in [100, 50] {
        canvas_at(ui, pct)?;
        let before = load(canvas)?;
        let at = |d: &Doc, id: &str| d.node(id).map(|n| (n.x, n.y)).unwrap_or_default();
        let (main, agent) = (at(&before, "demo-main"), at(&before, "demo-main:explore"));
        // The name sits two cells into the box; the band starts on the box's
        // corner and ends inside the subagent box (found from the document:
        // at 50 % the active terminal's halo may cover its label).
        let (mx, my) = ui.locate("Renamed by", CANVAS)?;
        let cells = |world: i32| if pct == 100 { world } else { (world + 1) / 2 };
        let end = ((mx - 2) as i32 + cells(agent.0 - main.0) + 3, my as i32 + cells(agent.1 - main.1));
        ui.drag(2, (mx - 2, my), (end.0 as u16, end.1 as u16))?;
        ui.wait(&format!("both selected at {pct} %"), Duration::from_secs(5), |t| t.contains("2 boxes selected (1 session, 1 subagent)"))?;
        anyhow::ensure!(geometry(&load(canvas)?) == geometry(&before), "the area selection moved a box at {pct} %");
        ui.drag(0, (mx, my), (mx + 3, my + 1))?;
        let d = if pct == 100 { (3, 1) } else { (6, 2) };
        let moved = |doc: &Doc, id: &str, from: (i32, i32)| at(doc, id) == (from.0 + d.0, from.1 + d.1);
        wait_doc(canvas, &format!("the session and its subagent box moved once by {d:?} at {pct} %"), |doc| moved(doc, "demo-main", main) && moved(doc, "demo-main:explore", agent))?;
        ui.send(b"\x1a")?;
        wait_doc(canvas, "Ctrl+Z put both back", |doc| at(doc, "demo-main") == main && at(doc, "demo-main:explore") == agent)?;
        back_to_terminal(ui)?;
    }
    Ok(())
}

#[cfg(test)]
mod perf_probe {
    use std::time::Duration;
    /// Manual: `PERF_CANVAS=<canvas.json copy> cargo test --release perf_probe -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perf_probe() {
        let root = std::env::temp_dir().join(format!("cc-perf-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work) = (root.join("home"), root.join("sessions"), root.join("work"));
        for d in [&sessions, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let instance = crate::paths::Instance::new(Some(format!("perf{}", std::process::id())), Some(home), Some(sessions)).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let canvas = root.join("canvas.json");
        std::fs::copy(std::env::var("PERF_CANVAS").unwrap(), &canvas).unwrap();
        let args: Vec<String> = vec![
            "--perf".into(), "--instance".into(), instance.name.clone(), "--home".into(), instance.home.to_string_lossy().into_owned(),
            "--sessions-dir".into(), instance.sessions_dir.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned(),
            "--canvas".into(), canvas.to_string_lossy().into_owned(),
        ];
        let (cols, rows) = (200, 55);
        let mut ui = super::Console::start(&exe, &args, cols, rows).unwrap();
        let hud = |ui: &super::Console| ui.text().lines().last().unwrap_or_default().rsplit("  ").find(|s| s.contains("fps")).unwrap_or("").trim().to_owned();
        ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Copilot Canvas") && t.contains("fps")).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        for zoom in ["100%", "50%", "25%"] {
            // Continuous middle-button pan, 125 mouse reports per second, as a fast hand would.
            let (x0, y0) = (90, 25);
            ui.send_now(format!("\x1b[<1;{x0};{y0}M").as_bytes()).unwrap();
            let started = std::time::Instant::now();
            let mut i = 0i32;
            while started.elapsed() < Duration::from_secs(3) {
                i += 1;
                let dx = ((i / 4) % 40) - 20;
                ui.send_now(format!("\x1b[<33;{};{}M", x0 + dx, y0 + (i / 9) % 6).as_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(8));
            }
            ui.send_now(format!("\x1b[<1;{x0};{y0}m").as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            println!("PAN at zoom {zoom}: {}", hud(&ui));
            ui.send_now(b"-").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        }
        ui.send(b"q").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod stress_probe {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use crate::client::doc::{Doc, Group, Node, NodeKind};

    /// Manual: `cargo test --release stress_probe -- --ignored --nocapture`.
    /// 100 live sessions (30 of them busy: streaming events, spawning subagents,
    /// asking permission) while the canvas pans; prints the --perf numbers.
    #[test]
    #[ignore]
    fn stress_probe() {
        let root = std::env::temp_dir().join(format!("cc-stress-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work) = (root.join("home"), root.join("sessions"), root.join("work"));
        for d in [&home, &sessions, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let pid = std::process::id();
        let mut doc = Doc::default();
        let n_sessions: usize = std::env::var("STRESS_SESSIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
        let n_agents: usize = std::env::var("STRESS_AGENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let ids: Vec<String> = (0..n_sessions).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        for (i, id) in ids.iter().enumerate() {
            let dir = sessions.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("workspace.yaml"),
                format!("id: {id}\nname: \"Stress {i}\"\ncwd: C:\\\\stress\nclient_name: github/cli\ncreated_at: 2026-10-21T10:00:00Z\nupdated_at: 2026-10-21T10:00:00Z\n"),
            )
            .unwrap();
            let mut events = String::from("{\"type\":\"session.start\",\"data\":{}}\n{\"type\":\"user.message\",\"data\":{}}\n");
            for a in 0..n_agents {
                events.push_str(&format!("{{\"type\":\"subagent.started\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\",\"agentName\":\"Agent {a}\"}}}}\n"));
                if a % 2 == 0 {
                    events.push_str(&format!("{{\"type\":\"subagent.completed\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\"}}}}\n"));
                }
            }
            std::fs::write(dir.join("events.jsonl"), events).unwrap();
            std::fs::write(dir.join(format!("inuse.{pid}.lock")), "").unwrap();
            let (g, k) = (i / 10, i % 10);
            doc.nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Session,
                name: format!("Stress {i}"),
                x: g as i32 * 80 + 2 + (k as i32 % 2) * 36,
                y: (k as i32 / 2) * 10 + 2,
                cwd: Some(work.to_string_lossy().into_owned()),
                parent: None,
                agent_key: None,
                source: None,
                command: None,
                fresh: false,
                automation: None,
                synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
            });
        }
        for g in 0..n_sessions.div_ceil(10) as i32 {
            doc.groups.push(Group { id: format!("g{g}"), name: format!("Group {g}"), x: g * 80, y: 0, w: 76, h: 54, color: (g % 6) as u8, folder: None, source: None, defaults: None });
        }
        let canvas = home.join("canvas.json");
        std::fs::write(&canvas, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        // 30 busy sessions append events continuously.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (stop, sessions, busy) = (stop.clone(), sessions.clone(), ids[..30].to_vec());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    n += 1;
                    for (i, id) in busy.iter().enumerate() {
                        let mut f = std::fs::OpenOptions::new().append(true).open(sessions.join(id).join("events.jsonl")).unwrap();
                        let mut lines = vec![
                            format!("{{\"type\":\"tool.execution_start\",\"data\":{{\"toolCallId\":\"t{n}\",\"toolName\":\"shell\"}}}}"),
                            format!("{{\"type\":\"tool.execution_complete\",\"data\":{{\"toolCallId\":\"t{n}\"}}}}"),
                        ];
                        if n.is_multiple_of(10) && n / 10 <= 12 {
                            lines.push(format!("{{\"type\":\"subagent.started\",\"agentId\":\"a{n}\",\"data\":{{\"toolCallId\":\"s{n}\",\"agentName\":\"Worker {n}\"}}}}"));
                        }
                        if n.is_multiple_of(15) {
                            lines.push(format!("{{\"type\":\"subagent.completed\",\"agentId\":\"a{}\",\"data\":{{\"toolCallId\":\"s{}\"}}}}", n - 5, n - 5));
                        }
                        if (n + i as u64).is_multiple_of(40) {
                            lines.push(format!("{{\"type\":\"permission.requested\",\"data\":{{\"requestId\":\"p{n}\"}}}}"));
                        }
                        if (n + i as u64) % 40 == 20 {
                            lines.push(format!("{{\"type\":\"permission.completed\",\"data\":{{\"requestId\":\"p{}\"}}}}", n - 20));
                        }
                        for line in lines {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };

        let instance = crate::paths::Instance::new(Some(format!("stress{pid}")), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec![
            "--perf".into(), "--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(),
            "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned(),
        ];
        let mut ui = super::Console::start(&exe, &args, 320, 55).unwrap();
        let hud = |ui: &super::Console| ui.text().lines().last().unwrap_or_default().rsplit("  ").find(|s| s.contains("fps")).unwrap_or("").trim().to_owned();
        ui.wait("canvas", Duration::from_secs(30), |t| t.contains("Stress 1") && t.contains("fps")).unwrap();
        std::thread::sleep(Duration::from_secs(6));
        println!("IDLE (sessions busy, no input): {}", hud(&ui));
        let (x0, y0) = (100, 25);
        ui.send_now(format!("\x1b[<1;{x0};{y0}M").as_bytes()).unwrap();
        let started = Instant::now();
        let mut i = 0i32;
        while started.elapsed() < Duration::from_secs(5) {
            i += 1;
            ui.send_now(format!("\x1b[<33;{};{}M", x0 + ((i / 3) % 40) - 20, y0 + (i / 9) % 6).as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(8));
        }
        ui.send_now(format!("\x1b[<1;{x0};{y0}m").as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        println!("PANNING (sessions busy): {}", hud(&ui));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = writer.join();
        ui.send(b"q").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod user_message_canvas_probe {
    use std::time::Duration;

    /// Opens a copy of an old session from the canvas (sandboxed COPILOT_HOME)
    /// and checks that your messages are tinted. `PROBE_SESSION=<id>`.
    #[test]
    #[ignore]
    fn user_message_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-umc-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        std::fs::create_dir_all(sessions.join(&id)).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("umc{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 60).unwrap();
        ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Probe")).unwrap();
        ui.send(b"\r").unwrap();
        let ok = ui.wait("resumed history", Duration::from_secs(90), |t| t.contains("❯ what is") || t.contains("❯ catch up"));
        std::thread::sleep(Duration::from_secs(4));
        let snap = canvas_host::emulator::Emulator::snapshot(&*crate::service::term::lock(&ui.screen));
        let user_bg = crate::protocol::COLOR_RGB | (30 << 16) | (44 << 8) | 70;
        let mut tinted = 0;
        for runs in &snap.lines {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            if runs.iter().any(|r| r.bg == user_bg) {
                tinted += 1;
                println!("TINTED: {}", text.trim().chars().take(100).collect::<String>());
            }
        }
        println!("resumed: {}, tinted rows: {tinted}", ok.is_ok());
        ui.send(b"\x1d").unwrap();
        ui.send(b"q").unwrap();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod upgrade_probe {
    use std::time::Duration;

    use super::Console;


    /// The old build must speak the same protocol (otherwise the restart is required at startup).
    /// Manual (after `cargo build --release`): `UPGRADE_OLD_EXE=<older copilot-canvas.exe> cargo test --release upgrade_probe -- --ignored --nocapture`.
    /// An older build's service keeps a shell running; the new client keeps it at
    /// the startup prompt, then updates it from the toolbar and the shell resumes.
    #[test]
    #[ignore]
    fn upgrade_probe() {
        let old = std::path::PathBuf::from(std::env::var("UPGRADE_OLD_EXE").expect("UPGRADE_OLD_EXE"));
        let new = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), r"\target\release\copilot-canvas.exe"));
        let root = std::env::temp_dir().join(format!("cc-upgrade-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work, copilot_home) = (root.join("home"), root.join("sessions"), root.join("work"), root.join("copilot-home"));
        for d in [&sessions, &work, &copilot_home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let instance = crate::paths::Instance::new(Some(format!("upgrade{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let args: Vec<String> = vec![
            "--demo".into(),
            "--instance".into(),
            instance.name.clone(),
            "--home".into(),
            home.to_string_lossy().into_owned(),
            "--sessions-dir".into(),
            sessions.to_string_lossy().into_owned(),
            "--cwd".into(),
            work.to_string_lossy().into_owned(),
        ];
        let env = [("COPILOT_HOME", copilot_home.as_path())];
        let result = (|| -> anyhow::Result<()> {
            println!("old build: a shell keeps running after the client detaches");
            let mut ui = Console::start_with_env(&old, &args, &env, 170, 46)?;
            ui.wait("old canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
            ui.send(b"\r")?;
            ui.wait("shell", Duration::from_secs(20), |t| t.contains("PS "))?;
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
            ui.send(b"\r")?;
            ui.wait_exit(Duration::from_secs(10))?;

            println!("new build: keep the old service at the startup prompt");
            let mut ui = Console::start_with_env(&new, &args, &env, 170, 46)?;
            ui.wait("startup prompt", Duration::from_secs(20), |t| t.contains("Restart it now? [y/N]"))?;
            if std::env::var("UPGRADE_ANSWER").as_deref() == Ok("y") {
                // Restart at startup (required for an older protocol): the shell resumes.
                ui.send(b"y\r")?;
                ui.wait("resumed after the startup restart", Duration::from_secs(40), |t| {
                    t.contains("Terminal service updated; resumed 1 session(s)") && t.contains("Build session canvas")
                })?;
                anyhow::ensure!(!ui.text().contains("⟳ Update service"), "service is current");
                let (bx, by) = ui.locate("Build session canvas", super::CANVAS)?;
                ui.click(bx, by)?;
                ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
                ui.wait("resumed shell", Duration::from_secs(20), |t| t.contains("PS "))?;
                ui.send(b"Write-Output ('UPGRADE_' + 'OK')\r")?;
                ui.wait("shell works", Duration::from_secs(10), |t| t.contains("UPGRADE_OK"))?;
                println!("startup restart: old service replaced, shell resumed");
                ui.send(b"\x1d")?;
                ui.send(b"q")?;
                ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
                ui.send(b"\x1b[C\r")?;
                ui.wait_exit(Duration::from_secs(10))?;
                return Ok(());
            }
            ui.send(b"n\r")?;
            ui.wait("update button", Duration::from_secs(20), |t| t.contains("⟳ Update service") && t.contains("Build session canvas"))?;

            println!("toolbar update restarts the service and resumes the shell");
            let (x, y) = ui.find("⟳ Update service").unwrap();
            ui.click(x + 2, y)?;
            ui.wait("confirm", Duration::from_secs(5), |t| t.contains("Update the terminal service?") && t.contains("1 running session(s)"))?;
            ui.send(b"\r")?;
            ui.wait("resumed", Duration::from_secs(40), |t| t.contains("Terminal service updated; resumed 1 session(s)"))?;
            anyhow::ensure!(!ui.text().contains("⟳ Update service"), "button gone after the update");
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            let builds: Vec<&str> = log.lines().filter_map(|l| l.split("service started, build ").nth(1)).collect();
            println!("service builds: {builds:?}");
            // The button is gone, so the new service matches the client build.
            anyhow::ensure!(builds.len() == 2 && builds[0] != builds[1], "the service was replaced by another build");
            let (bx, by) = ui.locate("Build session canvas", super::CANVAS)?;
            ui.click(bx, by)?;
            ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
            ui.wait("resumed shell", Duration::from_secs(20), |t| t.contains("PS "))?;
            ui.send(b"Write-Output ('UPGRADE_' + 'OK')\r")?;
            ui.wait("shell works", Duration::from_secs(10), |t| t.contains("UPGRADE_OK"))?;
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
            ui.send(b"\x1b[C\r")?;
            ui.wait_exit(Duration::from_secs(10))?;
            Ok(())
        })();
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_millis(2500));
        }
        result.unwrap();
        println!("PASS");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod image_paste_probe {
    use std::time::Duration;

    /// Manual: put an image on the clipboard, then
    /// `cargo test --release image_paste_probe -- --ignored --nocapture`.
    /// Runs real Copilot (sandboxed COPILOT_HOME, no prompt sent) and shows how
    /// it reacts to each paste keystroke.
    #[test]
    #[ignore]
    fn image_paste_probe() {
        let root = std::env::temp_dir().join(format!("cc-img-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let keys: Vec<(&str, Vec<u8>)> = std::env::var("PASTE_KEYS").ok().map(|k| vec![("custom", k.replace("ESC", "\x1b").into_bytes())]).unwrap_or_else(|| vec![
            ("legacy Ctrl+V", b"\x16".to_vec()),
            ("kitty Ctrl+V", b"\x1b[118;5u".to_vec()),
            ("Alt+V", b"\x1bv".to_vec()),
        ]);
        let kitty = std::env::var_os("NO_KITTY").is_none();
        let args: Vec<String> = vec!["--allow-all".into(), "--disallow-temp-dir".into()];
        let mut env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path())];
        let off = std::path::PathBuf::from("false");
        env.push(("COPILOT_AUTO_UPDATE", off.as_path()));
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, kitty).unwrap();
        let ready = ui.wait("copilot input", Duration::from_secs(60), |t| t.contains("Type @ to mention") || t.contains("? for shortcuts") || t.contains("trust") || t.contains("❯"));
        std::thread::sleep(Duration::from_secs(3));
        if ui.text().to_lowercase().contains("trust") {
            println!("--- trust screen ---\n{}", ui.text());
            ui.send(b"\r").unwrap();
            std::thread::sleep(Duration::from_secs(4));
        }
        println!("ready: {} kitty: {kitty} mode: {}", ready.is_ok(), ui.mode());
        for (name, bytes) in keys {
            ui.send(&bytes).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            let text = ui.text();
            let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            println!("=== after {name} ===\n{}", tail[tail.len().saturating_sub(8)..].join("\n"));
        }
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod passthrough_probe {
    use std::time::Duration;

    /// Manual: `CC_RAW_CAPTURE=<file> cargo test --release passthrough_probe -- --ignored --nocapture`.
    /// Does ConPTY pass OSC 9;4 / OSC 777 / BEL through to the terminal?
    #[test]
    #[ignore]
    fn passthrough_probe() {
        let script = "$e=[char]27; $b=[char]7; Write-Host -NoNewline \"$e]9;4;3;0$b\"; Write-Host -NoNewline \"$e]777;notify;Title;Body$b\"; Write-Host -NoNewline \"$b\"; Write-Host -NoNewline \"$e]0;MyTitle$b\"; Write-Host 'DONE_MARK'; Start-Sleep 2";
        let args: Vec<String> = vec!["-NoProfile".into(), "-Command".into(), script.into()];
        let mut ui = super::Console::start(std::path::Path::new("powershell.exe"), &args, 100, 20).unwrap();
        ui.wait("done", Duration::from_secs(20), |t| t.contains("DONE_MARK")).unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
    }
}

#[cfg(test)]
mod canvas_image_paste_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: image on the clipboard, then
    /// `cargo test --release canvas_image_paste_probe -- --ignored --nocapture`.
    /// A new Copilot session in a sandboxed COPILOT_HOME (no prompt is sent).
    #[test]
    #[ignore]
    fn canvas_image_paste_probe() {
        let root = std::env::temp_dir().join(format!("cc-cip-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        if std::env::var_os("PROBE_MOUSE").is_some() {
            let path = copilot_home.join("settings.json");
            let mut settings: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::json!({}));
            settings["mouse"] = serde_json::json!(true);
            std::fs::write(&path, settings.to_string()).unwrap();
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Paste probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: true, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("cip{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let count = |t: &str| t.matches("📷").count();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Paste probe"))?;
            ui.send(b"\r")?;
            ui.wait("copilot input", Duration::from_secs(90), |t| t.contains("/ commands"))?;
            std::thread::sleep(Duration::from_secs(3));
            // The test image goes on the clipboard only now (CLIP_SET script saves yours first).
            if let Some(script) = std::env::var_os("CLIP_SET") {
                let _ = std::process::Command::new("powershell").args(["-STA", "-NoProfile", "-File"]).arg(script).status();
            }
            ui.send(b"\x1bv")?;
            ui.wait("Alt+V attaches the image", Duration::from_secs(10), |t| count(t) >= 1)?;
            println!("Alt+V: attached");
            ui.send(b"\x16")?;
            ui.wait("Ctrl+V attaches the image", Duration::from_secs(10), |t| count(t) >= 2)?;
            println!("Ctrl+V: attached");
            let (x, y) = ui.find("❯ [").context("input line")?;
            ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes())?;
            ui.wait("pane menu", Duration::from_secs(5), |t| t.contains("Paste"))?;
            // The menu row (not the session named "Paste probe" nor the status hint).
            let text = ui.text();
            let (py, line) = text.lines().enumerate().find(|(_, l)| l.contains(" Paste ") && l.contains("Ctrl+V") && !l.contains("Ctrl+V paste")).context("Paste item")?;
            let px = crate::client::paint::text_width(&line[..line.find(" Paste ").unwrap_or(0)]) as u16 + 1;
            let py = py as u16;
            ui.click(px, py)?;
            ui.wait("menu Paste attaches the image", Duration::from_secs(10), |t| count(t) >= 3)?;
            println!("menu Paste: attached");
            if std::env::var_os("PROBE_MOUSE").is_some() {
                let (tx, ty) = ui.find(" Sessions ").context("Copilot tab bar")?;
                ui.click(tx + 2, ty)?;
                ui.wait("click switched Copilot's tab", Duration::from_secs(10), |t| t.contains("[Sessions]"))?;
                println!("click: reached Copilot (tab switched)");
            }
            Ok(())
        })();
        if let Some(script) = std::env::var_os("CLIP_RESTORE") {
            let _ = std::process::Command::new("powershell").args(["-STA", "-NoProfile", "-File"]).arg(script).status();
        }
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(14)..].join("\n"));
        }
        ui.send(b"\x15").ok();
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod fork_probe {
    use std::time::Duration;

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)); } else { out.push(p); }
        }
        out
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if src.is_dir() { copy_tree(&src, &dst); } else if !e.file_name().to_string_lossy().starts_with("inuse.") { std::fs::copy(&src, &dst).unwrap(); }
        }
    }

    /// Manual: `PROBE_SESSION=<id> cargo test --release fork_probe -- --ignored --nocapture`.
    /// Runs `/fork` on a sandboxed copy of a session and prints what Copilot wrote.
    #[test]
    #[ignore]
    fn fork_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-fork-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        std::fs::create_dir_all(sessions.join(&id)).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        copy_tree(&real.join("session-state").join(&id), &sessions.join(&id));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let args: Vec<String> = vec!["--resume".into(), id.clone(), "--disable-builtin-mcps".into()];
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, true).unwrap();
        let _ = ui.wait("ready", Duration::from_secs(90), |t| t.contains("/ commands") || t.contains("❯"));
        std::thread::sleep(Duration::from_secs(4));
        ui.send(b"/fork Probe fork").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\r").unwrap();
        std::thread::sleep(Duration::from_secs(8));
        let text = ui.text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        println!("--- screen tail ---\n{}", lines[lines.len().saturating_sub(10)..].join("\n"));
        for entry in std::fs::read_dir(&sessions).unwrap().flatten() {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            println!("=== {name}{}", if name == id { " (original)" } else { " (NEW)" });
            let mut files: Vec<String> = walk(&dir).into_iter().map(|p| format!("{} ({} B)", p.strip_prefix(&dir).unwrap().display(), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0))).collect();
            files.sort();
            println!("files: {files:?}");
            println!("{}", std::fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default());
            let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
            let kinds: Vec<String> = events.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).map(|v| v["type"].as_str().unwrap_or("?").to_owned()).collect();
            println!("events: {} lines; first: {:?}; last: {:?}", kinds.len(), kinds.iter().take(4).collect::<Vec<_>>(), kinds.iter().rev().take(4).collect::<Vec<_>>());
            if name != id {
                for line in events.lines().take(2) {
                    println!("  {}", &line[..line.len().min(900)]);
                }
            }
        }
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod fork_canvas_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: `PROBE_SESSION=<id> PROBE_TEXT=<text from its first message> cargo test --release fork_canvas_probe -- --ignored --nocapture`.
    /// New subsession of a sandboxed copy of a session: the child must open with the parent's conversation.
    #[test]
    #[ignore]
    fn fork_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let needle = std::env::var("PROBE_TEXT").unwrap();
        let root = std::env::temp_dir().join(format!("cc-fkc-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions.join(&id), &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let parent_events = std::fs::read(sessions.join(&id).join("events.jsonl")).unwrap();
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Fork parent".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fkc{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Fork parent"))?;
            ui.send(b"s")?;
            ui.wait("subsession dialog with fork default", Duration::from_secs(10), |t| t.contains("New subsession") && t.contains("◉ Fork parent's conversation"))?;
            ui.send(b"Fork probe child\r")?;
            ui.wait("child opens with the parent's conversation", Duration::from_secs(120), |t| t.contains("Fork probe child · ") && t.contains(&needle))?;
            let doc: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
            let child = doc.nodes.iter().find(|n| n.name == "Fork probe child").context("child node")?;
            anyhow::ensure!(child.parent.as_deref() == Some(id.as_str()), "linked to its parent");
            let yaml = std::fs::read_to_string(sessions.join(&child.id).join("workspace.yaml")).context("forked session on disk")?;
            anyhow::ensure!(yaml.contains("Fork probe child"), "fork named: {yaml}");
            anyhow::ensure!(std::fs::read(sessions.join(&id).join("events.jsonl"))? == parent_events, "parent history untouched");
            println!("forked child {} opened with the parent's conversation", &child.id[..8]);
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(25)..].join("\n"));
        }
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod mcp_picker_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: `cargo test --release mcp_picker_probe -- --ignored --nocapture` (sandboxed COPILOT_HOME, no prompt sent).
    #[test]
    #[ignore]
    fn mcp_picker_probe() {
        let root = std::env::temp_dir().join(format!("cc-mcp-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in ["config.json", "settings.json", "mcp-config.json"] {
            let _ = std::fs::copy(real.join(f), copilot_home.join(f));
        }
        let instance = crate::paths::Instance::new(Some(format!("mcp{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&crate::client::doc::Doc::default()).unwrap()).unwrap();
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Your canvas is empty"))?;
            ui.send(b"n")?;
            ui.wait("dialog with MCP row", Duration::from_secs(60), |t| t.contains("MCP servers") && t.contains(" on") && t.contains("▾ choose"))?;
            ui.send(b"MCP probe")?;
            // Name → Folder → Browse… → Recent → Agent → Model → MCP servers.
            for _ in 0..6 {
                ui.send(b"\t")?;
            }
            ui.send(b"\r")?;
            ui.wait("picker", Duration::from_secs(10), |t| t.contains("MCP servers for the new session") && t.contains("[x]"))?;
            ui.send(b"n")?;
            ui.wait("none selected", Duration::from_secs(5), |t| t.contains(" 0 of ") && !t.contains("[x]"))?;
            ui.send(b"\x13")?;
            ui.wait("preset prompt", Duration::from_secs(5), |t| t.contains("Save MCP preset"))?;
            ui.send(b"Minimal\r")?;
            ui.wait("preset saved and chosen", Duration::from_secs(5), |t| t.contains(" Minimal ") && t.contains("MCP servers for the new session"))?;
            ui.send(b"\r")?;
            ui.wait("dialog shows the choice", Duration::from_secs(5), |t| t.contains("0 of ") && t.contains("“None”"))?;
            ui.send(b"\t\t\r")?;
            ui.wait("session started", Duration::from_secs(90), |t| t.contains("MCP probe · ") && t.contains("❯"))?;
            std::thread::sleep(Duration::from_secs(20));
            let log = std::fs::read_to_string(instance.log_path())?;
            let pid = log.lines().rev().find_map(|l| l.split(" pid ").nth(1)).and_then(|p| p.trim().parse::<u32>().ok()).context("pid")?;
            let query = format!("$c=(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine; ([regex]::Matches($c,'--disable-mcp-server')).Count; $all=Get-CimInstance Win32_Process; $kids=@{{}}; foreach($p in $all){{ $kids[[int]$p.ParentProcessId]+=@($p) }}; $q=@({pid}); $n=0; $agency=0; while($q.Count){{ $c=$q[0]; $q=@($q | Select-Object -Skip 1); foreach($k in @($kids[[int]$c])){{ if($k){{ $n++; if($k.Name -eq 'agency.exe'){{$agency++}}; $q+=[int]$k.ProcessId }} }} }}; $n; $agency");
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &query]).output()?;
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let nums: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
            println!("--disable-mcp-server flags: {} · processes under the session: {} · agency.exe: {}", nums[0], nums[1], nums[2]);
            anyhow::ensure!(nums[0] > 0, "flags passed");
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
            ui.send(b"\x1b[C\r")?;
            let _ = ui.wait_exit(Duration::from_secs(20));
            let doc: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
            anyhow::ensure!(doc.mcp.presets.iter().any(|p| p.name == "Minimal"), "preset remembered");
            anyhow::ensure!(!doc.mcp.last_off.is_empty(), "last choice remembered");
            anyhow::ensure!(doc.nodes.iter().any(|n| n.name == "MCP probe" && n.mcp_off.as_ref().is_some_and(|o| !o.is_empty())), "session remembers its servers");
            println!("remembered: preset Minimal, last choice ({} off), per-session list", doc.mcp.last_off.len());
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(30)..].join("\n"));
            ui.send(b"\x1b").ok();
            ui.send(b"\x1d").ok();
            ui.send(b"q").ok();
            ui.send(b"\x1b[C\r").ok();
            let _ = ui.wait_exit(Duration::from_secs(20));
        }
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod progress_probe {
    use std::time::Duration;

    /// Manual: `CC_RAW_CAPTURE=<file> cargo test --release progress_probe -- --ignored --nocapture`.
    /// One trivial turn in a sandboxed session (MCP servers off) to record Copilot's OSC 9;4 sequence.
    #[test]
    #[ignore]
    fn progress_probe() {
        let root = std::env::temp_dir().join(format!("cc-prog-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let mut args: Vec<String> = vec!["--allow-all".into(), "--disable-builtin-mcps".into(), "-i".into(), "Reply with just the word OK and nothing else.".into()];
        if let Some(c) = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect::<Vec<_>>().into_iter().reduce(|a, b| format!("{a}\n{b}")) {
            for name in c.lines() {
                args.extend(["--disable-mcp-server".into(), name.to_owned()]);
            }
        }
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, true).unwrap();
        let _ = ui.wait("reply", Duration::from_secs(120), |t| t.lines().any(|l| l.trim() == "● OK" || l.trim() == "OK"));
        std::thread::sleep(Duration::from_secs(8));
        let text = ui.text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        println!("--- screen tail ---\n{}", lines[lines.len().saturating_sub(8)..].join("\n"));
        let events = std::fs::read_dir(copilot_home.join("session-state")).unwrap().flatten().next().map(|d| std::fs::read_to_string(d.path().join("events.jsonl")).unwrap_or_default()).unwrap_or_default();
        let kinds: Vec<String> = events.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).map(|v| v["type"].as_str().unwrap_or("?").to_owned()).collect();
        println!("events: {}", kinds.join(" > "));
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod status_canvas_probe {
    use std::time::{Duration, Instant};

    /// Manual: `cargo test --release status_canvas_probe -- --ignored --nocapture`.
    /// One trivial turn in a sandboxed session started from the canvas (MCP servers off):
    /// records when the box shows working and when it shows done · review.
    #[test]
    #[ignore]
    fn status_canvas_probe() {
        let root = std::env::temp_dir().join(format!("cc-stat-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let mut doc = crate::client::doc::Doc::default();
        doc.mcp.last_off = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("stat{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Your canvas is empty"))?;
            ui.send(b"n")?;
            ui.wait("dialog", Duration::from_secs(30), |t| t.contains("First prompt") && t.contains("▾ choose"))?;
            ui.send(b"Status probe")?;
            // Name → Folder → Browse… → Recent → Agent → Model → MCP servers → First prompt.
            for _ in 0..7 {
                ui.send(b"\t")?;
            }
            ui.send(b"Reply with just the word OK and nothing else.\r")?;
            std::thread::sleep(Duration::from_millis(800));
            ui.send(b"\x1d")?; // back to the canvas, so the finished turn waits for review
            let start = Instant::now();
            let header = |t: &str| t.lines().find(|l| l.contains("Status probe · ")).map(|l| l.to_owned()).unwrap_or_default();
            let (mut working, mut replied, mut done) = (None, None, None);
            while start.elapsed() < Duration::from_secs(150) && done.is_none() {
                let text = ui.text();
                let h = header(&text);
                if working.is_none() && h.contains("· working") {
                    working = Some(start.elapsed());
                }
                if replied.is_none() && text.lines().any(|l| l.contains("● OK")) {
                    replied = Some(start.elapsed());
                }
                if replied.is_some() && h.contains("done · review") {
                    done = Some(start.elapsed());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("working seen at {working:?}, reply visible at {replied:?}, done · review at {done:?}");
            let (Some(r), Some(d)) = (replied, done) else { anyhow::bail!("did not reach done · review") };
            println!("reply → done · review: {} ms", d.saturating_sub(r).as_millis());
            anyhow::ensure!(working.is_some(), "working was shown");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(20)..].join("\n"));
        }
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod loop_canvas_probe {
    use std::io::Write;
    use std::time::Duration;

    /// Manual: `PROBE_SESSION=<id> cargo test --release loop_canvas_probe -- --ignored --nocapture`.
    /// A resumed sandbox copy of a session gets Copilot's schedule events appended:
    /// the box must loop while the schedule is active and stop when it is cancelled.
    #[test]
    #[ignore]
    fn loop_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-loop-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions.join(&id), &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Loop probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None,
            mcp_off: Some(crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect()),
            passive: None,
            no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("loop{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let events = sessions.join(&id).join("events.jsonl");
        let append = |line: String| {
            let mut f = std::fs::OpenOptions::new().append(true).open(&events).unwrap();
            writeln!(f, "{line}").unwrap();
        };
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Loop probe"))?;
            ui.send(b"\r")?;
            ui.wait("copilot resumed", Duration::from_secs(90), |t| t.contains("/ commands"))?;
            std::thread::sleep(Duration::from_secs(2));
            ui.send(b"\x1d")?;
            anyhow::ensure!(!ui.text().contains("↻"), "no loop before a schedule exists");
            let now = crate::paths::rfc3339_now();
            append(format!(r#"{{"type":"session.schedule_created","data":{{"id":7,"intervalMs":900000,"prompt":"probe","recurring":true,"origin":"model"}},"id":"probe-1","timestamp":"{now}","parentId":null}}"#));
            ui.wait("loop shown", Duration::from_secs(10), |t| t.contains("↻ every 15m") && t.contains("◂") && t.contains("↻ Loop probe"))?;
            println!("schedule created → box loops (↻ every 15m)");
            let now = crate::paths::rfc3339_now();
            append(format!(r#"{{"type":"session.schedule_cancelled","data":{{"id":7}},"id":"probe-2","timestamp":"{now}","parentId":null}}"#));
            ui.wait("loop removed", Duration::from_secs(10), |t| !t.contains("↻"))?;
            println!("schedule cancelled → loop gone");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen ---\n{}", lines[..lines.len().min(20)].join("\n"));
        }
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod plugin_probe {
    use std::time::Duration;

    /// Manual: `PROBE_PLUGIN=<dir> cargo test --release plugin_probe -- --ignored --nocapture`.
    /// Starts Copilot (sandboxed, no prompt) with `--plugin-dir` and lists skills.
    #[test]
    #[ignore]
    fn plugin_probe() {
        let plugin = std::env::var("PROBE_PLUGIN").unwrap();
        let root = std::env::temp_dir().join(format!("cc-plg-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let mut args: Vec<String> = vec!["--plugin-dir".into(), plugin.clone(), "--disable-builtin-mcps".into()];
        for name in crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name) {
            args.extend(["--disable-mcp-server".into(), name]);
        }
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 160, 50, true).unwrap();
        let _ = ui.wait("ready", Duration::from_secs(90), |t| t.contains("/ commands"));
        std::thread::sleep(Duration::from_secs(3));
        ui.send(b"/skills").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        ui.send(b"\r").unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let text = ui.text();
        println!("skill listed: {}", text.contains("canvas-probe-skill"));
        for line in text.lines().filter(|l| l.to_lowercase().contains("canvas-probe") || l.contains("plugin")) {
            println!("  {}", line.trim());
        }
        let _ = ui.send(b"\x1b");
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod passive_probe {
    use std::io::Write;
    use std::time::Duration;

    use anyhow::Context;

    use super::Ui;

    /// Manual (real Copilot, ~2 premium requests, sandboxed):
    /// `cargo test --release passive_probe -- --ignored --nocapture`.
    /// A passive session made in the UI runs on an emitted event, reports a
    /// status, messages the active session (typed in when idle), and a git
    /// push in the active session's history is detected as pr.pushed.
    #[test]
    #[ignore]
    fn passive_probe() {
        let root = std::env::temp_dir().join(format!("cc-psv-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work, folder) = (root.join("copilot"), root.join("canvas"), root.join("work"), root.join("group"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work, &folder] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let off: Vec<String> = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        let active = uuid::Uuid::new_v4().to_string();
        let mut doc = crate::client::doc::Doc::default();
        doc.mcp.last_off = off.clone();
        doc.groups.push(crate::client::doc::Group { id: "g-probe".into(), name: "Probe group".into(), x: 0, y: 0, w: 70, h: 16, color: 0, folder: Some(folder.display().to_string()), source: None, defaults: None });
        doc.nodes.push(crate::client::doc::Node {
            id: active.clone(), kind: crate::client::doc::NodeKind::Session, name: "Active probe".into(), x: 2, y: 2,
            cwd: Some(work.display().to_string()), parent: None, agent_key: None, source: None, command: None, fresh: true, automation: None, synced_name: None, reviewed_ms: None,
            mcp_off: Some(off.clone()), passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("psv{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 210, 56).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Probe group") && t.contains("Active probe"))?;
            println!("1. create a passive session from the group menu");
            let (gx, gy) = ui.find_in("Probe group", 34, u16::MAX).context("group title")?;
            ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", gx + 1, gy + 1, gx + 1, gy + 1).as_bytes())?;
            ui.wait("group menu", Duration::from_secs(5), |t| t.contains("New passive session…"))?;
            let (mx, my) = ui.find("New passive session…").context("menu item")?;
            ui.click(mx, my)?;
            ui.wait("templates", Duration::from_secs(5), |t| t.contains("Drift guard") && t.contains("Rollout tracker"))?;
            ui.send(b"Custom")?;
            ui.send(b"\r")?;
            ui.wait("name prompt", Duration::from_secs(5), |t| t.contains("Name the Custom passive session"))?;
            ui.send(b"\x15Probe watcher\r")?;
            ui.wait("detail panel", Duration::from_secs(10), |t| t.contains("⟲ Probe watcher") && t.contains("Events…"))?;
            let (ex, ey) = ui.find("Events…").context("events button")?;
            ui.click(ex + 1, ey)?;
            ui.wait("events prompt", Duration::from_secs(5), |t| t.contains("Events it reacts to"))?;
            ui.send(b"\x15probe.ping\r")?;
            ui.wait("subscribed", Duration::from_secs(5), |t| t.contains("on probe.ping"))?;
            let canvas: crate::client::doc::Doc = loop {
                std::thread::sleep(Duration::from_millis(500));
                let d: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
                if d.nodes.iter().any(|n| n.passive.as_ref().is_some_and(|p| p.subscribe == ["probe.ping"])) {
                    break d;
                }
            };
            let passive = canvas.nodes.iter().find(|n| n.passive.is_some()).context("passive node")?;
            let slug = passive.passive.as_ref().unwrap().slug.clone();
            let rules = folder.join(".canvas").join("passive").join(&slug).join("rules.md");
            anyhow::ensure!(rules.is_file() && folder.join("ai-docs").join("GOALS.md").is_file() && folder.join(".canvas").join("events.yaml").is_file(), "files scaffolded");
            std::fs::write(&rules, "# Probe watcher\n\nOn probe.ping run exactly these two commands, nothing else, then stop:\n1. copilot-canvas status ok \"pong N\"  (N is the n field of the event data)\n2. copilot-canvas send --to \"Active probe\" \"pong N\"\n")?;

            println!("2. open the active session (it receives messages when idle)");
            let (ax, ay) = ui.locate("Active probe", super::CANVAS)?;
            ui.double_click(ax, ay)?;
            ui.wait("active session ready", Duration::from_secs(120), |t| t.contains("/ commands"))?;
            ui.send(b"\x1d")?;
            std::thread::sleep(Duration::from_secs(3));

            println!("3. the active session emits probe.ping (as its skill would)");
            let status = std::process::Command::new(&exe)
                .args(["emit", "probe.ping", "--data", r#"{"n": 7}"#])
                .env("COPILOT_CANVAS_HOME", &home)
                .env("COPILOT_CANVAS_SESSION", &active)
                .status()?;
            anyhow::ensure!(status.success(), "emit failed");
            let started = std::time::Instant::now();
            ui.wait("passive ran and reported", Duration::from_secs(240), |t| t.contains("pong 7") && t.contains("ok ·"))?;
            println!("   passive reported \"ok · pong 7\" after {:.0}s", started.elapsed().as_secs_f32());
            ui.wait("message typed into the active session", Duration::from_secs(120), |t| t.contains("[From Probe watcher via Copilot Canvas] pong 7"))?;
            println!("   message delivered into the active session");

            println!("4. a git push in the active session's history becomes pr.pushed");
            let events = sessions.join(&active).join("events.jsonl");
            let now = crate::paths::rfc3339_now();
            let mut f = std::fs::OpenOptions::new().append(true).open(&events)?;
            writeln!(f, r#"{{"type":"tool.execution_start","timestamp":"{now}","data":{{"toolCallId":"probe-push","toolName":"powershell","arguments":{{"command":"git push origin feature"}}}}}}"#)?;
            writeln!(f, r#"{{"type":"tool.execution_complete","timestamp":"{now}","data":{{"toolCallId":"probe-push","success":true,"shellExecution":{{"exitCode":0}},"result":"To https://example/repo\n   1111111..2222222  feature -> feature"}}}}"#)?;
            drop(f);
            let (px, py) = ui.locate("Probe watcher", super::CANVAS)?;
            ui.click(px, py)?;
            ui.wait("pr.pushed in the group's recent events", Duration::from_secs(20), |t| t.contains("pr.pushed · Active probe"))?;
            println!("   detected pr.pushed from the active session");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            println!("--- screen ---\n{text}");
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            println!("--- service log ---\n{}", log.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        ui.send(b"\x1b").ok();
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(4));
        }
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        } else {
            println!("artifacts kept in {}", root.display());
        }
        result.unwrap();
    }
}

#[cfg(test)]
mod factory_probe {
    use std::time::Duration;

    use anyhow::Context;

    fn sandbox(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("cc-{tag}-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, sessions) = (root.join("copilot"), root.join("canvas"), root.join("copilot").join("session-state"));
        for d in [&sessions, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        (root, copilot_home, home, sessions)
    }

    fn quit(ui: &mut super::Console, instance: &crate::paths::Instance) {
        ui.send(b"\x1b").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit") || t.contains("Quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    /// Manual, read-only (no Copilot): your real automations on a sandbox canvas.
    /// `PROBE_AUTOMATIONS=<manifest folder> cargo test --release factory_canvas -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn factory_canvas() {
        let dir = std::env::var("PROBE_AUTOMATIONS").expect("set PROBE_AUTOMATIONS to a manifest folder");
        let (root, copilot_home, home, sessions) = sandbox("fac");
        let doc = crate::client::doc::Doc { automations_dir: Some(dir), doctor_created: true, ..Default::default() };
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fac{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 230, 64).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("automations on the canvas", Duration::from_secs(40), |t| t.contains("ExcelTeams-Morning") && t.contains("Excel Teams pipeline"))?;
            std::thread::sleep(Duration::from_secs(2));
            println!("--- canvas ---\n{}", ui.text());
            let (x, y) = ui.find_in("◷ ExcelTeams-Morning", 34, u16::MAX).context("Morning box")?;
            ui.click(x, y)?;
            ui.wait("info page", Duration::from_secs(10), |t| t.contains("◷ ExcelTeams-Morning") && t.contains("Runs "))?;
            std::thread::sleep(Duration::from_millis(500));
            println!("--- info page ---\n{}", ui.text());
            Ok(())
        })();
        if result.is_err() {
            println!("--- screen ---\n{}", ui.text());
        }
        quit(&mut ui, &instance);
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }

    /// Manual (real Copilot, ~1-2 premium requests; registers and removes a
    /// temporary scheduled task): `cargo test --release factory_probe -- --ignored --nocapture`.
    /// A failing automation pulses on the canvas, the default doctor is created
    /// for the automations group, wakes on automation.failed and reports.
    #[test]
    #[ignore]
    fn factory_probe() {
        let (root, copilot_home, home, sessions) = sandbox("fpr");
        let (dir, state) = (root.join("automations"), root.join("state"));
        std::fs::create_dir_all(dir.join("probe")).unwrap();
        let task = format!("CanvasProbe-{}", std::process::id());
        let python = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap()).join(r"Programs\Python\Python314\pythonw.exe");
        std::fs::write(
            dir.join("automations.yaml"),
            format!("launcher: 'Q:\\repos\\Automations\\scripts\\run_background_task.py'\npython: '{}'\nstate: '{}'\n", python.display(), state.display()),
        )
        .unwrap();
        std::fs::write(
            dir.join("probe").join(format!("{task}.yaml")),
            format!("name: '{task}'\nfamily: 'Probe family'\ncommand: 'C:\\Windows\\System32\\cmd.exe'\nargs: '/c exit 3'\ncwd: '{}'\nschedule:\n  - 'daily 03:17'\ntime_limit: '5m'\n", root.display()),
        )
        .unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let applied = std::process::Command::new(&exe).args(["automations", "apply", "--yes", "--dir"]).arg(&dir).output().unwrap();
        println!("{}", String::from_utf8_lossy(&applied.stdout));
        assert!(applied.status.success(), "{}", String::from_utf8_lossy(&applied.stderr));
        let off: Vec<String> = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        let mut doc = crate::client::doc::Doc { automations_dir: Some(dir.display().to_string()), ..Default::default() };
        doc.mcp.last_off = off;
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fpr{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 210, 56).unwrap();
        let result = (|| -> anyhow::Result<()> {
            println!("1. the automation appears in its family and the doctor is added");
            ui.wait("automation and doctor", Duration::from_secs(40), |t| t.contains(&task) && t.contains("Probe family") && t.contains("⟲ Doctor"))?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let canvas: crate::client::doc::Doc = loop {
                let d: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
                if d.nodes.iter().any(|n| n.passive.is_some()) || std::time::Instant::now() > deadline {
                    break d;
                }
                std::thread::sleep(Duration::from_millis(300));
            };
            let doctor = canvas.nodes.iter().find(|n| n.passive.is_some()).context("doctor node")?;
            let passive = doctor.passive.as_ref().unwrap();
            anyhow::ensure!(passive.subscribe == ["automation.failed", "automation.check"], "doctor subscriptions {:?}", passive.subscribe);
            let folder = canvas.group(&passive.group).and_then(|g| g.folder.clone()).context("automations group folder")?;
            let rules = crate::bus::passive_paths(std::path::Path::new(&folder), &passive.slug).rules;
            anyhow::ensure!(std::fs::read_to_string(&rules)?.contains("Automation doctor"), "doctor rules scaffolded");
            std::fs::write(&rules, "# Probe doctor\n\nOn automation.failed run exactly this command, nothing else, then stop:\ncopilot-canvas status alert \"saw NAME exit EXIT\"  (NAME and EXIT from the event data: name, exit)\n")?;
            std::thread::sleep(Duration::from_secs(3));

            println!("2. the task fails: its box pulses and the toolbar counts it");
            crate::taskschd::run(&task)?;
            ui.wait("failure shown", Duration::from_secs(60), |t| t.contains("last run failed") && t.contains("1 automation failing"))?;
            println!("3. the service wakes the doctor, which reports");
            let started = std::time::Instant::now();
            ui.wait("doctor reported", Duration::from_secs(300), |t| t.contains(&format!("saw {task} exit 3")))?;
            println!("   doctor reported after {:.0}s", started.elapsed().as_secs_f32());
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            anyhow::ensure!(log.contains(&format!("automation.failed {task}: exit code 3")), "service logged the failure");

            println!("4. the info page shows the failed run");
            let (x, y) = ui.find_in(&format!("◷ {task}"), 34, u16::MAX).context("task box")?;
            ui.click(x, y)?;
            ui.wait("info page", Duration::from_secs(10), |t| t.contains("exit code 3") && t.contains("1 of 1 failed") && t.contains("Ask doctor"))?;
            Ok(())
        })();
        if result.is_err() {
            println!("--- screen ---\n{}", ui.text());
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            println!("--- service log ---\n{}", log.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        quit(&mut ui, &instance);
        let _ = crate::taskschd::delete(&task);
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        } else {
            println!("artifacts kept in {}", root.display());
        }
        result.unwrap();
    }
}

#[cfg(test)]
mod two_clients_probe {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::Console;

    /// Manual: `cargo build --release; cargo test --release two_clients_probe -- --ignored --nocapture`.
    /// Two TUI clients on one instance in hidden pseudoconsoles, one over
    /// `--remote loopback` and one over the local pipe: a box renamed in
    /// either shows in the other (the service owns the canvas and patches both).
    #[test]
    #[ignore]
    fn two_clients_probe() {
        let root = std::env::temp_dir().join(format!("cc-two-clients-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work) = (root.join("home"), root.join("sessions"), root.join("work"));
        for d in [&sessions, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let instance = crate::paths::Instance::new(Some(format!("two{}", std::process::id())), Some(home), Some(sessions)).unwrap();
        let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args = |remote: bool| -> Vec<String> {
            let mut args: Vec<String> = vec![
                "--demo".into(), "--instance".into(), instance.name.clone(), "--home".into(), instance.home.to_string_lossy().into_owned(),
                "--sessions-dir".into(), instance.sessions_dir.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned(),
            ];
            if remote {
                args.extend(["--remote".into(), "loopback".into()]);
            }
            args
        };
        let result = (|| -> anyhow::Result<()> {
            // The remote client starts the service listening on loopback; the local one joins over the pipe.
            let mut remote = Console::start(&exe, &args(true), 150, 40)?;
            remote.wait("remote canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
            let mut local = Console::start(&exe, &args(false), 150, 40)?;
            local.wait("local canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
            println!("  · rename in the remote client, see it in the local one");
            remote.send(b"r")?;
            remote.wait("rename dialog", Duration::from_secs(5), |t| t.contains("Rename (canvas label)"))?;
            remote.send(b"\x15Renamed remotely\r")?;
            if let Err(error) = local.wait("the remote rename", Duration::from_secs(5), |t| t.contains("Renamed remotely")) {
                println!("--- remote ---\n{}", remote.text());
                println!("--- saved ---\n{}", std::fs::read_to_string(instance.home.join("demo-canvas.json")).unwrap_or_default().lines().filter(|l| l.contains("\"name\"")).collect::<Vec<_>>().join("\n"));
                println!("--- service log ---\n{}", std::fs::read_to_string(instance.log_path()).unwrap_or_default());
                return Err(error);
            }
            println!("  · rename in the local client, see it in the remote one");
            local.send(b"r")?;
            local.wait("rename dialog", Duration::from_secs(5), |t| t.contains("Rename (canvas label)"))?;
            local.send(b"\x15Renamed locally\r")?;
            remote.wait("the local rename", Duration::from_secs(5), |t| t.contains("Renamed locally"))?;
            println!("  · panning one client leaves the other's view alone");
            // The box on the canvas (left of the detail panel on the right).
            let label = |ui: &Console| ui.find_in("Renamed locally", 34, 84);
            let (before, moved) = (label(&remote), label(&local));
            anyhow::ensure!(before.is_some() && moved.is_some(), "the box is on both canvases");
            local.send(b"\x1b[1;5D")?;
            local.wait("the local view panned", Duration::from_secs(5), |_| label(&local) != moved)?;
            std::thread::sleep(Duration::from_millis(500));
            anyhow::ensure!(label(&remote) == before, "the remote view moved with the local one");
            for ui in [&mut remote, &mut local] {
                ui.send(b"q")?;
                anyhow::ensure!(ui.wait_exit(Duration::from_secs(10))? == 0, "client exit code");
            }
            let saved: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(instance.home.join("demo-canvas.json"))?)?;
            anyhow::ensure!(saved.node("demo-main").is_some_and(|n| n.name == "Renamed locally"), "the service saved the last rename");
            println!("PASS");
            Ok(())
        })();
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_millis(1500));
        }
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        } else {
            println!("artifacts kept in {}", root.display());
        }
        result.unwrap();
    }
}