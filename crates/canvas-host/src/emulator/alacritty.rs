//! The alacritty_terminal engine behind [`Emulator`].

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll as GridScroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use canvas_common::protocol::*;

use super::{Config, Emulator, Engine, Events, Scroll, Snapshot};

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

enum Pending {
    Reply(Vec<u8>),
    Title(Option<String>),
    Bell,
    Clipboard(String),
}

#[derive(Clone)]
pub struct Listener {
    pending: Arc<Mutex<Vec<Pending>>>,
    size: Arc<AtomicU32>,
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
        let pending = match event {
            Event::PtyWrite(text) => Pending::Reply(text.into_bytes()),
            Event::Title(title) => Pending::Title(Some(title)),
            Event::ResetTitle => Pending::Title(None),
            Event::Bell => Pending::Bell,
            Event::ClipboardStore(_, text) => Pending::Clipboard(text),
            Event::ColorRequest(index, format) => Pending::Reply(format(palette(index)).into_bytes()),
            Event::TextAreaSizeRequest(format) => {
                let size = self.size.load(Ordering::Relaxed);
                let window = WindowSize { num_cols: (size >> 16) as u16, num_lines: (size & 0xffff) as u16, cell_width: 8, cell_height: 16 };
                Pending::Reply(format(window).into_bytes())
            }
            // Clipboard reads are refused: applications must not read the user's clipboard.
            _ => return,
        };
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).push(pending);
    }
}

pub struct AlacrittyEmulator {
    pub term: Term<Listener>,
    parser: Processor,
    pending: Arc<Mutex<Vec<Pending>>>,
    size: Arc<AtomicU32>,
    osc: OscScan,
}

impl AlacrittyEmulator {
    pub fn new(config: &Config) -> Self {
        let pending = Arc::new(Mutex::new(Vec::new()));
        let size = Arc::new(AtomicU32::new(((config.cols as u32) << 16) | config.rows as u32));
        let listener = Listener { pending: pending.clone(), size: size.clone() };
        let term_config = TermConfig { scrolling_history: config.scrollback, kitty_keyboard: config.kitty_keyboard, ..TermConfig::default() };
        let term = Term::new(term_config, &Dims { cols: config.cols as usize, rows: config.rows as usize }, listener);
        Self { term, parser: Processor::new(), pending, size, osc: OscScan::default() }
    }

    fn drain(&mut self, events: &mut dyn Events) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        for item in pending {
            match item {
                Pending::Reply(bytes) => events.reply(bytes),
                Pending::Title(title) => events.title(title),
                Pending::Bell => events.bell(),
                Pending::Clipboard(text) => events.clipboard(text),
            }
        }
    }
}

