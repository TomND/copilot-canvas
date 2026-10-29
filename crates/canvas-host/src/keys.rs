//! Encodes structured key input into the byte sequences a terminal application
//! expects, honoring the application's current terminal modes (`MODE_*` bits).

use crate::protocol::{
    Input, Key, MOD_ALT, MOD_CTRL, MOD_SHIFT, MODE_ALT_SCREEN, MODE_APP_CURSOR, MODE_BRACKETED_PASTE, MODE_FOCUS, MODE_KITTY_DISAMBIGUATE,
    MODE_MOUSE, MODE_SGR_MOUSE, MODE_UTF8_MOUSE,
};

fn modifier_param(mods: u8) -> u8 {
    1 + if mods & MOD_SHIFT != 0 { 1 } else { 0 } + if mods & MOD_ALT != 0 { 2 } else { 0 } + if mods & MOD_CTRL != 0 { 4 } else { 0 }
}

pub fn encode(input: &Input, mode: u32) -> Vec<u8> {
    match input {
        Input::Raw(bytes) => bytes.clone(),
        Input::Paste(text) => {
            let lines = paste_lines(text);
            let lines: Vec<String> = lines.iter().map(|line| sanitize(line)).collect();
            join_paste(&lines, mode, &encode_key(Key::Enter, MOD_SHIFT, mode))
        }
        Input::Focus(focused) => {
            if mode & MODE_FOCUS != 0 {
                if *focused { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() }
            } else {
                Vec::new()
            }
        }
        Input::Key { key, mods } => encode_key(*key, *mods, mode),
    }
}

/// The lines of pasted text, by the rules every engine follows:
///
/// - paste markers (`ESC [200~`, `ESC [201~`) are removed, also ones that
///   removing another would form, so the text cannot end a bracketed paste
///   and inject keys;
/// - CR LF, CR and LF each end a line;
/// - line breaks at the end are dropped: a paste never ends with Enter.
///
/// Empty text (or only line breaks) has no lines.
pub fn paste_lines(text: &str) -> Vec<String> {
    let mut clean = text.to_owned();
    while clean.contains("\x1b[200~") || clean.contains("\x1b[201~") {
        clean = clean.replace("\x1b[200~", "").replace("\x1b[201~", "");
    }
    let clean = clean.replace("\r\n", "\n").replace('\r', "\n");
    let clean = clean.trim_end_matches('\n');
    if clean.is_empty() {
        return Vec::new();
    }
    clean.split('\n').map(str::to_owned).collect()
}

/// Control characters a paste may not carry become spaces, exactly as
/// Ghostty's paste encoder does (xterm's disallowed paste controls: NUL,
/// Ctrl+C/D/E/O/Q/R/S/U/V/W/Z, BS, ESC, FS and DEL), so neither engine lets
/// pasted text act as those keys.
pub fn sanitize(line: &str) -> String {
    line.chars().map(|c| if matches!(c, '\0' | '\x03'..='\x05' | '\x08' | '\x0f' | '\x11'..='\x13' | '\x15'..='\x17' | '\x1a'..='\x1c' | '\x7f') { ' ' } else { c }).collect()
}

/// Encoded paste of `lines` (already sanitized): one bracketed paste whose
/// lines end with CR, as Windows Terminal and xterm send them, when the
/// program enabled bracketed paste; otherwise the lines separated by
/// `newline_key`, the program's "insert a line break" key (Shift+Enter),
/// never by a bare CR (Enter submits). No lines: nothing.
pub fn join_paste(lines: &[String], mode: u32, newline_key: &[u8]) -> Vec<u8> {
    if lines.is_empty() {
        return Vec::new();
    }
    if mode & MODE_BRACKETED_PASTE != 0 {
        return format!("\x1b[200~{}\x1b[201~", lines.join("\r")).into_bytes();
    }
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(newline_key);
        }
        out.extend_from_slice(line.as_bytes());
    }
    out
}

