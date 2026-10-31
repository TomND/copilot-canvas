//! The ConPTY recorder: runs a program at a fixed size, answers its terminal
//! queries, types an optional input script and records every output read.
//!
//! Script lines (`#` starts a comment):
//!
//! ```text
//! wait 500              sleep (ms)
//! wait-for PS C:\       until the screen shows the text (timeout: `timeout MS`, default 15000)
//! wait-quiet 300        until no output arrived for 300 ms since the step started (at most the timeout)
//! timeout 20000         timeout of later wait-for / wait-quiet steps
//! type dir\r            raw text; escapes \r \n \t \e \\ \xNN
//! line dir              text, then Enter
//! key enter             a key, encoded for the program's modes: enter tab backtab backspace esc
//!                       up down left right home end pgup pgdn insert delete space f1..f12 or one
//!                       character, with ctrl- alt- shift- prefixes (key ctrl-c)
//! paste text            bracketed paste when the program asked for it
//! resize 100 30         resize the pseudoconsole
//! mark NAME             checkpoint: expected files keep the screen here
//! kill                  end the program now
//! ```

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use canvas_common::protocol::{Input, Key, MOD_ALT, MOD_CTRL, MOD_SHIFT};
use canvas_host::emulator::Engine;
use crossbeam_channel::{RecvTimeoutError, unbounded};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::replay::{Expected, EventLog, Screen, new_emulator, row_text};
use crate::vtrec::{Event, Header, Kind, Recording, VERSION, clamp_size};

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Wait(u64),
    WaitFor(String, u64),
    WaitQuiet(u64, u64),
    Send(Input),
    Resize(u16, u16),
    Mark(String),
    Kill,
}

/// Default timeout of `wait-for` and `wait-quiet`.
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// `\r \n \t \e \\ \xNN` escapes.
pub fn unescape(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let digits: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&digits, 16).with_context(|| format!("bad escape \\x{digits}"))?);
            }
            other => bail!("unknown escape \\{}", other.map(String::from).unwrap_or_default()),
        }
    }
    Ok(out)
}

/// `[ctrl-][alt-][shift-]name`.
pub fn parse_key(spec: &str) -> Result<Input> {
    let mut mods = 0u8;
    let mut rest = spec.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let (prefix, bit) = if lower.starts_with("ctrl-") && rest.len() > 5 {
            (5, MOD_CTRL)
        } else if lower.starts_with("alt-") && rest.len() > 4 {
            (4, MOD_ALT)
        } else if lower.starts_with("shift-") && rest.len() > 6 {
            (6, MOD_SHIFT)
        } else {
            break;
        };
        mods |= bit;
        rest = &rest[prefix..];
    }
    let key = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "backtab" => Key::BackTab,
        "backspace" | "bs" => Key::Backspace,
        "esc" | "escape" => Key::Esc,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pgup" | "pageup" => Key::PageUp,
        "pgdn" | "pagedown" => Key::PageDown,
        "insert" | "ins" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "space" => Key::Char(' '),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => Key::F(f[1..].parse()?),
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Char(c),
                _ => bail!("unknown key {spec:?}"),
            }
        }
    };
    Ok(Input::Key { key, mods })
}

