//! The client-side model of one hosted terminal, built from L1 frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use canvas_common::protocol::{F_ITALIC, Frame, Frame8, Placement, Run, Run8, runs_v7};

/// Hyperlink targets kept before the table is rebuilt from the rows.
const MAX_LINKS: usize = 1024;

#[derive(Default, Debug)]
pub struct TermView {
    pub cols: u16,
    pub rows: u16,
    /// Protocol-7 rows (what the TUI draws), also derived from protocol-8 frames.
    pub lines: Vec<Vec<Run>>,
    pub cursor: Option<(u16, u16)>,
    pub display_offset: u32,
    pub history: u32,
    /// Protocol-8 rows (empty when the service sends protocol-7 frames);
    /// `Run8::link` indexes `links`.
    pub rows8: Vec<Vec<Run8>>,
    pub links: Vec<String>,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    /// `MODE_*` bits.
    pub modes: u32,
    /// Kitty graphics placements in the viewport (`ServiceMsg::Placements`).
    pub placements: Vec<Placement>,
    /// The images the placements show, by image id.
    pub images: HashMap<u32, TermImage>,
}

/// One image of a terminal (`ServiceMsg::Image`).
#[derive(Clone)]
pub struct TermImage {
    pub width: u32,
    pub height: u32,
    /// `IMAGE_PNG` or `IMAGE_RGBA` (front ends may decode PNG before it arrives here).
    pub format: u8,
    pub data: Arc<Vec<u8>>,
    /// Unique per received image (texture cache key).
    pub serial: u64,
}

impl std::fmt::Debug for TermImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TermImage({}x{}, format {}, {} bytes, #{})", self.width, self.height, self.format, self.data.len(), self.serial)
    }
}

static IMAGE_SERIAL: AtomicU64 = AtomicU64::new(1);

impl TermView {
    pub fn apply(&mut self, frame: Frame) {
        if frame.full || frame.rows != self.rows || frame.cols != self.cols {
            self.lines = vec![Vec::new(); frame.rows as usize];
        }
        self.rows8.clear();
        self.links.clear();
        self.cols = frame.cols;
        self.rows = frame.rows;
        for (y, runs) in frame.changed {
            if let Some(line) = self.lines.get_mut(y as usize) {
                *line = runs;
            }
        }
        self.cursor = frame.cursor;
        self.display_offset = frame.display_offset;
        self.history = frame.history;
    }

    /// Applies a protocol-8 frame; `lines` gets the same protocol-7 rows a
    /// protocol-7 frame of this screen would carry.
    pub fn apply8(&mut self, frame: Frame8) {
        let resized = frame.rows != self.rows || frame.cols != self.cols || self.rows8.len() != frame.rows as usize;
        if frame.full || resized {
            self.lines = vec![Vec::new(); frame.rows as usize];
            self.rows8 = vec![Vec::new(); frame.rows as usize];
            self.links.clear();
        }
        self.cols = frame.cols;
        self.rows = frame.rows;
        for (y, mut row) in frame.changed {
            if let Some(line) = self.lines.get_mut(y as usize) {
                *line = runs_v7(&row, &frame.links);
            }
            for run in row.iter_mut().filter(|r| r.link > 0) {
                run.link = match frame.links.get(run.link as usize - 1) {
                    Some(target) => intern(&mut self.links, target),
                    None => 0,
                };
            }
            if let Some(slot) = self.rows8.get_mut(y as usize) {
                *slot = row;
            }
        }
        if self.links.len() > MAX_LINKS {
            self.compact_links();
        }
        self.cursor = frame.cursor;
        self.display_offset = frame.display_offset;
        self.history = frame.history;
        self.cursor_shape = frame.cursor_shape;
        self.cursor_blink = frame.cursor_blink;
        self.modes = frame.modes;
    }

    /// An image arrived (it replaces one with the same id).
    pub fn set_image(&mut self, id: u32, width: u32, height: u32, format: u8, data: Vec<u8>) {
        let serial = IMAGE_SERIAL.fetch_add(1, Ordering::Relaxed);
        self.images.insert(id, TermImage { width, height, format, data: Arc::new(data), serial });
    }

    /// The viewport's placements; images no placement shows are forgotten
    /// (the service sends them again before they are placed again).
    pub fn set_placements(&mut self, placements: Vec<Placement>) {
        self.images.retain(|id, _| placements.iter().any(|p| p.image_id == *id));
        self.placements = placements;
    }

    /// Drops link targets no row references any more.
    fn compact_links(&mut self) {
        let old = std::mem::take(&mut self.links);
        for run in self.rows8.iter_mut().flatten().filter(|r| r.link > 0) {
            run.link = old.get(run.link as usize - 1).map(|target| intern(&mut self.links, target)).unwrap_or(0);
        }
    }