/// Bytes for a mouse-wheel notch, or `None` when the wheel should scroll the
/// terminal's own scrollback (normal screen, or Shift held).
pub fn encode_wheel(up: bool, col: u16, row: u16, mods: u8, mode: u32) -> Option<Vec<u8>> {
    if mods & MOD_SHIFT != 0 {
        return None;
    }
    if mode & MODE_MOUSE != 0 {
        let mut button: u32 = if up { 64 } else { 65 };
        if mods & MOD_ALT != 0 {
            button += 8;
        }
        if mods & MOD_CTRL != 0 {
            button += 16;
        }
        let (x, y) = (col as u32 + 1, row as u32 + 1);
        if mode & MODE_SGR_MOUSE != 0 {
            return Some(format!("\x1b[<{button};{x};{y}M").into_bytes());
        }
        let mut bytes = b"\x1b[M".to_vec();
        if mode & MODE_UTF8_MOUSE != 0 {
            for value in [32 + button, 32 + x.min(2015), 32 + y.min(2015)] {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(char::from_u32(value).unwrap_or(' ').encode_utf8(&mut buf).as_bytes());
            }
        } else {
            bytes.extend([(32 + button) as u8, (32 + x.min(223)) as u8, (32 + y.min(223)) as u8]);
        }
        return Some(bytes);
    }
    if mode & MODE_ALT_SCREEN != 0 {
        // Full-screen apps without mouse support (e.g. Copilot with `mouse` off) page with these.
        return Some(if up { b"\x1b[5~".to_vec() } else { b"\x1b[6~".to_vec() });
    }
    None
}

