//! A screen as the bytes that draw it (docs/VM-SESSIONS.md A2c, the gap
//! reset): when a VM session's satellite can no longer replay the output the
//! hub missed, it sends its own engine's screen as a repaint, and the hub
//! feeds it to a fresh engine. Cells (text, wide characters and combining
//! marks), colours, attributes, underline styles and colours, hyperlinks,
//! the alternate screen, the input modes, the cursor's place and shape come
//! back; the scrollback before the gap and soft-wrap state do not.

use canvas_common::protocol::*;

use super::Snapshot;

/// SGR parameters of a colour (`base` 38 foreground, 48 background, 58 underline).
fn color(out: &mut String, base: u32, color: u32) {
    if color == COLOR_DEFAULT {
        if base != 58 {
            out.push_str(&format!(";{}", base + 1));
        }
    } else if color & COLOR_RGB != 0 {
        out.push_str(&format!(";{base};2;{};{};{}", (color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff));
    } else {
        out.push_str(&format!(";{base};5;{}", (color - 1) & 0xff));
    }
}

fn sgr(run: &Run8) -> String {
    let mut out = String::from("\x1b[0");
    let flags = run.flags;
    for (bit, code) in [(F_BOLD as u32, "1"), (F_DIM as u32, "2"), (F_ITALIC as u32, "3"), (F8_BLINK, "5"), (F_INVERSE as u32, "7"), (F_HIDDEN as u32, "8"), (F_STRIKE as u32, "9"), (F8_OVERLINE, "53")] {
        if flags & bit != 0 {
            out.push(';');
            out.push_str(code);
        }
    }
    if flags & F_UNDERLINE as u32 != 0 {
        match (flags & F8_UL_MASK) >> F8_UL_SHIFT {
            UL_DOUBLE => out.push_str(";4:2"),
            UL_CURLY => out.push_str(";4:3"),
            UL_DOTTED => out.push_str(";4:4"),
            UL_DASHED => out.push_str(";4:5"),
            _ => out.push_str(";4"),
        }
    }
    color(&mut out, 38, run.fg);
    color(&mut out, 48, run.bg);
    if run.underline != COLOR_DEFAULT {
        color(&mut out, 58, run.underline);
    }
    out.push('m');
    out
}

/// The bytes that draw `snapshot` on a reset terminal of its size.
pub fn repaint(snapshot: &Snapshot) -> Vec<u8> {
    let mut out = String::from("\x1bc");
    let modes = snapshot.modes;
    if modes & MODE_ALT_SCREEN != 0 {
        out.push_str("\x1b[?1049h");
    }
    out.push_str("\x1b[H\x1b[2J");
    for (y, row) in snapshot.lines.iter().enumerate() {
        for run in row.iter().filter(|r| !r.is_soft_wrap() && !r.text.is_empty()) {
            out.push_str(&format!("\x1b[{};{}H", y + 1, run.col + 1));
            out.push_str(&sgr(run));
            let link = (run.link > 0).then(|| snapshot.links.get(run.link as usize - 1)).flatten();
            if let Some(uri) = link {
                out.push_str(&format!("\x1b]8;;{uri}\x1b\\"));
            }
            out.push_str(&run.text);
            if link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
        }
    }
    out.push_str("\x1b[0m");
    for (bit, mode) in [
        (MODE_MOUSE, 1000),
        (MODE_MOUSE_DRAG, 1002),
        (MODE_MOUSE_MOTION, 1003),
        (MODE_SGR_MOUSE, 1006),
        (MODE_UTF8_MOUSE, 1005),
        (MODE_BRACKETED_PASTE, 2004),
        (MODE_APP_CURSOR, 1),
        (MODE_FOCUS, 1004),
    ] {
        if modes & bit != 0 {
            out.push_str(&format!("\x1b[?{mode}h"));
        }
    }
    if modes & MODE_APP_KEYPAD != 0 {
        out.push_str("\x1b=");
    }
    let kitty = (modes >> MODE_KITTY_SHIFT) & 0x1f;
    if kitty != 0 {
        out.push_str(&format!("\x1b[>{kitty}u"));
    }
    let shape = match (snapshot.cursor_shape, snapshot.cursor_blink) {
        (CURSOR_UNDERLINE, true) => 3,
        (CURSOR_UNDERLINE, false) => 4,
        (CURSOR_BAR, true) => 5,
        (CURSOR_BAR, false) => 6,
        (_, true) => 1,
        (_, false) => 2,
    };
    out.push_str(&format!("\x1b[{shape} q"));
    if modes & MODE_SHOW_CURSOR == 0 {
        out.push_str("\x1b[?25l");
    }
    if let Some((x, y)) = snapshot.cursor {
        out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{Config, Emulator, alacritty::AlacrittyEmulator, feed_collect};

    fn engine() -> AlacrittyEmulator {
        AlacrittyEmulator::new(&Config { cols: 40, rows: 8, scrollback: 100, kitty_keyboard: true, pull_scrollback: true })
    }

    /// Without the soft-wrap markers (a repaint does not restore them).
    fn cells(snapshot: &Snapshot) -> Vec<Vec<Run8>> {
        snapshot.lines.iter().map(|row| row.iter().filter(|r| !r.is_soft_wrap()).cloned().collect()).collect()
    }

    fn round_trip(bytes: &[u8]) {
        let mut a = engine();
        feed_collect(&mut a, bytes);
        let before = a.snapshot();
        let mut b = engine();
        feed_collect(&mut b, &repaint(&before));
        let after = b.snapshot();
        assert_eq!(cells(&after), cells(&before), "cells after repainting {:?}", String::from_utf8_lossy(bytes));
        assert_eq!(after.links, before.links);
        assert_eq!((after.cursor, after.cursor_shape, after.cursor_blink, after.modes), (before.cursor, before.cursor_shape, before.cursor_blink, before.modes));
    }

    #[test]
    fn a_repaint_draws_the_same_screen_on_a_fresh_engine() {
        round_trip(b"plain text\r\nsecond line");
        round_trip("\x1b[1;3;4mbold italic underline\x1b[0m \x1b[38;5;196mred\x1b[48;2;1;2;3m on rgb\x1b[0m\r\n\x1b[4:3;58;2;255;0;0mcurly\x1b[0m \x1b[9;53mstruck\x1b[0m \x1b[2;38;5;3mdim\x1b[0m".as_bytes());
        round_trip("wide 中文 and e\u{301} combined\r\n\x1b]8;;https://example.com/x\x1b\\a link\x1b]8;;\x1b\\ after".as_bytes());
        round_trip(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b=\x1b[>1u\x1b[5 q\x1b[3;7Hfull screen");
        round_trip(b"\x1b[?25l\x1b[4 qhidden cursor\x1b[7;2Hlow");
        round_trip(b"\x1b[7mreverse\x1b[27m \x1b[8mhidden\x1b[0m");
    }
}