pub fn parse_script(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut timeout = DEFAULT_TIMEOUT_MS;
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (command, arg) = match trimmed.split_once(' ') {
            Some((command, arg)) => (command, arg),
            None => (trimmed, ""),
        };
        let number = number + 1;
        let ms = |arg: &str| arg.trim().parse::<u64>().with_context(|| format!("line {number}: {command} needs milliseconds"));
        let step = match command {
            "wait" => Step::Wait(ms(arg)?),
            "timeout" => {
                timeout = ms(arg)?;
                continue;
            }
            "wait-for" => {
                ensure!(!arg.is_empty(), "line {number}: wait-for needs text");
                Step::WaitFor(arg.to_owned(), timeout)
            }
            "wait-quiet" => Step::WaitQuiet(ms(arg)?, timeout),
            "type" => Step::Send(Input::Raw(unescape(arg).with_context(|| format!("line {number}"))?)),
            "line" => {
                steps.push(Step::Send(Input::Raw(unescape(arg).with_context(|| format!("line {number}"))?)));
                Step::Send(Input::Key { key: Key::Enter, mods: 0 })
            }
            "key" => Step::Send(parse_key(arg).with_context(|| format!("line {number}"))?),
            "paste" => Step::Send(Input::Paste(String::from_utf8(unescape(arg)?).context("paste text must be UTF-8")?)),
            "resize" => {
                let mut parts = arg.split_whitespace().map(str::parse::<u16>);
                match (parts.next(), parts.next()) {
                    (Some(Ok(cols)), Some(Ok(rows))) if cols >= 2 && rows >= 2 => {
                        let (cols, rows) = clamp_size(cols, rows);
                        Step::Resize(cols, rows)
                    }
                    _ => bail!("line {number}: resize COLS ROWS"),
                }
            }
            "mark" => {
                ensure!(!arg.trim().is_empty(), "line {number}: mark needs a name");
                Step::Mark(arg.trim().to_owned())
            }
            "kill" => Step::Kill,
            other => bail!("line {number}: unknown script command {other:?}"),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// What to record.
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Added to the inherited environment (like hosted terminals: TERM, COLORTERM).
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// Recording stops (the program is killed) after this long.
    pub limit: Duration,
    /// Recording stops (the program is killed) after this much output
    /// ([`MAX_RECORDED_BYTES`] for the CLI); the description says so.
    pub max_bytes: u64,
    pub script: Vec<Step>,
    pub description: String,
    /// Prints the script's progress to stderr.
    pub verbose: bool,
}

/// Recorder-side state while a step runs.
enum Running {
    Idle,
    Until(Instant),
    WaitFor(String, Instant),
    /// Quiet period, step start, timeout.
    Quiet(Duration, Instant, Instant),
}

/// Output a recording keeps at most (`copilot-canvas record`).
pub const MAX_RECORDED_BYTES: u64 = 64 << 20;

/// `at + by`, or a year later when that does not fit (durations come from
/// scripts and the command line).
fn later(at: Instant, by: Duration) -> Instant {
    at.checked_add(by).or_else(|| at.checked_add(Duration::from_secs(365 * 86_400))).unwrap_or(at)
}

/// Runs the program and returns the recording.
pub fn record(spec: &Spec) -> Result<Recording> {
    ensure!(spec.cols >= 20 && spec.rows >= 5, "the pseudoconsole needs at least 20x5");
    let (cols, rows) = clamp_size(spec.cols, spec.rows);
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).context("cannot create a pseudoconsole")?;
    let mut command = CommandBuilder::new(&spec.program);
    command.args(&spec.args);
    let cwd = match &spec.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir()?,
    };
    command.cwd(&cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command).with_context(|| format!("cannot start {}", spec.program))?;
    let started = Instant::now();
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut master = Some(pair.master);
    let (tx, rx) = unbounded::<(Instant, Vec<u8>)>();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 || tx.send((Instant::now(), buffer[..n].to_vec())).is_err() {
                break;
            }
        }
    });

    let mut emu = new_emulator(Engine::Alacritty, cols, rows)?;
    let mut recording = Recording::new(Header {
        vtrec: VERSION,
        cols,
        rows,
        program: std::iter::once(spec.program.as_str()).chain(spec.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        description: spec.description.clone(),
        source: "conpty".into(),
        recorded_at: canvas_common::paths::rfc3339_now(),
    });
    let mut last = started;
    let mut push = |recording: &mut Recording, at: Instant, kind: Kind| {
        let delta_ms = at.saturating_duration_since(last).as_millis() as u64;
        last = last.max(at);
        recording.events.push(Event { delta_ms, kind });
    };
    let deadline = later(started, spec.limit);
    let mut recorded = 0u64;
    let mut capped = false;
    let mut steps = spec.script.iter();
    let mut running = Running::Idle;
    let mut last_output = started;
    let mut exited_at: Option<Instant> = None;
    let mut killed = false;
    let mut script_error: Option<anyhow::Error> = None;

    let mut reader_done = false;
    loop {
        let mut batch = Vec::new();
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(item) => batch.push(item),
            Err(RecvTimeoutError::Disconnected) => {
                reader_done = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        batch.extend(rx.try_iter());
        for (at, bytes) in batch {
            last_output = at;
            if capped || recorded.saturating_add(bytes.len() as u64) > spec.max_bytes {
                if !capped {
                    eprintln!("record: {} bytes recorded, the limit; ending the program", recorded);
                    capped = true;
                    killed = true;
                }
                continue;
            }
            recorded += bytes.len() as u64;
            let mut events = EventLog::default();
            emu.feed(&bytes, &mut events);
            for reply in events.replies {
                let _ = writer.write_all(reply.as_bytes()).and_then(|_| writer.flush());
            }
            push(&mut recording, at, Kind::Output(bytes));
        }
        let now = Instant::now();
        if let Some(at) = exited_at {
            // The program is gone: let ConPTY flush, then close it so the reader ends.
            if master.is_some() && now >= later(at, Duration::from_millis(300)) {
                master = None;
            }
            if now >= later(at, Duration::from_secs(5)) {
                break;
            }
            continue;
        }
        if now >= later(deadline, Duration::from_secs(10)) {
            bail!("the program did not end after being killed");
        }
        if child.try_wait()?.is_some() {
            exited_at = Some(now);
            continue;
        }
        if now >= deadline || killed {
            if spec.verbose && !killed {
                eprintln!("record: time limit reached; ending the program");
            }
            if pid != 0 {
                let _ = canvas_sys::process::kill_tree(pid);
            }
            let _ = child.kill();
            killed = true;
            continue;
        }
        if script_error.is_some() {
            killed = true;
            continue;
        }
        let done = match &running {
            Running::Idle => true,
            Running::Until(until) => now >= *until,
            Running::WaitFor(text, until) => {
                let screen = emu.snapshot();
                if screen.lines.iter().any(|row| row_text(row).contains(text.as_str())) {
                    true
                } else if now >= *until {
                    let shown = screen.lines.iter().map(|r| row_text(r)).collect::<Vec<_>>().join("\n");
                    script_error = Some(anyhow::anyhow!("timed out waiting for {text:?}\n--- screen ---\n{shown}"));
                    continue;
                } else {
                    false
                }
            }
            // Quiet for the whole period since the step started: slow programs answer late.
            Running::Quiet(quiet, since, until) => now.saturating_duration_since(last_output.max(*since)) >= *quiet || now >= *until,
        };
        if !done {
            continue;
        }
        running = Running::Idle;
        let Some(step) = steps.next() else { continue };
        if spec.verbose {
            let what = match step {
                Step::Send(Input::Raw(bytes)) => format!("type {:?}", String::from_utf8_lossy(bytes)),
                other => format!("{other:?}"),
            };
            eprintln!("record: {:>6} ms  {what}", started.elapsed().as_millis());
        }
        match step {
            Step::Wait(ms) => running = Running::Until(later(now, Duration::from_millis(*ms))),
            Step::WaitFor(text, timeout) => running = Running::WaitFor(text.clone(), later(now, Duration::from_millis(*timeout))),
            Step::WaitQuiet(quiet, timeout) => running = Running::Quiet(Duration::from_millis(*quiet), now, later(now, Duration::from_millis(*timeout))),
            Step::Send(input) => {
                let bytes = emu.encode(input);
                // A program that just exited cannot read; its recording is still wanted.
                let _ = writer.write_all(&bytes).and_then(|_| writer.flush());
                push(&mut recording, now, Kind::Input(bytes));
            }
            Step::Resize(cols, rows) => {
                if let Some(master) = &master {
                    master.resize(PtySize { rows: *rows, cols: *cols, pixel_width: 0, pixel_height: 0 })?;
                }
                emu.resize(*cols, *rows);
                push(&mut recording, now, Kind::Resize(*cols, *rows));
            }
            Step::Mark(name) => push(&mut recording, now, Kind::Mark(name.clone())),
            Step::Kill => killed = true,
        }
    }
    drop(master);
    drop(writer);
    if reader_done {
        let _ = reader_thread.join();
    }
    if let Some(error) = script_error {
        return Err(error);
    }
    if steps.next().is_some() && spec.verbose {
        eprintln!("record: the program ended before the script did");
    }
    if capped {
        let note = format!("(output capped at {} bytes)", spec.max_bytes);
        recording.header.description = if recording.header.description.is_empty() { note } else { format!("{} {note}", recording.header.description) };
    }
    Ok(recording)
}

/// What a recording shows on the recorder's own engine (for a summary).
pub fn preview(recording: &Recording) -> Result<Expected> {
    crate::replay::replay(Engine::Alacritty, recording)
}

/// The text of a screen (rows joined).
pub fn screen_text(screen: &Screen) -> String {
    screen.text.join("\n").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scripts() {
        let script = "# comment\nwait 50\ntimeout 900\nwait-for PS C:\\>  \ntype a\\tb\\e\\x41\\\\\nline dir\nkey ctrl-c\nkey Shift-Tab\nkey f5\nkey q\npaste x\\ny\nresize 100 30\nwait-quiet 200\nmark shot one\nkill\n";
        let steps = parse_script(script).unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Wait(50),
                Step::WaitFor("PS C:\\>  ".into(), 900),
                Step::Send(Input::Raw(b"a\tb\x1bA\\".to_vec())),
                Step::Send(Input::Raw(b"dir".to_vec())),
                Step::Send(Input::Key { key: Key::Enter, mods: 0 }),
                Step::Send(Input::Key { key: Key::Char('c'), mods: MOD_CTRL }),
                Step::Send(Input::Key { key: Key::Tab, mods: MOD_SHIFT }),
                Step::Send(Input::Key { key: Key::F(5), mods: 0 }),
                Step::Send(Input::Key { key: Key::Char('q'), mods: 0 }),
                Step::Send(Input::Paste("x\ny".into())),
                Step::Resize(100, 30),
                Step::WaitQuiet(200, 900),
                Step::Mark("shot one".into()),
                Step::Kill,
            ]
        );
        for bad in ["wait soon", "key ctrl-nope", "resize 1", "frobnicate", "type \\q", "mark"] {
            assert!(parse_script(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn records_a_program_and_its_scripted_input() {
        let script = parse_script("wait-for ready\nmark ready\nline echo typed-in\nwait-for typed-in\nline exit\n").unwrap();
        let spec = Spec {
            program: "cmd.exe".into(),
            args: vec!["/q".into(), "/k".into(), "prompt $g& echo ready".into()],
            cwd: None,
            env: Vec::new(),
            cols: 60,
            rows: 10,
            limit: Duration::from_secs(20),
            max_bytes: MAX_RECORDED_BYTES,
            script,
            description: "test".into(),
            verbose: false,
        };
        let recording = record(&spec).unwrap();
        assert!(recording.chunks() > 0);
        assert_eq!(recording.marks(), ["ready"]);
        assert!(recording.events.iter().any(|e| e.kind == Kind::Input(b"echo typed-in".to_vec())));
        let expected = preview(&recording).unwrap();
        assert!(expected.screens[0].text.iter().any(|l| l.contains("ready")));
        let end = screen_text(expected.screens.last().unwrap());
        assert!(end.contains("typed-in"), "{end}");
    }

    #[test]
    fn output_beyond_the_cap_ends_the_recording() {
        let spec = Spec {
            program: "cmd.exe".into(),
            args: vec!["/d".into(), "/c".into(), "for /l %i in (1,1,100000) do @echo 0123456789abcdefghijklmnopqrstuvwxyz".into()],
            cwd: None,
            env: Vec::new(),
            cols: 60,
            rows: 10,
            limit: Duration::from_secs(30),
            max_bytes: 4096,
            script: Vec::new(),
            description: "flood".into(),
            verbose: false,
        };
        let started = Instant::now();
        let recording = record(&spec).unwrap();
        assert!(recording.output_bytes() <= 4096, "{} bytes", recording.output_bytes());
        assert_eq!(recording.header.description, "flood (output capped at 4096 bytes)");
        assert!(started.elapsed() < Duration::from_secs(20), "the program is ended at the cap");
    }

    #[test]
    fn huge_script_values_neither_overflow_nor_allocate() {
        assert_eq!(parse_script("resize 60000 60000").unwrap(), [Step::Resize(crate::vtrec::MAX_COLS, crate::vtrec::MAX_ROWS)]);
        let now = Instant::now();
        assert!(later(now, Duration::from_millis(u64::MAX)) >= now);
        assert!(later(now, Duration::MAX) >= now);
    }
}