/// Press + release of a mouse button, or `None` when the app did not ask for
/// mouse reports (then a click only focuses and selects in the canvas).
pub fn encode_click(button: u8, col: u16, row: u16, mods: u8, mode: u32) -> Option<Vec<u8>> {
    if !mode & MODE_MOUSE != 0 || button > 2 {
        return None;
    }
    let mut code = button as u32;
    if mods & MOD_SHIFT != 0 {
        code += 4;
    }
    if mods & MOD_ALT != 0 {
        code += 8;
    }
    if mods & MOD_CTRL != 0 {
        code += 16;
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if mode & MODE_SGR_MOUSE != 0 {
        return Some(format!("\x1b[<{code};{x};{y}M\x1b[<{code};{x};{y}m").into_bytes());
    }
    let release = (code & !3) | 3;
    let mut bytes = Vec::new();
    for value in [code, release] {
        bytes.extend_from_slice(b"\x1b[M");
        if mode & MODE_UTF8_MOUSE != 0 {
            for v in [32 + value, 32 + x.min(2015), 32 + y.min(2015)] {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(char::from_u32(v).unwrap_or(' ').encode_utf8(&mut buf).as_bytes());
            }
        } else {
            bytes.extend([(32 + value) as u8, (32 + x.min(223)) as u8, (32 + y.min(223)) as u8]);
        }
    }
    Some(bytes)
}

fn csi_mod(final_byte: char, mods: u8) -> Vec<u8> {
    format!("\x1b[1;{}{final_byte}", modifier_param(mods)).into_bytes()
}

fn tilde(code: u8, mods: u8) -> Vec<u8> {
    if mods == 0 { format!("\x1b[{code}~").into_bytes() } else { format!("\x1b[{code};{}~", modifier_param(mods)).into_bytes() }
}

fn encode_key(key: Key, mods: u8, mode: u32) -> Vec<u8> {
    let kitty = mode & MODE_KITTY_DISAMBIGUATE != 0;
    let alt = mods & MOD_ALT != 0;
    let ctrl = mods & MOD_CTRL != 0;
    let with_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    match key {
        Key::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                let control = match lower {
                    'a'..='z' => Some(lower as u8 - b'a' + 1),
                    '@' | ' ' | '2' => Some(0),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '^' | '6' => Some(0x1e),
                    '_' | '-' | '7' => Some(0x1f),
                    '?' | '8' => Some(0x7f),
                    _ => None,
                };
                if let Some(byte) = control {
                    return with_alt(vec![byte]);
                }
            }
            let mut buf = [0u8; 4];
            with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        Key::Enter => {
            if kitty && mods & (MOD_SHIFT | MOD_CTRL) != 0 {
                format!("\x1b[13;{}u", modifier_param(mods)).into_bytes()
            } else if mods & MOD_SHIFT != 0 {
                // Without the kitty protocol, Alt+Enter is the de-facto "newline" chord.
                b"\x1b\r".to_vec()
            } else {
                with_alt(b"\r".to_vec())
            }
        }
        Key::Tab => {
            if mods & MOD_SHIFT != 0 { b"\x1b[Z".to_vec() } else { with_alt(b"\t".to_vec()) }
        }
        Key::BackTab => b"\x1b[Z".to_vec(),
        Key::Backspace => {
            if ctrl { with_alt(vec![0x08]) } else { with_alt(vec![0x7f]) }
        }
        Key::Esc => {
            if kitty { b"\x1b[27u".to_vec() } else { with_alt(vec![0x1b]) }
        }
        Key::Up | Key::Down | Key::Right | Key::Left | Key::Home | Key::End => {
            let final_byte = match key {
                Key::Up => 'A',
                Key::Down => 'B',
                Key::Right => 'C',
                Key::Left => 'D',
                Key::Home => 'H',
                _ => 'F',
            };
            if mods != 0 {
                csi_mod(final_byte, mods)
            } else if mode & MODE_APP_CURSOR != 0 {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                format!("\x1b[{final_byte}").into_bytes()
            }
        }
        Key::PageUp => tilde(5, mods),
        Key::PageDown => tilde(6, mods),
        Key::Insert => tilde(2, mods),
        Key::Delete => tilde(3, mods),
        Key::F(n) => match n {
            1..=4 => {
                let final_byte = (b'P' + n - 1) as char;
                if mods == 0 { format!("\x1bO{final_byte}").into_bytes() } else { csi_mod(final_byte, mods) }
            }
            5 => tilde(15, mods),
            6 => tilde(17, mods),
            7 => tilde(18, mods),
            8 => tilde(19, mods),
            9 => tilde(20, mods),
            10 => tilde(21, mods),
            11 => tilde(23, mods),
            12 => tilde(24, mods),
            _ => Vec::new(),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn key(k: Key, mods: u8) -> Vec<u8> {
        encode(&Input::Key { key: k, mods }, 0)
    }

    #[test]
    fn printable_and_control_keys() {
        assert_eq!(key(Key::Char('a'), 0), b"a");
        assert_eq!(key(Key::Char('é'), 0), "é".as_bytes());
        assert_eq!(key(Key::Char('c'), MOD_CTRL), [3]);
        assert_eq!(key(Key::Char('C'), MOD_CTRL | MOD_SHIFT), [3]);
        assert_eq!(key(Key::Char('x'), MOD_ALT), b"\x1bx");
        assert_eq!(key(Key::Char(' '), MOD_CTRL), [0]);
        assert_eq!(key(Key::Enter, 0), b"\r");
        assert_eq!(key(Key::Backspace, 0), [0x7f]);
        assert_eq!(key(Key::Tab, MOD_SHIFT), b"\x1b[Z");
    }

    #[test]
    fn cursor_keys_follow_application_mode_and_modifiers() {
        assert_eq!(key(Key::Up, 0), b"\x1b[A");
        assert_eq!(encode(&Input::Key { key: Key::Up, mods: 0 }, MODE_APP_CURSOR), b"\x1bOA");
        assert_eq!(key(Key::Left, MOD_CTRL), b"\x1b[1;5D");
        assert_eq!(key(Key::Delete, 0), b"\x1b[3~");
        assert_eq!(key(Key::PageUp, MOD_SHIFT), b"\x1b[5;2~");
        assert_eq!(key(Key::F(1), 0), b"\x1bOP");
        assert_eq!(key(Key::F(12), 0), b"\x1b[24~");
    }

    #[test]
    fn clicks_reach_the_app_only_when_it_asked_for_mouse_reports() {
        assert_eq!(encode_click(0, 4, 2, 0, 0), None);
        let sgr = MODE_MOUSE | MODE_SGR_MOUSE;
        assert_eq!(encode_click(0, 4, 2, 0, sgr).unwrap(), b"\x1b[<0;5;3M\x1b[<0;5;3m");
        assert_eq!(encode_click(0, 0, 0, MOD_CTRL, sgr).unwrap(), b"\x1b[<16;1;1M\x1b[<16;1;1m");
        let x10 = MODE_MOUSE;
        assert_eq!(encode_click(0, 0, 0, 0, x10).unwrap(), b"\x1b[M !!\x1b[M#!!");
    }

    #[test]
    fn shift_enter_uses_kitty_protocol_when_enabled() {
        assert_eq!(key(Key::Enter, MOD_SHIFT), b"\x1b\r");
        let kitty = MODE_KITTY_DISAMBIGUATE;
        assert_eq!(encode(&Input::Key { key: Key::Enter, mods: MOD_SHIFT }, kitty), b"\x1b[13;2u");
        assert_eq!(encode(&Input::Key { key: Key::Esc, mods: 0 }, kitty), b"\x1b[27u");
    }

    #[test]
    fn paste_is_bracketed_and_cannot_terminate_early() {
        let bracketed = MODE_BRACKETED_PASTE;
        let encoded = encode(&Input::Paste("a\x1b[201~b\nc".into()), bracketed);
        assert_eq!(encoded, b"\x1b[200~ab\rc\x1b[201~");
        let nested = encode(&Input::Paste("a\x1b[20\x1b[201~1~b\x1b[2\x1b[200~00~c".into()), bracketed);
        assert_eq!(nested, b"\x1b[200~abc\x1b[201~", "markers formed by removing others go too");
        assert_eq!(encode(&Input::Paste("a\x1bb\x03\tc\x7f".into()), bracketed), b"\x1b[200~a b \tc \x1b[201~", "control characters become spaces");
    }

    /// A pasted text and its bytes: bracketed, plain, with the kitty keyboard protocol.
    pub(crate) type PasteCase = (&'static str, &'static [u8], &'static [u8], &'static [u8]);

    /// (pasted text, bracketed, plain, kitty keyboard) for both engines.
    pub(crate) const PASTES: &[PasteCase] = &[
        ("one line", b"\x1b[200~one line\x1b[201~", b"one line", b"one line"),
        ("a\r\nb\rc\nd", b"\x1b[200~a\rb\rc\rd\x1b[201~", b"a\x1b\rb\x1b\rc\x1b\rd", b"a\x1b[13;2ub\x1b[13;2uc\x1b[13;2ud"),
        ("ls\r\n", b"\x1b[200~ls\x1b[201~", b"ls", b"ls"),
        ("x\n\n\r\n", b"\x1b[200~x\x1b[201~", b"x", b"x"),
        ("def f():\r\n    return 1\r\n\r\n\r\nprint(f())\r\n", b"\x1b[200~def f():\r    return 1\r\r\rprint(f())\x1b[201~", b"def f():\x1b\r    return 1\x1b\r\x1b\r\x1b\rprint(f())", b"def f():\x1b[13;2u    return 1\x1b[13;2u\x1b[13;2u\x1b[13;2uprint(f())"),
        ("\r\n", b"", b"", b""),
        ("", b"", b"", b""),
        ("  lead\ttab  \n", b"\x1b[200~  lead\ttab  \x1b[201~", b"  lead\ttab  ", b"  lead\ttab  "),
        ("x\x1b[201~\ny", b"\x1b[200~x\ry\x1b[201~", b"x\x1b\ry", b"x\x1b[13;2uy"),
        ("中文\r\ne\u{301}", "\x1b[200~中文\re\u{301}\x1b[201~".as_bytes(), "中文\x1b\re\u{301}".as_bytes(), "中文\x1b[13;2ue\u{301}".as_bytes()),
    ];

    #[test]
    fn pasted_line_breaks_never_act_as_enter() {
        let kitty = MODE_KITTY_DISAMBIGUATE;
        for &(text, bracketed, plain, kitty_bytes) in PASTES {
            let input = Input::Paste(text.into());
            assert_eq!(encode(&input, MODE_BRACKETED_PASTE), bracketed, "{text:?} bracketed");
            assert_eq!(encode(&input, MODE_BRACKETED_PASTE | kitty), bracketed, "{text:?} bracketed with kitty keys");
            assert_eq!(encode(&input, 0), plain, "{text:?} plain");
            assert_eq!(encode(&input, kitty), kitty_bytes, "{text:?} kitty");
            // Outside a bracketed paste no CR or LF is sent except as part of Alt+Enter.
            for bytes in [plain, kitty_bytes] {
                let rest = String::from_utf8_lossy(bytes).replace("\x1b\r", "");
                assert!(!rest.contains(['\r', '\n']), "{text:?}: {bytes:?}");
            }
        }
        assert_eq!(paste_lines("a\r\n\r\nb\r\n"), ["a", "", "b"]);
        assert!(paste_lines("\n\r\n").is_empty());
    }

    #[test]
    fn focus_reports_only_when_requested() {
        assert!(encode(&Input::Focus(true), 0).is_empty());
        assert_eq!(encode(&Input::Focus(true), MODE_FOCUS), b"\x1b[I");
    }

    #[test]
    fn wheel_reaches_full_screen_apps_and_scrolls_history_otherwise() {
        let plain = 0;
        assert_eq!(encode_wheel(true, 4, 2, 0, plain), None, "normal screen: scrollback");
        let alt = MODE_ALT_SCREEN;
        assert_eq!(encode_wheel(true, 0, 0, 0, alt).as_deref(), Some(&b"\x1b[5~"[..]));
        assert_eq!(encode_wheel(false, 0, 0, 0, alt).as_deref(), Some(&b"\x1b[6~"[..]));
        assert_eq!(encode_wheel(true, 0, 0, MOD_SHIFT, alt), None, "Shift forces scrollback");
        let sgr = MODE_ALT_SCREEN | MODE_MOUSE | MODE_SGR_MOUSE;
        assert_eq!(encode_wheel(true, 4, 2, 0, sgr).as_deref(), Some(&b"\x1b[<64;5;3M"[..]));
        assert_eq!(encode_wheel(false, 4, 2, MOD_CTRL, sgr).as_deref(), Some(&b"\x1b[<81;5;3M"[..]));
        let x10 = MODE_MOUSE;
        assert_eq!(encode_wheel(true, 0, 0, 0, x10).as_deref(), Some(&[0x1b, b'[', b'M', 96, 33, 33][..]));
    }
}