fn color(color: Color, flags: &mut u32) -> u32 {
    match color {
        Color::Spec(rgb) => COLOR_RGB | (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32,
        Color::Indexed(index) => index as u32 + 1,
        Color::Named(named) => {
            let value = named as usize;
            if value < 16 {
                value as u32 + 1
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&value) {
                *flags |= F_DIM as u32;
                (value - NamedColor::DimBlack as usize) as u32 + 1
            } else {
                if named == NamedColor::DimForeground {
                    *flags |= F_DIM as u32;
                }
                COLOR_DEFAULT
            }
        }
    }
}

fn trim_trailing_blank(runs: &mut Vec<Run8>) {
    while let Some(run) = runs.last_mut() {
        if run.bg != COLOR_DEFAULT || run.flags & (F_INVERSE | F_UNDERLINE | F_STRIKE) as u32 != 0 || run.link != 0 {
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

impl Emulator for AlacrittyEmulator {
    fn engine(&self) -> Engine {
        Engine::Alacritty
    }

    fn feed(&mut self, bytes: &[u8], events: &mut dyn Events) {
        // alacritty ignores progress and notification sequences; find them in the raw output.
        for event in self.osc.feed(bytes) {
            match event {
                OscEvent::Progress(state) => events.progress(state),
                OscEvent::Notify => events.notify(),
            }
        }
        self.parser.advance(&mut self.term, bytes);
        self.drain(events);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size.store(((cols as u32) << 16) | rows as u32, Ordering::Relaxed);
        self.term.resize(Dims { cols: cols as usize, rows: rows as usize });
    }

    fn snapshot(&self) -> Snapshot {
        let term = &self.term;
        let grid = term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let offset = grid.display_offset();
        let mut links: Vec<String> = Vec::new();
        let mut lines = Vec::with_capacity(rows);
        for y in 0..rows {
            let row = &grid[Line(y as i32 - offset as i32)];
            let mut runs: Vec<Run8> = Vec::new();
            for x in 0..cols {
                let cell = &row[Column(x)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let mut flags = 0u32;
                let fg = color(cell.fg, &mut flags);
                let mut bg_flags = 0u32;
                let bg = color(cell.bg, &mut bg_flags);
                let f = cell.flags;
                for (flag, bit) in [
                    (Flags::BOLD, F_BOLD),
                    (Flags::ITALIC, F_ITALIC),
                    (Flags::INVERSE, F_INVERSE),
                    (Flags::DIM, F_DIM),
                    (Flags::HIDDEN, F_HIDDEN),
                    (Flags::STRIKEOUT, F_STRIKE),
                ] {
                    if f.contains(flag) {
                        flags |= bit as u32;
                    }
                }
                let underline = if f.contains(Flags::DOUBLE_UNDERLINE) {
                    UL_DOUBLE
                } else if f.contains(Flags::UNDERCURL) {
                    UL_CURLY
                } else if f.contains(Flags::DOTTED_UNDERLINE) {
                    UL_DOTTED
                } else if f.contains(Flags::DASHED_UNDERLINE) {
                    UL_DASHED
                } else if f.contains(Flags::UNDERLINE) {
                    UL_SINGLE
                } else {
                    0
                };
                if underline != 0 {
                    flags |= F_UNDERLINE as u32 | underline << F8_UL_SHIFT;
                }
                let mut ul_flags = 0;
                let underline_color = cell.underline_color().map(|c| color(c, &mut ul_flags)).unwrap_or(COLOR_DEFAULT);
                let ch = if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
                let link = match cell.hyperlink() {
                    Some(h) => {
                        let uri = h.uri();
                        match links.iter().position(|l| l == uri) {
                            Some(i) => i as u32 + 1,
                            None => {
                                links.push(uri.to_owned());
                                links.len() as u32
                            }
                        }
                    }
                    None => 0,
                };
                let wide = f.contains(Flags::WIDE_CHAR);
                let extra: Vec<char> = cell.zerowidth().map(|z| z.to_vec()).unwrap_or_default();
                let same = |run: &Run8| {
                    run.fg == fg && run.bg == bg && run.flags == flags && run.link == link && run.wide == wide && run.underline == underline_color
                };
                let run = match runs.last_mut() {
                    Some(run) if same(run) => run,
                    _ => {
                        runs.push(Run8 { col: x as u16, fg, bg, flags, link, wide, underline: underline_color, ..Default::default() });
                        runs.last_mut().expect("just pushed")
                    }
                };
                if !extra.is_empty() {
                    let index = run.text.chars().count() - run.clusters.iter().map(|(_, n)| *n as usize).sum::<usize>();
                    run.clusters.push((index as u16, extra.len().min(255) as u8));
                }
                run.text.push(ch);
                run.text.extend(extra);
            }
            trim_trailing_blank(&mut runs);
            lines.push(runs);
        }
        let mode = *term.mode();
        let cursor_point = grid.cursor.point;
        let cursor = (offset == 0 && mode.contains(TermMode::SHOW_CURSOR))
            .then(|| (cursor_point.column.0 as u16, cursor_point.line.0.max(0) as u16));
        let style = term.cursor_style();
        let cursor_shape = match style.shape {
            CursorShape::Underline => CURSOR_UNDERLINE,
            CursorShape::Beam => CURSOR_BAR,
            CursorShape::HollowBlock => CURSOR_BLOCK_HOLLOW,
            _ => CURSOR_BLOCK,
        };
        Snapshot {
            cols: cols as u16,
            rows: rows as u16,
            lines,
            links,
            cursor,
            cursor_shape,
            cursor_blink: style.blinking,
            display_offset: offset as u32,
            history: grid.history_size() as u32,
            modes: self.modes(),
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        self.term.scroll_display(match scroll {
            Scroll::Delta(d) => GridScroll::Delta(d),
            Scroll::Top => GridScroll::Top,
            Scroll::Bottom => GridScroll::Bottom,
        });
    }

    fn display_offset(&self) -> u32 {
        self.term.grid().display_offset() as u32
    }

    fn modes(&self) -> u32 {
        modes(*self.term.mode())
    }

    fn encode(&mut self, input: &Input) -> Vec<u8> {
        crate::keys::encode(input, self.modes())
    }

    fn encode_wheel(&mut self, up: bool, col: u16, row: u16, mods: u8) -> Option<Vec<u8>> {
        crate::keys::encode_wheel(up, col, row, mods, self.modes())
    }

    fn encode_click(&mut self, button: u8, col: u16, row: u16, mods: u8) -> Option<Vec<u8>> {
        crate::keys::encode_click(button, col, row, mods, self.modes())
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn end_sync(&mut self, events: &mut dyn Events) {
        self.parser.stop_sync(&mut self.term);
        self.drain(events);
    }
}

/// `MODE_*` bits of alacritty's terminal mode.
pub fn modes(mode: TermMode) -> u32 {
    let mut bits = 0;
    for (flag, bit) in [
        (TermMode::MOUSE_REPORT_CLICK, MODE_MOUSE),
        (TermMode::MOUSE_DRAG, MODE_MOUSE | MODE_MOUSE_DRAG),
        (TermMode::MOUSE_MOTION, MODE_MOUSE | MODE_MOUSE_MOTION),
        (TermMode::SGR_MOUSE, MODE_SGR_MOUSE),
        (TermMode::UTF8_MOUSE, MODE_UTF8_MOUSE),
        (TermMode::BRACKETED_PASTE, MODE_BRACKETED_PASTE),
        (TermMode::ALT_SCREEN, MODE_ALT_SCREEN),
        (TermMode::APP_CURSOR, MODE_APP_CURSOR),
        (TermMode::FOCUS_IN_OUT, MODE_FOCUS),
        (TermMode::SHOW_CURSOR, MODE_SHOW_CURSOR),
        (TermMode::APP_KEYPAD, MODE_APP_KEYPAD),
        (TermMode::DISAMBIGUATE_ESC_CODES, 1 << MODE_KITTY_SHIFT),
        (TermMode::REPORT_EVENT_TYPES, 2 << MODE_KITTY_SHIFT),
        (TermMode::REPORT_ALTERNATE_KEYS, 4 << MODE_KITTY_SHIFT),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8 << MODE_KITTY_SHIFT),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16 << MODE_KITTY_SHIFT),
    ] {
        if mode.contains(flag) {
            bits |= bit;
        }
    }
    bits
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::feed_collect;

    fn emulator(cols: u16, rows: u16) -> AlacrittyEmulator {
        AlacrittyEmulator::new(&Config { cols, rows, scrollback: 1000, kitty_keyboard: false })
    }

    #[test]
    fn snapshot_preserves_colors_wide_chars_and_cursor() {
        let mut emu = emulator(20, 3);
        feed_collect(&mut emu, "\x1b[31mred\x1b[0m 中!\r\n\x1b[48;2;1;2;3mbg\x1b[0m".as_bytes());
        let snap = emu.snapshot();
        let first = snap.row_v7(0);
        assert_eq!(first[0], Run { col: 0, text: "red".into(), fg: 2, bg: 0, flags: 0, link: None });
        assert_eq!(first[1].col, 3);
        assert_eq!(first[1].text, " 中!");
        assert_eq!(snap.row_v7(1)[0].bg, COLOR_RGB | 0x010203);
        assert_eq!(snap.cursor, Some((2, 1)));
        assert!(snap.lines[2].is_empty(), "blank rows carry no runs");
        // Protocol 8 keeps the engine's widths: the wide character is its own run.
        let wide: Vec<_> = snap.lines[0].iter().filter(|r| r.wide).collect();
        assert_eq!((wide.len(), wide[0].text.as_str(), wide[0].col, wide[0].width()), (1, "中", 4, 2));
    }

    #[test]
    fn grapheme_clusters_and_underline_styles_reach_protocol_8() {
        let mut emu = emulator(20, 2);
        feed_collect(&mut emu, "e\u{301}x \x1b[4:3mcurly\x1b[0m".as_bytes());
        let snap = emu.snapshot();
        let row = &snap.lines[0];
        assert_eq!(row[0].cells()[..2], ["e\u{301}", "x"], "combining mark stays in its cell");
        let curly = row.iter().find(|r| r.text == "curly").expect("styled run");
        assert_eq!((curly.flags & F8_UL_MASK) >> F8_UL_SHIFT, UL_CURLY);
        assert_ne!(curly.flags & F_UNDERLINE as u32, 0);
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
        let mut emu = emulator(40, 2);
        feed_collect(&mut emu, b"see \x1b]8;;https://github.com/pr/1\x1b\\PR 1\x1b]8;;\x1b\\ now");
        let snap = emu.snapshot();
        let line = snap.row_v7(0);
        let linked: Vec<_> = line.iter().filter(|r| r.link.is_some()).collect();
        assert_eq!(linked.len(), 1);
        assert_eq!((linked[0].text.as_str(), linked[0].link.as_deref()), ("PR 1", Some("https://github.com/pr/1")));
        assert!(line.last().unwrap().text.ends_with("now"));
        assert_eq!(snap.links, vec!["https://github.com/pr/1".to_owned()]);
    }

    #[test]
    fn answers_cursor_position_queries_required_by_conpty() {
        let mut emu = emulator(20, 5);
        let events = feed_collect(&mut emu, b"ab\x1b[6n");
        assert_eq!(events.replies, vec![b"\x1b[1;3R".to_vec()]);
        let events = feed_collect(&mut emu, b"\x1b]11;?\x07");
        let reply = String::from_utf8(events.replies[0].clone()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:"), "background color query answered: {reply:?}");
    }

    #[test]
    fn titles_bells_and_progress_are_events() {
        let mut emu = emulator(20, 5);
        let events = feed_collect(&mut emu, b"\x1b]0;hello\x07\x07\x1b]9;4;3;0\x07\x1b]777;notify;a;b\x07");
        assert_eq!(events.title, Some(Some("hello".into())));
        assert_eq!((events.bells, events.progress.as_slice(), events.notifications), (1, &[3u8][..], 1));
    }

    #[test]
    fn hidden_cursor_and_scrollback_are_reflected() {
        let mut emu = emulator(10, 3);
        for i in 0..10 {
            feed_collect(&mut emu, format!("line{i}\r\n").as_bytes());
        }
        feed_collect(&mut emu, b"\x1b[?25l");
        assert_eq!(emu.snapshot().cursor, None);
        emu.scroll(Scroll::Delta(3));
        let snap = emu.snapshot();
        assert_eq!(snap.display_offset, 3);
        assert!(snap.history >= 3);
        assert_eq!(snap.lines[0][0].text, "line5");
    }

    #[test]
    fn modes_follow_the_program() {
        let mut emu = emulator(10, 3);
        feed_collect(&mut emu, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h");
        let m = emu.modes();
        for bit in [MODE_ALT_SCREEN, MODE_MOUSE, MODE_SGR_MOUSE, MODE_BRACKETED_PASTE, MODE_APP_CURSOR] {
            assert_ne!(m & bit, 0, "mode bit {bit:#x}");
        }
    }

    #[test]
    fn palette_matches_xterm_cube() {
        assert_eq!(palette(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(palette(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(palette(232), Rgb { r: 8, g: 8, b: 8 });
    }
}
