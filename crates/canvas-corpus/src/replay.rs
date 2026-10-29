//! Replays a recording into an engine and captures what a client would see,
//! writes and reads expected files, and compares them cell by cell.

use std::fmt::Write as _;

use anyhow::{Result, ensure};
use canvas_common::protocol::*;
use canvas_host::emulator::{self, Config, Emulator, Engine, Events, Snapshot};
use serde::{Deserialize, Serialize};

use crate::vtrec::{Kind, Recording, clamp_size};

/// Hosted terminals' scrollback (`term.rs`).
pub const SCROLLBACK: usize = 5000;
/// A synchronized update (DEC 2026) still open this long (virtual time) is
/// flushed before the next event, as the host's emulator thread does.
pub const SYNC_FLUSH_MS: u64 = 150;
/// The mark of the screen after the whole stream.
pub const END: &str = "end";

/// The emulator configuration of hosted terminals.
pub fn config(cols: u16, rows: u16) -> Config {
    Config { cols, rows, scrollback: SCROLLBACK, kitty_keyboard: true }
}

/// A fresh emulator; fails instead of silently falling back to another engine.
pub fn new_emulator(engine: Engine, cols: u16, rows: u16) -> Result<Box<dyn Emulator>> {
    ensure!(engine.available(), "the {} engine is not in this build", engine.name());
    let emulator = emulator::create(engine, &config(cols, rows));
    ensure!(emulator.engine() == engine, "the {} engine failed to start", engine.name());
    Ok(emulator)
}

/// Everything the engine reported while the stream was fed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct EventLog {
    /// Title changes (OSC 0/1/2 and resets).
    pub titles: u32,
    /// The title at the end (`None`: never set, or reset).
    pub title: Option<String>,
    pub bells: u32,
    /// OSC 52 clipboard writes.
    pub clipboard: Vec<String>,
    /// OSC 9;4 progress states in order.
    pub progress: Vec<u8>,
    pub notifications: u32,
    /// Answers to the program's queries (cursor reports, colours, device attributes).
    pub replies: Vec<String>,
}

