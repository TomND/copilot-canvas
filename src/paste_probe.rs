//! `copilot-canvas paste-probe [--plain] [--kitty] [--keys] [--raw]`
//! (hidden): a program that shows how pasted text reaches it, for the
//! copy-and-paste checks (`selftest`, `selftest --client gui --parity`,
//! `selftest agent --paste`).
//!
//! It turns bracketed paste on (`--plain`: leaves it off) and, with
//! `--kitty`, the kitty keyboard protocol (disambiguate), reads raw VT input
//! (as the agent CLIs do) and prints one line per item:
//!
//! - `PASTE <n> lines: <text>`: a bracketed paste (control characters
//!   escaped: `\r`, `\n`, `\t`, `\e`, `\xNN`);
//! - `ENTER`: a carriage return outside a paste (what submits a message);
//! - `LF`, `ALT-ENTER` (ESC CR), `SHIFT-ENTER` (CSI 13;2u): line-break keys;
//! - `TEXT <text>` and `KEY <sequence>` for everything else.
//!
//! Ctrl+D (or Ctrl+C) quits. `--raw` also prints every read (`RAW …`).
//! `--keys` reads console key events instead (as the TUI does, through
//! crossterm) and prints each one.
//!
//! `paste-probe --drive TEXT [flags]` runs the probe in a hidden
//! pseudoconsole, writes TEXT (escapes `\e`, `\r`, `\n`, `\t`, `\xNN`, `\\`)
//! to it and prints its screen: what a program behind ConPTY receives.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;

/// Printed once the probe reads input.
pub const READY: &str = "PASTE-PROBE READY";
/// Printed for a carriage return outside a paste.
pub const ENTER: &str = "ENTER";

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// One thing the program received.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A bracketed paste's content.
    Paste(String),
    Enter,
    LineFeed,
    AltEnter,
    ShiftEnter,
    Text(String),
    Key(String),
    Quit,
}

impl Item {
    /// The line the probe prints.
    pub fn line(&self) -> String {
        match self {
            Item::Paste(text) => format!("PASTE {} lines: {}", line_count(text), escape(text)),
            Item::Enter => ENTER.into(),
            Item::LineFeed => "LF".into(),
            Item::AltEnter => "ALT-ENTER".into(),
            Item::ShiftEnter => "SHIFT-ENTER".into(),
            Item::Text(text) => format!("TEXT {}", escape(text)),
            Item::Key(seq) => format!("KEY {}", escape(seq)),
            Item::Quit => "BYE".into(),
        }
    }
}

/// Lines of pasted text (CR LF, CR and LF each end one).
pub fn line_count(text: &str) -> usize {
    text.replace("\r\n", "\n").replace('\r', "\n").matches('\n').count() + 1
}

/// Control characters as `\r`, `\n`, `\t`, `\e`, `\xNN`; a backslash doubled.
pub fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\x1b' => out.push_str("\\e"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The inverse of [`escape`] (for `--drive`).
pub fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).map(char::from).unwrap_or('?'));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Splits raw VT input into [`Item`]s across reads.
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

