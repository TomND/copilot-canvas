//! The client-side model of one hosted terminal, built from L1 frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use canvas_common::protocol::{F_ITALIC, Frame, Frame8, MODE_ALT_SCREEN, Placement, Run, Run8, row_soft_wrap, runs_v7};

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

    /// Selected text, copied by the rules of [`TermView::selection_text_with`]
    /// without program-specific ones.
    pub fn selection_text(&self, selection: &TermSelection) -> String {
        self.selection_text_with(selection, CopyRules::default())
    }

    /// Selected text, line by line:
    ///
    /// - a soft-wrapped row (the engine's wrap state, [`row_soft_wrap`])
    ///   continues on the next row: the two rejoin without a break, keeping
    ///   the wrapped row's spaces up to where its text ends (a wide character
    ///   that moved on to the next row leaves no gap);
    /// - every other row ends a line: CR LF (the Windows clipboard's line
    ///   break), with its trailing blanks trimmed; blank lines, leading
    ///   indentation and spaces inside lines stay;
    /// - on a full-screen view (the alternate screen) every row ends a line:
    ///   such programs lay out and wrap their text themselves and often paint
    ///   whole rows (padding, borders, Copilot's scrollbar) straight into the
    ///   next one, so the wrap state says nothing about their text;
    /// - a wide character or grapheme cluster is copied once, also when the
    ///   selection starts on its right half;
    /// - only the terminal's cells are read: nothing the client draws over
    ///   them (selection, cursor, the user-message bar) is copied.
    ///
    /// Protocol-7 rows carry no wrap state: each of them ends a line.
    pub fn selection_text_with(&self, selection: &TermSelection, rules: CopyRules) -> String {
        let ((c0, r0), (c1, r1)) = selection.ordered();
        let cols = self.cols as usize;
        let full_screen = self.modes & MODE_ALT_SCREEN != 0;
        let scrollbar = rules.edge_scrollbar && full_screen;
        let mut out = String::new();
        for row in r0..=r1 {
            let (cells, wrap) = self.text_cells(row as usize);
            let last = row == r1;
            let soft = if last || full_screen { None } else { wrap };
            let mut start = if row == r0 { c0 as usize } else { 0 };
            if start > 0 && cells.get(start).is_some_and(Option::is_none) {
                start -= 1;
            }
            let mut end = if last { (c1 as usize + 1).min(cols) } else { cols };
            if let Some(wrap_end) = soft {
                end = end.min(wrap_end as usize);
            } else if scrollbar && end == cols && is_edge_scrollbar(&cells) {
                end -= 1;
            }
            let text: String = cells.get(start..end.max(start)).unwrap_or_default().iter().flatten().map(String::as_str).collect();
            if soft.is_some() {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end_matches(' '));
                if !last {
                    out.push_str("\r\n");
                }
            }
        }
        out
    }

    /// One string per column of a viewport row (`None`: the right half of a
    /// wide character; a grapheme cluster is one string) and, for a
    /// soft-wrapped row, the column where its text ends.
    pub fn text_cells(&self, row: usize) -> (Vec<Option<String>>, Option<u16>) {
        let cols = self.cols as usize;
        let mut cells = vec![Some(" ".to_owned()); cols];
        if self.rows8.is_empty() {
            for (cell, text) in cells.iter_mut().zip(self.row_cells(row)) {
                *cell = (!text.is_empty()).then_some(text);
            }
            return (cells, None);
        }
        let Some(runs) = self.rows8.get(row) else { return (cells, None) };
        for run in runs {
            let step = if run.wide { 2 } else { 1 };
            for (i, text) in run.cells().into_iter().enumerate() {
                let col = run.col as usize + i * step;
                if let Some(cell) = cells.get_mut(col) {
                    *cell = Some(text.to_owned());
                }
                if run.wide
                    && let Some(cell) = cells.get_mut(col + 1)
                {
                    *cell = None;
                }
            }
        }
        (cells, row_soft_wrap(runs))
    }
}

/// Program-specific copy rules ([`TermView::selection_text_with`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyRules {
    /// The program draws a scrollbar in the last column of its full-screen
    /// view (Copilot CLI): on the alternate screen, a `┃` or `│` alone in
    /// the last column of a row that ends its line (the cell before it is
    /// blank) is the scrollbar and is not copied.
    pub edge_scrollbar: bool,
}