impl Events for EventLog {
    fn reply(&mut self, bytes: Vec<u8>) {
        self.replies.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    fn title(&mut self, title: Option<String>) {
        self.titles += 1;
        self.title = title;
    }
    fn bell(&mut self) {
        self.bells += 1;
    }
    fn clipboard(&mut self, text: String) {
        self.clipboard.push(text);
    }
    fn progress(&mut self, state: u8) {
        self.progress.push(state);
    }
    fn notify(&mut self) {
        self.notifications += 1;
    }
}

/// The viewport at a mark (or at the end).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Screen {
    pub mark: String,
    pub cols: u16,
    pub rows: u16,
    pub cursor: Option<(u16, u16)>,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    pub display_offset: u32,
    pub history: u32,
    pub modes: u32,
    /// Derived from `modes` for reviewers; not compared.
    #[serde(default)]
    pub mode_names: Vec<String>,
    pub links: Vec<String>,
    /// Derived from `lines` for reviewers; not compared.
    #[serde(default)]
    pub text: Vec<String>,
    pub lines: Vec<Vec<Run8>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Expected {
    pub engine: String,
    pub events: EventLog,
    /// One per mark in stream order, then [`END`].
    pub screens: Vec<Screen>,
}

/// Plain text of one row (cells at their columns, trailing blanks trimmed).
pub fn row_text(row: &[Run8]) -> String {
    let mut line = String::new();
    let mut col = 0u16;
    for run in row {
        if run.col > col {
            line.push_str(&" ".repeat((run.col - col) as usize));
        }
        line.push_str(&run.text);
        col = run.col + run.width();
    }
    line.trim_end_matches(' ').to_owned()
}

/// Names of `MODE_*` bits.
pub fn mode_names(modes: u32) -> Vec<String> {
    const NAMES: [(u32, &str); 11] = [
        (MODE_MOUSE, "mouse"),
        (MODE_MOUSE_DRAG, "mouse_drag"),
        (MODE_MOUSE_MOTION, "mouse_motion"),
        (MODE_SGR_MOUSE, "sgr_mouse"),
        (MODE_UTF8_MOUSE, "utf8_mouse"),
        (MODE_BRACKETED_PASTE, "bracketed_paste"),
        (MODE_ALT_SCREEN, "alt_screen"),
        (MODE_APP_CURSOR, "app_cursor"),
        (MODE_FOCUS, "focus"),
        (MODE_SHOW_CURSOR, "show_cursor"),
        (MODE_APP_KEYPAD, "app_keypad"),
    ];
    let mut names: Vec<String> = NAMES.iter().filter(|(bit, _)| modes & bit != 0).map(|(_, name)| (*name).to_owned()).collect();
    let kitty = (modes >> MODE_KITTY_SHIFT) & 0x1f;
    if kitty != 0 {
        names.push(format!("kitty_keyboard={kitty}"));
    }
    let known = NAMES.iter().fold(0x1f << MODE_KITTY_SHIFT, |acc, (bit, _)| acc | bit);
    for bit in 0..32 {
        if modes & !known & (1 << bit) != 0 {
            names.push(format!("bit{bit}"));
        }
    }
    names
}

impl Screen {
    pub fn capture(mark: &str, snapshot: &Snapshot) -> Self {
        Self {
            mark: mark.to_owned(),
            cols: snapshot.cols,
            rows: snapshot.rows,
            cursor: snapshot.cursor,
            cursor_shape: snapshot.cursor_shape,
            cursor_blink: snapshot.cursor_blink,
            display_offset: snapshot.display_offset,
            history: snapshot.history,
            modes: snapshot.modes,
            mode_names: mode_names(snapshot.modes),
            links: snapshot.links.clone(),
            text: snapshot.lines.iter().map(|row| row_text(row)).collect(),
            lines: snapshot.lines.clone(),
        }
    }
}

/// Feeds the recording into a fresh `engine`, respecting chunk boundaries,
/// resizes and (for synchronized updates) virtual time.
pub fn replay(engine: Engine, recording: &Recording) -> Result<Expected> {
    // Recordings built in code skip the parser's clamp.
    let (cols, rows) = clamp_size(recording.header.cols, recording.header.rows);
    let mut emu = new_emulator(engine, cols, rows)?;
    let mut events = EventLog::default();
    let mut screens = Vec::new();
    let mut clock = 0u64;
    let mut sync_since: Option<u64> = None;
    for event in &recording.events {
        clock = clock.saturating_add(event.delta_ms);
        if sync_since.is_some_and(|since| clock.saturating_sub(since) >= SYNC_FLUSH_MS) && emu.sync_deadline().is_some() {
            emu.end_sync(&mut events);
            sync_since = None;
        }
        match &event.kind {
            Kind::Output(bytes) => {
                emu.feed(bytes, &mut events);
                sync_since = emu.sync_deadline().map(|_| sync_since.unwrap_or(clock));
            }
            Kind::Resize(cols, rows) => {
                let (cols, rows) = clamp_size(*cols, *rows);
                emu.resize(cols, rows);
            }
            Kind::Input(_) => {}
            Kind::Mark(name) => screens.push(Screen::capture(name, &emu.snapshot())),
        }
    }
    if emu.sync_deadline().is_some() {
        emu.end_sync(&mut events);
    }
    screens.push(Screen::capture(END, &emu.snapshot()));
    Ok(Expected { engine: engine.name().to_owned(), events, screens })
}

fn json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("serializable")
}

/// `[` items one per line `]` at `indent`.
fn list(out: &mut String, items: impl IntoIterator<Item = String>, indent: &str) {
    let items: Vec<String> = items.into_iter().collect();
    if items.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push_str("[\n");
    for (i, item) in items.iter().enumerate() {
        let comma = if i + 1 < items.len() { "," } else { "" };
        let _ = writeln!(out, "{indent}  {item}{comma}");
    }
    out.push_str(indent);
    out.push(']');
}

impl Expected {
    /// Pretty JSON with stable field order; one row per line so diffs stay readable.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let _ = writeln!(out, "  \"engine\": {},", json(&self.engine));
        let events = serde_json::to_string_pretty(&self.events).expect("serializable").replace('\n', "\n  ");
        let _ = writeln!(out, "  \"events\": {events},");
        out.push_str("  \"screens\": [\n");
        for (i, s) in self.screens.iter().enumerate() {
            out.push_str("    {\n");
            for (key, value) in [
                ("mark", json(&s.mark)),
                ("cols", json(&s.cols)),
                ("rows", json(&s.rows)),
                ("cursor", json(&s.cursor)),
                ("cursor_shape", json(&s.cursor_shape)),
                ("cursor_blink", json(&s.cursor_blink)),
                ("display_offset", json(&s.display_offset)),
                ("history", json(&s.history)),
                ("modes", json(&s.modes)),
                ("mode_names", json(&s.mode_names)),
            ] {
                let _ = writeln!(out, "      \"{key}\": {value},");
            }
            out.push_str("      \"links\": ");
            list(&mut out, s.links.iter().map(json), "      ");
            out.push_str(",\n      \"text\": ");
            list(&mut out, s.text.iter().map(json), "      ");
            out.push_str(",\n      \"lines\": ");
            list(&mut out, s.lines.iter().map(json), "      ");
            out.push_str(if i + 1 < self.screens.len() { "\n    },\n" } else { "\n    }\n" });
        }
        out.push_str("  ]\n}\n");
        out
    }

    pub fn from_json(text: &str) -> Result<Self> {
        Ok(serde_json::from_str(text)?)
    }
}