    /// One string per cell of a viewport row; the second cell of a wide
    /// character is empty.
    pub fn row_cells(&self, row: usize) -> Vec<String> {
        let mut cells = vec![" ".to_owned(); self.cols as usize];
        for run in self.lines.get(row).map(Vec::as_slice).unwrap_or_default() {
            let mut col = run.col as usize;
            for ch in run.text.chars() {
                let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if width == 0 {
                    if let Some(cell) = col.checked_sub(1).and_then(|c| cells.get_mut(c)) {
                        cell.push(ch);
                    }
                    continue;
                }
                if let Some(cell) = cells.get_mut(col) {
                    *cell = ch.to_string();
                }
                if width == 2
                    && let Some(cell) = cells.get_mut(col + 1)
                {
                    cell.clear();
                }
                col += width;
            }
        }
        cells
    }

    /// Selected text; rows that fill the width are soft-wrapped and rejoin.
    pub fn selection_text(&self, selection: &TermSelection) -> String {
        let ((c0, r0), (c1, r1)) = selection.ordered();
        let mut out = String::new();
        for row in r0..=r1 {
            let cells = self.row_cells(row as usize);
            let Some(last) = cells.len().checked_sub(1) else { continue };
            let start = if row == r0 { c0 as usize } else { 0 };
            let end = if row == r1 { (c1 as usize).min(last) } else { last };
            let text: String = if start <= end { cells[start..=end].concat() } else { String::new() };
            let wrapped = row < r1 && !cells[last].trim().is_empty();
            out.push_str(if wrapped { &text } else { text.trim_end() });
            if row < r1 && !wrapped {
                out.push_str("\r\n");
            }
        }
        out
    }
}

/// Index + 1 of `target` in `table`, adding it when new.
fn intern(table: &mut Vec<String>, target: &str) -> u32 {
    match table.iter().position(|t| t == target) {
        Some(index) => index as u32 + 1,
        None => {
            table.push(target.to_owned());
            table.len() as u32
        }
    }
}

/// Rows of a Copilot viewport that show your own messages. Copilot draws them
/// like its output: " ❯ text" (non-italic; " ❯ Thought" is italic reasoning),
/// continued by lines indented three spaces, possibly across blank lines,
/// until its next item (" ● ", " $ ", …).
pub fn user_message_rows(lines: &[Vec<Run>]) -> Vec<bool> {
    let text_of = |runs: &[Run]| -> String {
        let mut text = String::new();
        for run in runs {
            let width = unicode_width::UnicodeWidthStr::width(text.as_str());
            if (run.col as usize) > width {
                text.push_str(&" ".repeat(run.col as usize - width));
            }
            text.push_str(&run.text);
        }
        // Copilot's scrollbar sits in the last column.
        text.trim_end_matches([' ', '┃', '│']).to_owned()
    };
    let starts = |runs: &[Run], text: &str| {
        text.starts_with(" ❯ ")
            && text.chars().nth(3).is_some_and(|c| !c.is_whitespace())
            && !runs.iter().any(|r| r.col <= 3 && r.col as usize + r.text.chars().count() > 3 && r.flags & F_ITALIC != 0)
    };
    let mut marked = vec![false; lines.len()];
    let mut in_block = false;
    let mut blanks: Vec<usize> = Vec::new();
    for (y, runs) in lines.iter().enumerate() {
        let text = text_of(runs);
        if starts(runs, &text) {
            in_block = true;
            blanks.clear();
            marked[y] = true;
        } else if in_block && text.trim().is_empty() {
            blanks.push(y);
        } else if in_block && text.starts_with("   ") && text.chars().nth(3).is_some_and(|c| !c.is_whitespace()) {
            for b in blanks.drain(..) {
                marked[b] = true;
            }
            marked[y] = true;
        } else {
            in_block = false;
            blanks.clear();
        }
    }
    marked
}

