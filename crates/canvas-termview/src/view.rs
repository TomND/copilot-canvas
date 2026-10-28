//! The client-side model of one hosted terminal, built from L1 frames.

use canvas_common::protocol::{F_ITALIC, Frame, Run};

#[derive(Default, Debug)]
pub struct TermView {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run>>,
    pub cursor: Option<(u16, u16)>,
    pub display_offset: u32,
    pub history: u32,
}

impl TermView {
    pub fn apply(&mut self, frame: Frame) {
        if frame.full || frame.rows != self.rows || frame.cols != self.cols {
            self.lines = vec![Vec::new(); frame.rows as usize];
        }
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