/// One grid cell, resolved for comparison.
#[derive(Clone, Debug, PartialEq)]
struct Cell<'a> {
    text: &'a str,
    wide: bool,
    fg: u32,
    bg: u32,
    flags: u32,
    underline: u32,
    link: Option<&'a str>,
}

const BLANK: Cell<'static> = Cell { text: " ", wide: false, fg: 0, bg: 0, flags: 0, underline: 0, link: None };

/// Full rows × cols grid; `None` is the spacer right of a wide cell.
fn grid(screen: &Screen) -> Vec<Vec<Option<Cell<'_>>>> {
    let cols = screen.cols as usize;
    let mut rows = Vec::with_capacity(screen.lines.len());
    for line in &screen.lines {
        let mut row: Vec<Option<Cell>> = vec![Some(BLANK); cols];
        for run in line {
            let link = (run.link > 0).then(|| screen.links.get(run.link as usize - 1).map(String::as_str)).flatten();
            let step = if run.wide { 2 } else { 1 };
            for (i, text) in run.cells().into_iter().enumerate() {
                let col = run.col as usize + i * step;
                if row.len() < col + step {
                    row.resize(col + step, Some(BLANK));
                }
                row[col] = Some(Cell { text, wide: run.wide, fg: run.fg, bg: run.bg, flags: run.flags, underline: run.underline, link });
                if run.wide {
                    row[col + 1] = None;
                }
            }
        }
        rows.push(row);
    }
    rows
}

fn color(value: u32) -> String {
    if value == COLOR_DEFAULT {
        "default".into()
    } else if value & COLOR_RGB != 0 {
        format!("#{:06x}", value & 0xff_ffff)
    } else {
        format!("idx{}", value - 1)
    }
}

fn describe(cell: &Option<Cell>) -> String {
    let Some(cell) = cell else { return "(wide spacer)".into() };
    let mut out = format!("{:?}", cell.text);
    if cell.wide {
        out.push_str(" wide");
    }
    if cell.fg != 0 {
        let _ = write!(out, " fg={}", color(cell.fg));
    }
    if cell.bg != 0 {
        let _ = write!(out, " bg={}", color(cell.bg));
    }
    for (bit, name) in [
        (F_BOLD as u32, "bold"),
        (F_ITALIC as u32, "italic"),
        (F_INVERSE as u32, "inverse"),
        (F_DIM as u32, "dim"),
        (F_HIDDEN as u32, "hidden"),
        (F_STRIKE as u32, "strike"),
        (F8_OVERLINE, "overline"),
        (F8_BLINK, "blink"),
    ] {
        if cell.flags & bit != 0 {
            let _ = write!(out, " {name}");
        }
    }
    if cell.flags & F_UNDERLINE as u32 != 0 {
        let style = match (cell.flags & F8_UL_MASK) >> F8_UL_SHIFT {
            UL_DOUBLE => "double",
            UL_CURLY => "curly",
            UL_DOTTED => "dotted",
            UL_DASHED => "dashed",
            _ => "single",
        };
        let _ = write!(out, " underline={style}");
    }
    if cell.underline != 0 {
        let _ = write!(out, " ul_color={}", color(cell.underline));
    }
    if let Some(link) = cell.link {
        let _ = write!(out, " link={link}");
    }
    out
}

/// Cell differences shown per screen before summarising.
const MAX_CELL_DIFFS: usize = 12;
/// Rows whose full text is shown per screen.
const MAX_ROW_DIFFS: usize = 6;