/// The row's last cell is a vertical bar standing apart from its text.
fn is_edge_scrollbar(cells: &[Option<String>]) -> bool {
    match cells {
        [.., Some(before), Some(bar)] => before == " " && matches!(bar.as_str(), "┃" | "│"),
        _ => false,
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

    fn run8(col: u16, text: &str) -> Run8 {
        Run8 { col, text: text.into(), ..Default::default() }
    }

    fn view8(cols: u16, rows: Vec<Vec<Run8>>) -> TermView {
        let mut view = TermView::default();
        let frame = Frame8 {
            id: "t".into(),
            cols,
            rows: rows.len() as u16,
            full: true,
            changed: rows.into_iter().enumerate().map(|(y, row)| (y as u16, row)).collect(),
            cursor: None,
            cursor_shape: 0,
            cursor_blink: false,
            display_offset: 0,
            history: 0,
            modes: 0,
            links: Vec::new(),
        };
        view.apply8(frame);
        view
    }

    fn sel(a: (u16, u16), b: (u16, u16)) -> TermSelection {
        TermSelection { id: "t".into(), anchor: a, head: b }
    }

    #[test]
    fn selection_copies_reading_order_text_and_wide_characters_once() {
        let mut wide = run8(6, "中");
        wide.wide = true;
        let view = view8(8, vec![vec![run8(0, "hello "), wide], vec![run8(0, "abcdefgh")], vec![run8(0, "ij")], vec![run8(2, "tail")]]);
        assert_eq!(view.selection_text(&sel((0, 0), (4, 0))), "hello");
        assert_eq!(view.selection_text(&sel((6, 0), (7, 0))), "中", "wide character copied once");
        assert_eq!(view.selection_text(&sel((7, 0), (7, 0))), "中", "starting on its right half");
        // Backwards drag; row 1 fills the width but ends its line (no wrap state).
        assert_eq!(view.selection_text(&sel((1, 2), (3, 1))), "defgh\r\nij");
        assert_eq!(view.selection_text(&sel((0, 2), (7, 3))), "ij\r\n  tail");
        assert!(sel((5, 1), (2, 2)).contains(0, 2) && !sel((5, 1), (2, 2)).contains(4, 1));
    }

    #[test]
    fn soft_wrapped_rows_rejoin_and_hard_breaks_blank_lines_and_indentation_stay() {
        let view = view8(10, vec![
            // "The quick brown fox jumps" wrapped at 10 columns (the engines trim the wrapped rows' trailing spaces).
            vec![run8(0, "The quick"), Run8::soft_wrap(10)],
            vec![run8(0, "brown fox"), Run8::soft_wrap(10)],
            vec![run8(0, "jumps")],
            vec![],
            vec![run8(0, "    let x;"), Run8::soft_wrap(10)],
            vec![run8(0, "  y")],
            // Exactly the width, then a new line: not joined.
            vec![run8(0, "0123456789")],
            vec![run8(0, "next  "), run8(8, "x")],
        ]);
        assert_eq!(
            view.selection_text(&sel((0, 0), (9, 7))),
            "The quick brown fox jumps\r\n\r\n    let x;  y\r\n0123456789\r\nnext    x",
            "soft wraps rejoin; hard breaks, the blank line, indentation and inner spaces stay"
        );
        assert_eq!(view.selection_text(&sel((4, 0), (3, 1))), "quick brow", "a selection inside a wrapped line");
        assert_eq!(view.selection_text(&sel((0, 2), (9, 2))), "jumps", "trailing padding of a line is trimmed");
        assert_eq!(view.selection_text(&sel((0, 3), (9, 3))), "");
        // A row of blanks that wraps keeps its spaces.
        let blanks = view8(4, vec![vec![run8(0, "a"), Run8::soft_wrap(4)], vec![Run8::soft_wrap(4)], vec![run8(0, "b")]]);
        assert_eq!(blanks.selection_text(&sel((0, 0), (3, 2))), "a       b");
    }

    #[test]
    fn a_wide_character_that_moved_on_leaves_no_gap_and_clusters_stay_whole() {
        let mut wide = run8(0, "中");
        wide.wide = true;
        let mut cluster = run8(3, "e\u{301}!");
        cluster.clusters = vec![(0, 1)];
        // "123456789" then a wide character that did not fit in the last column.
        let view = view8(10, vec![vec![run8(0, "123456789"), Run8::soft_wrap(9)], vec![wide, run8(2, "x"), cluster]]);
        assert_eq!(view.selection_text(&sel((0, 0), (9, 1))), "123456789中xe\u{301}!");
        assert_eq!(view.selection_text(&sel((3, 1), (3, 1))), "e\u{301}", "a cluster is one cell");
    }

    #[test]
    fn full_screen_rows_never_rejoin() {
        // vim and OpenCode paint whole rows into the next one: every row wraps.
        let rows = vec![vec![run8(0, "~"), Run8::soft_wrap(6)], vec![run8(0, "~"), Run8::soft_wrap(6)], vec![run8(0, "a b c"), Run8::soft_wrap(6)], vec![run8(0, "d")]];
        let mut view = view8(6, rows);
        assert_eq!(view.selection_text(&sel((0, 0), (5, 3))), "~     ~     a b c d", "a shell's soft wraps rejoin");
        view.modes = MODE_ALT_SCREEN;
        assert_eq!(view.selection_text(&sel((0, 0), (5, 3))), "~\r\n~\r\na b c\r\nd");
    }

    #[test]
    fn copilots_scrollbar_column_is_not_copied_on_its_full_screen_view() {
        let rows = vec![
            vec![run8(0, " ● reply line one"), run8(19, "┃")],
            vec![run8(0, "   more│"), run8(19, "│")],
            vec![run8(0, " ❯ a│b"), run8(18, "x┃")],
        ];
        let mut view = view8(20, rows);
        let all = sel((0, 0), (19, 2));
        let copilot = CopyRules { edge_scrollbar: true };
        assert_eq!(view.selection_text_with(&all, copilot), " ● reply line one  ┃\r\n   more│           │\r\n ❯ a│b            x┃", "only on the alternate screen");
        view.modes = MODE_ALT_SCREEN;
        assert_eq!(view.selection_text_with(&all, copilot), " ● reply line one\r\n   more│\r\n ❯ a│b            x┃", "a bar right after text stays");
        assert_eq!(view.selection_text(&all), " ● reply line one  ┃\r\n   more│           │\r\n ❯ a│b            x┃", "other programs keep it");
    }

    #[test]
    fn protocol7_rows_have_no_wrap_state() {
        let view = TermView { cols: 4, rows: 2, lines: vec![vec![run(0, "abcd")], vec![run(0, "ef")]], ..Default::default() };
        assert_eq!(view.selection_text(&sel((0, 0), (3, 1))), "abcd\r\nef");
        assert_eq!(view.text_cells(0), (vec![Some("a".into()), Some("b".into()), Some("c".into()), Some("d".into())], None));
    }
}