/// Text selected in a terminal pane (viewport cell coordinates, inclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct TermSelection {
    pub id: String,
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl TermSelection {
    /// (start, end) in reading order.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (a, b) = (self.anchor, self.head);
        if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) }
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        let ((c0, r0), (c1, r1)) = self.ordered();
        (row > r0 || (row == r0 && col >= c0)) && (row < r1 || (row == r1 && col <= c1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(col: u16, text: &str) -> Run {
        Run { col, text: text.into(), fg: 0, bg: 0, flags: 0, link: None }
    }

    #[test]
    fn finds_user_messages_but_not_reasoning_or_the_input_box() {
        let italic = |col: u16, text: &str| Run { col, text: text.into(), fg: 0, bg: 0, flags: F_ITALIC, link: None };
        let lines = vec![
            vec![run(0, " ❯ please fix the build"), run(119, "┃")],
            vec![run(0, "   and run the tests")],
            vec![run(119, "┃")],
            vec![run(0, "   second paragraph")],
            vec![],
            vec![run(0, " ● On it."), run(119, "┃")],
            vec![run(0, "   continuing reply")],
            vec![run(0, " ❯ "), italic(3, "Thought")],
            vec![run(0, "   not mine")],
            vec![run(0, "❯ typing in the input box")],
        ];
        let marked = user_message_rows(&lines);
        assert_eq!(marked, [true, true, true, true, false, false, false, false, false, false]);
    }

    #[test]
    fn protocol8_frames_keep_the_protocol7_view_and_a_consistent_link_table() {
        use canvas_common::protocol::Frame8;
        let run8 = |col: u16, text: &str, link: u32| Run8 { col, text: text.into(), link, ..Default::default() };
        let frame = |full: bool, changed: Vec<(u16, Vec<Run8>)>, links: &[&str]| Frame8 {
            id: "t".into(),
            cols: 20,
            rows: 3,
            full,
            changed,
            cursor: Some((1, 2)),
            cursor_shape: 2,
            cursor_blink: true,
            display_offset: 0,
            history: 4,
            modes: 1,
            links: links.iter().map(|s| s.to_string()).collect(),
        };
        let mut view = TermView::default();
        let rows = vec![
            (0, vec![run8(0, "ab", 0), run8(2, "cd", 0)]),
            (1, vec![run8(0, "PR 1", 1)]),
            (2, vec![run8(3, "x", 2)]),
        ];
        let first = frame(true, rows.clone(), &["https://a", "https://b"]);
        let expected: Vec<Vec<Run>> = rows.iter().map(|(_, row)| runs_v7(row, &first.links)).collect();
        view.apply8(first);
        assert_eq!(view.lines, expected, "the protocol-7 view equals the service's protocol-7 rows");
        assert_eq!(view.lines[0], vec![run(0, "abcd")], "adjacent runs merge as protocol 7 did");
        assert_eq!((view.cursor_shape, view.cursor_blink, view.modes, view.history), (2, true, 1, 4));
        // A later frame with its own link numbering.
        view.apply8(frame(false, vec![(2, vec![run8(0, "y", 1)])], &["https://a"]));
        assert_eq!(view.links[view.rows8[2][0].link as usize - 1], "https://a");
        assert_eq!(view.links[view.rows8[1][0].link as usize - 1], "https://a");
        assert_eq!(view.lines[2][0].link.as_deref(), Some("https://a"));
        assert_eq!(view.lines[1][0].text, "PR 1");
    }

    #[test]
    fn images_live_as_long_as_a_placement_shows_them() {
        let mut view = TermView::default();
        view.set_image(1, 2, 1, 0, vec![0; 8]);
        view.set_image(2, 1, 1, 0, vec![0; 4]);
        let serial = view.images[&1].serial;
        view.set_placements(vec![Placement { image_id: 1, cols: 2, rows: 1, ..Default::default() }]);
        assert_eq!(view.images.keys().copied().collect::<Vec<_>>(), [1], "image 2 is not shown: forgotten");
        view.set_image(1, 2, 1, 0, vec![1; 8]);
        assert_ne!(view.images[&1].serial, serial, "replaced pixels get a new serial");
        view.set_placements(Vec::new());
        assert!(view.images.is_empty() && view.placements.is_empty());
    }

    #[test]
    fn selection_copies_reading_order_text_and_rejoins_wrapped_rows() {
        let view = TermView {
            cols: 8,
            rows: 4,
            lines: vec![vec![run(0, "hello 中")], vec![run(0, "abcdefgh")], vec![run(0, "ij")], vec![run(2, "tail")]],
            ..Default::default()
        };
        let sel = |a, b| TermSelection { id: "t".into(), anchor: a, head: b };
        assert_eq!(view.selection_text(&sel((0, 0), (4, 0))), "hello");
        assert_eq!(view.selection_text(&sel((6, 0), (7, 0))), "中", "wide character copied once");
        // Backwards drag; row 1 fills the width so it joins row 2 without a newline.
        assert_eq!(view.selection_text(&sel((1, 2), (3, 1))), "defghij");
        assert_eq!(view.selection_text(&sel((0, 2), (7, 3))), "ij\r\n  tail");
        assert!(sel((5, 1), (2, 2)).contains(0, 2) && !sel((5, 1), (2, 2)).contains(4, 1));
    }
}