fn compare_screen(expected: &Screen, actual: &Screen, out: &mut Vec<String>) {
    let at = format!("[{}]", expected.mark);
    if expected.mark != actual.mark {
        out.push(format!("{at} mark: expected {:?}, actual {:?}", expected.mark, actual.mark));
    }
    let mut field = |name: &str, e: String, a: String| {
        if e != a {
            out.push(format!("{at} {name}: expected {e}, actual {a}"));
        }
    };
    field("size", format!("{}x{}", expected.cols, expected.rows), format!("{}x{}", actual.cols, actual.rows));
    field("cursor", format!("{:?}", expected.cursor), format!("{:?}", actual.cursor));
    field("cursor_shape", expected.cursor_shape.to_string(), actual.cursor_shape.to_string());
    field("cursor_blink", expected.cursor_blink.to_string(), actual.cursor_blink.to_string());
    field("display_offset", expected.display_offset.to_string(), actual.display_offset.to_string());
    field("history", expected.history.to_string(), actual.history.to_string());
    field(
        "modes",
        format!("{:#x} {:?}", expected.modes, mode_names(expected.modes)),
        format!("{:#x} {:?}", actual.modes, mode_names(actual.modes)),
    );
    let (eg, ag) = (grid(expected), grid(actual));
    let mut cells = 0usize;
    let mut rows = 0usize;
    let mut row_texts = Vec::new();
    for y in 0..eg.len().max(ag.len()) {
        let (er, ar) = (eg.get(y), ag.get(y));
        let width = er.map_or(0, Vec::len).max(ar.map_or(0, Vec::len));
        let mut row_differs = false;
        for x in 0..width {
            let e = er.and_then(|r| r.get(x)).cloned().unwrap_or(Some(BLANK));
            let a = ar.and_then(|r| r.get(x)).cloned().unwrap_or(Some(BLANK));
            if e != a {
                if cells < MAX_CELL_DIFFS {
                    out.push(format!("{at} row {y} col {x}: expected {} | actual {}", describe(&e), describe(&a)));
                }
                cells += 1;
                row_differs = true;
            }
        }
        if row_differs {
            if rows < MAX_ROW_DIFFS {
                let text = |s: &Screen| s.lines.get(y).map(|r| row_text(r)).unwrap_or_default();
                row_texts.push(format!("{at} row {y}:\n      - {:?}\n      + {:?}", text(expected), text(actual)));
            }
            rows += 1;
        }
    }
    if cells > 0 {
        out.push(format!("{at} {cells} cell(s) differ in {rows} row(s)"));
        out.extend(row_texts);
    }
    // Soft wraps (the marker run carries no cells).
    let wraps = |s: &Screen| s.lines.iter().map(|row| row_soft_wrap(row)).collect::<Vec<_>>();
    let (ew, aw) = (wraps(expected), wraps(actual));
    let describe_wrap = |w: Option<u16>| w.map_or("ends its line".to_owned(), |end| format!("wraps after column {end}"));
    for y in 0..ew.len().max(aw.len()) {
        let (e, a) = (ew.get(y).copied().flatten(), aw.get(y).copied().flatten());
        if e != a {
            out.push(format!("{at} row {y}: expected it {} | actual {}", describe_wrap(e), describe_wrap(a)));
        }
    }
}