/// Length of the escape sequence at the start of `bytes` (which starts with
/// ESC), or `None` while it is incomplete.
fn sequence_len(bytes: &[u8]) -> Option<usize> {
    match bytes.get(1)? {
        b'[' => {
            let end = bytes[2..].iter().position(|b| (0x40..=0x7e).contains(b))?;
            Some(end + 3)
        }
        b'O' => bytes.get(2).map(|_| 3),
        _ => Some(2),
    }
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Item> {
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        let mut text: Vec<u8> = Vec::new();
        let flush = |text: &mut Vec<u8>, out: &mut Vec<Item>| {
            if !text.is_empty() {
                out.push(Item::Text(String::from_utf8_lossy(text).into_owned()));
                text.clear();
            }
        };
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            if let Some(paste) = self.paste.as_mut() {
                if rest.starts_with(PASTE_END) {
                    out.push(Item::Paste(String::from_utf8_lossy(paste).into_owned()));
                    self.paste = None;
                    i += PASTE_END.len();
                } else if PASTE_END.starts_with(rest) {
                    break;
                } else {
                    paste.push(rest[0]);
                    i += 1;
                }
                continue;
            }
            match rest[0] {
                0x1b => {
                    flush(&mut text, &mut out);
                    let Some(len) = sequence_len(rest) else { break };
                    let seq = &rest[..len];
                    match seq {
                        PASTE_START => self.paste = Some(Vec::new()),
                        b"\x1b\r" => out.push(Item::AltEnter),
                        b"\x1b[13;2u" => out.push(Item::ShiftEnter),
                        b"\x1b[100;5u" | b"\x1b[99;5u" => out.push(Item::Quit),
                        _ => out.push(Item::Key(String::from_utf8_lossy(seq).into_owned())),
                    }
                    i += len;
                }
                b'\r' => {
                    flush(&mut text, &mut out);
                    out.push(Item::Enter);
                    i += 1;
                }
                b'\n' => {
                    flush(&mut text, &mut out);
                    out.push(Item::LineFeed);
                    i += 1;
                }
                0x03 | 0x04 => {
                    flush(&mut text, &mut out);
                    out.push(Item::Quit);
                    i += 1;
                }
                b if b < 0x20 || b == 0x7f => {
                    flush(&mut text, &mut out);
                    out.push(Item::Key((b as char).to_string()));
                    i += 1;
                }
                b => {
                    text.push(b);
                    i += 1;
                }
            }
        }
        flush(&mut text, &mut out);
        self.pending = buf[i..].to_vec();
        out
    }
}

pub fn run(plain: bool, kitty: bool, keys: bool, raw: bool) -> Result<()> {
    if keys {
        return run_keys(plain);
    }
    console_modes();
    let mut out = std::io::stdout();
    if !plain {
        write!(out, "\x1b[?2004h")?;
    }
    if kitty {
        write!(out, "\x1b[>1u")?;
    }
    write!(out, "{READY} (bracketed paste {}, kitty keyboard {})\r\n", if plain { "off" } else { "on" }, if kitty { "on" } else { "off" })?;
    out.flush()?;
    let mut stdin = RawStdin::new();
    let mut decoder = Decoder::default();
    'read: loop {
        let input = stdin.read_some()?;
        if input.is_empty() {
            break;
        }
        if raw {
            write!(out, "RAW {}\r\n", escape(&String::from_utf8_lossy(&input)))?;
        }
        for item in decoder.feed(&input) {
            write!(out, "{}\r\n", item.line())?;
            if item == Item::Quit {
                break 'read;
            }
        }
        out.flush()?;
    }
    write!(out, "\x1b[?2004l\x1b[<u")?;
    out.flush()?;
    Ok(())
}

/// Console key events through crossterm, as the TUI reads them.
fn run_keys(plain: bool) -> Result<()> {
    use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers};
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    if !plain {
        ratatui::crossterm::execute!(out, EnableBracketedPaste)?;
    }
    write!(out, "{READY} (console key events, bracketed paste {})\r\n", if plain { "off" } else { "on" })?;
    out.flush()?;
    loop {
        let first = event::read()?;
        let mut batch = vec![first];
        while event::poll(Duration::ZERO)? {
            batch.push(event::read()?);
        }
        write!(out, "BATCH {}\r\n", batch.len())?;
        let mut quit = false;
        for event in batch {
            if let Event::Key(key) = &event
                && key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('d')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                quit = true;
            }
            let line = match &event {
                Event::Key(key) => format!("{:?} {:?} {:?}", key.kind, key.code, key.modifiers),
                Event::Paste(text) => format!("PASTE {}", escape(text)),
                other => format!("{other:?}"),
            };
            write!(out, "  {line}\r\n")?;
        }
        out.flush()?;
        if quit {
            break;
        }
    }
    if !plain {
        ratatui::crossterm::execute!(out, DisableBracketedPaste)?;
    }
    ratatui::crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// Standard input as the console delivers it: unlike `std::io::stdin`,
/// which ends a console's input at Ctrl+Z (0x1A), Ctrl+Z arrives as a key
/// (`KEY \x1a`), as it does in the agent CLIs. Text comes as UTF-8.
struct RawStdin(windows_sys::Win32::Foundation::HANDLE);