/// Human-readable differences; empty when `actual` matches `expected`.
pub fn compare(expected: &Expected, actual: &Expected) -> Vec<String> {
    let mut out = Vec::new();
    let (e, a) = (&expected.events, &actual.events);
    let mut field = |name: &str, ev: String, av: String| {
        if ev != av {
            out.push(format!("events.{name}: expected {ev}, actual {av}"));
        }
    };
    field("titles", e.titles.to_string(), a.titles.to_string());
    field("title", format!("{:?}", e.title), format!("{:?}", a.title));
    field("bells", e.bells.to_string(), a.bells.to_string());
    field("clipboard", format!("{:?}", e.clipboard), format!("{:?}", a.clipboard));
    field("progress", format!("{:?}", e.progress), format!("{:?}", a.progress));
    field("notifications", e.notifications.to_string(), a.notifications.to_string());
    field("replies", format!("{:?}", e.replies), format!("{:?}", a.replies));
    if expected.screens.len() != actual.screens.len() {
        let marks = |x: &Expected| x.screens.iter().map(|s| s.mark.clone()).collect::<Vec<_>>();
        out.push(format!("screens: expected {:?}, actual {:?}", marks(expected), marks(actual)));
    }
    for (e, a) in expected.screens.iter().zip(&actual.screens) {
        compare_screen(e, a, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtrec::{Event, Header};

    fn recording(chunks: &[&[u8]]) -> Recording {
        let header = Header { vtrec: 1, cols: 20, rows: 4, program: String::new(), description: String::new(), source: "synthetic".into(), recorded_at: String::new() };
        let mut rec = Recording::new(header);
        rec.events = chunks.iter().map(|c| Event::output(1, c.to_vec())).collect();
        rec
    }

    #[test]
    fn expected_json_round_trips_and_is_stable() {
        let rec = recording(&[b"\x1b]0;t\x07\x1b[1;31mred\x1b[0m \x1b]8;;https://x\x1b\\ln\x1b]8;;\x1b\\ \xe4\xb8\xad\x07", b"\r\nnext"]);
        let expected = replay(Engine::Alacritty, &rec).unwrap();
        let text = expected.to_json();
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok(), "{text}");
        let back = Expected::from_json(&text).unwrap();
        assert_eq!(back, expected);
        assert_eq!(back.to_json(), text);
        assert_eq!((expected.events.titles, expected.events.bells), (1, 1));
        assert_eq!(expected.screens[0].text[0], "red ln 中");
        assert!(compare(&expected, &back).is_empty());
    }

    #[test]
    fn compare_reports_cells_and_events() {
        let expected = replay(Engine::Alacritty, &recording(&[b"abc\r\nxyz"])).unwrap();
        let actual = replay(Engine::Alacritty, &recording(&[b"abd\x07\r\n\x1b[1mxyz"])).unwrap();
        let diff = compare(&expected, &actual).join("\n");
        assert!(diff.contains("events.bells: expected 0, actual 1"), "{diff}");
        assert!(diff.contains("[end] row 0 col 2: expected \"c\" | actual \"d\""), "{diff}");
        assert!(diff.contains("[end] row 1 col 0: expected \"x\" | actual \"x\" bold"), "{diff}");
        assert!(diff.contains("4 cell(s) differ in 2 row(s)"), "{diff}");
    }

    #[test]
    fn compare_reports_soft_wraps() {
        let expected = replay(Engine::Alacritty, &recording(&[b"0123456789abcdefghijklmnop"])).unwrap();
        let actual = replay(Engine::Alacritty, &recording(&[b"0123456789abcdefghij\r\nklmnop"])).unwrap();
        assert_eq!(expected.screens[0].text, actual.screens[0].text, "the same cells");
        let diff = compare(&expected, &actual);
        assert_eq!(diff, ["[end] row 0: expected it wraps after column 20 | actual ends its line"]);
        let text = expected.to_json();
        assert!(text.contains("\"flags\":4096"), "the marker is stored: {text}");
        assert_eq!(Expected::from_json(&text).unwrap(), expected);
    }

    #[test]
    fn huge_sizes_and_times_in_built_recordings_are_bounded() {
        let mut rec = recording(&[b"x"]);
        (rec.header.cols, rec.header.rows) = (60_000, 60_000);
        rec.events.push(Event { delta_ms: u64::MAX, kind: Kind::Resize(65_535, 65_535) });
        rec.events.push(Event::output(u64::MAX, b"\x1b[?2026hy".to_vec()));
        let expected = replay(Engine::Alacritty, &rec).unwrap();
        let end = expected.screens.last().unwrap();
        assert_eq!((end.cols, end.rows), (crate::vtrec::MAX_COLS, crate::vtrec::MAX_ROWS));
    }

    #[test]
    fn marks_capture_screens_and_open_sync_updates_flush() {
        let mut rec = recording(&[b"one"]);
        rec.events.push(Event { delta_ms: 0, kind: Kind::Mark("first".into()) });
        rec.events.push(Event::output(1, b"\x1b[?2026h\r\ntwo".to_vec()));
        rec.events.push(Event { delta_ms: 0, kind: Kind::Mark("inside".into()) });
        rec.events.push(Event::output(SYNC_FLUSH_MS, b"!".to_vec()));
        rec.events.push(Event { delta_ms: 0, kind: Kind::Mark("flushed".into()) });
        let expected = replay(Engine::Alacritty, &rec).unwrap();
        let marks: Vec<_> = expected.screens.iter().map(|s| s.mark.as_str()).collect();
        assert_eq!(marks, ["first", "inside", "flushed", END]);
        assert_eq!(expected.screens[1].text[1], "", "alacritty holds a synchronized update back");
        assert_eq!(expected.screens[2].text[1], "two!");
    }

    #[test]
    fn mode_names_cover_unknown_bits() {
        assert_eq!(mode_names(MODE_ALT_SCREEN | MODE_KITTY_DISAMBIGUATE | 1 << 30), ["alt_screen", "kitty_keyboard=1", "bit30"]);
    }
}