impl RawStdin {
    fn new() -> Self {
        // SAFETY: the process's own standard input handle.
        Self(unsafe { windows_sys::Win32::System::Console::GetStdHandle(windows_sys::Win32::System::Console::STD_INPUT_HANDLE) })
    }

    /// The next input (empty at its end).
    fn read_some(&mut self) -> std::io::Result<Vec<u8>> {
        let mut wide = [0u16; 4096];
        let mut chars = 0u32;
        // SAFETY: `wide` is valid for its length for the whole (synchronous) call; no read control.
        let console = unsafe { windows_sys::Win32::System::Console::ReadConsoleW(self.0, wide.as_mut_ptr().cast(), wide.len() as u32, &mut chars, std::ptr::null()) };
        if console != 0 {
            return Ok(String::from_utf16_lossy(&wide[..chars as usize]).into_bytes());
        }
        // Not a console (a pipe): its bytes.
        let mut buf = vec![0u8; 8192];
        let mut read = 0u32;
        // SAFETY: as above.
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(read as usize);
        Ok(buf)
    }
}

/// Raw VT input (no line editing, echo or Ctrl+C handling), VT output.
fn console_modes() {
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: the process's own standard handles; failures leave the modes as they were.
    unsafe {
        SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), ENABLE_VIRTUAL_TERMINAL_INPUT);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(output, &mut mode) != 0 {
            SetConsoleMode(output, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// `--drive TEXT`: the probe (or `program`, once `ready` shows) in a hidden
/// pseudoconsole; prints its screen after TEXT was written to it.
pub fn drive(text: &str, flags: &[&str], program: Option<&str>, ready: Option<&str>) -> Result<()> {
    let (exe, args, ready) = match program {
        Some(program) => {
            let mut words = program.split_whitespace();
            let exe = std::path::PathBuf::from(words.next().unwrap_or_default());
            (exe, words.map(str::to_owned).collect::<Vec<_>>(), ready.unwrap_or(">").to_owned())
        }
        None => {
            let mut args = vec!["paste-probe".to_owned()];
            args.extend(flags.iter().map(|f| format!("--{f}")));
            (std::env::current_exe()?, args, READY.to_owned())
        }
    };
    let mut console = crate::selftest::Console::start(&exe, &args, 120, 40)?;
    console.wait("the program", Duration::from_secs(30), |t| t.contains(&ready))?;
    std::thread::sleep(Duration::from_millis(500));
    console.send_now(unescape(text).as_bytes())?;
    std::thread::sleep(Duration::from_millis(2500));
    println!("{}", console.text().trim_end());
    if program.is_none() {
        console.send_now(b"\x04")?;
        let _ = console.wait_exit(Duration::from_secs(5));
    } else {
        // Ends the program.
        let _ = console.wait_exit(Duration::ZERO);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pastes_and_line_break_keys_are_told_apart_across_reads() {
        let mut d = Decoder::default();
        assert_eq!(d.feed(b"ab\x1b[200~one\r\n\r\n  two\x1b[2"), vec![Item::Text("ab".into())]);
        assert_eq!(d.feed(b"01~\rx\x1b\r\x1b[13;2u\n\x1b[A\x04"), vec![
            Item::Paste("one\r\n\r\n  two".into()),
            Item::Enter,
            Item::Text("x".into()),
            Item::AltEnter,
            Item::ShiftEnter,
            Item::LineFeed,
            Item::Key("\x1b[A".into()),
            Item::Quit,
        ]);
        assert_eq!(Item::Paste("one\r\n\r\n  two".into()).line(), "PASTE 3 lines: one\\r\\n\\r\\n  two");
        assert_eq!(d.feed(b"\x1b"), vec![], "an escape waits for the rest of its sequence");
        assert_eq!(d.feed(b"[99;5u"), vec![Item::Quit]);
    }

    #[test]
    fn escapes_round_trip() {
        let text = "a\r\n\tb\x1b[200~\\ \x01";
        assert_eq!(escape(text), "a\\r\\n\\tb\\e[200~\\\\ \\x01");
        assert_eq!(unescape(&escape(text)), text);
        assert_eq!(line_count("a\r\nb\rc\nd"), 4);
    }
}
