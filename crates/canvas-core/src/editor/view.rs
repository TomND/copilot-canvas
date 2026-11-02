//! Layout caching and everything that works in visual rows: vertical moves,
//! paging, Home/End, scrolling, keeping the caret visible and mouse mapping.

use std::sync::Arc;

use ratatui::layout::Rect;

use super::layout::Layout;
use super::markdown::{self, LineKind};
use super::text::{char_count, indent_of, word_at, word_left, word_right};
use super::{Editor, LineCache, Mode, Outcome, Pos};

/// Layouts made before the cache is emptied (bounds memory on huge files).
const CACHE_LIMIT: usize = 4096;

/// A visual row: (logical line, row of its layout).
pub(super) type Row = (usize, usize);

impl Editor {
    pub(super) fn reset_caches(&mut self) {
        self.cache = vec![LineCache::default(); self.lines.len()];
        self.cached = 0;
        self.kinds.clear();
        self.view.shown.clear();
        self.find_refresh();
    }

    /// Empties the layout cache when it grew too big.
    pub(super) fn trim_cache(&mut self) {
        if self.cached > CACHE_LIMIT || self.cache.len() != self.lines.len() {
            self.cache = vec![LineCache::default(); self.lines.len()];
            self.cached = 0;
        }
    }

    /// The wrap width (no wrapping before the first draw).
    pub(super) fn wrap_width(&self) -> usize {
        if self.view.width == 0 { usize::MAX } else { self.view.width }
    }

    /// The block kind of line `index` (front matter, fence, code, text).
    pub(super) fn kind(&mut self, index: usize) -> LineKind {
        let index = index.min(self.lines.len() - 1);
        while self.kinds.len() <= index {
            let i = self.kinds.len();
            let state = self.kinds.last().map(|k| k.1).unwrap_or_default();
            let next = markdown::step(state, i, &self.lines[i]);
            self.kinds.push(next);
        }
        self.kinds[index].0
    }

    /// Whether line `index` shows formatted: Live mode, not the cursor's
    /// line, not a selected line.
    pub(super) fn is_formatted(&self, index: usize) -> bool {
        if self.mode != Mode::Live || index == self.cursor.line {
            return false;
        }
        match self.anchor {
            Some(anchor) => !(anchor.line.min(self.cursor.line)..=anchor.line.max(self.cursor.line)).contains(&index),
            None => true,
        }
    }

    /// Line `index` laid out raw or formatted (cached).
    pub(super) fn layout(&mut self, index: usize, formatted: bool) -> Arc<Layout> {
        if self.cache.len() != self.lines.len() {
            self.cache = vec![LineCache::default(); self.lines.len()];
        }
        let index = index.min(self.lines.len() - 1);
        let width = self.wrap_width();
        let kind = if formatted { self.kind(index) } else { LineKind::Text };
        let slot = &self.cache[index];
        let cached = if formatted { &slot.formatted } else { &slot.raw };
        if let Some(layout) = cached
            && layout.width == width
            && layout.kind == kind
        {
            return layout.clone();
        }
        let line = &self.lines[index];
        let layout = Arc::new(if formatted { Layout::formatted(line, kind, width) } else { Layout::raw(line, width) });
        let slot = &mut self.cache[index];
        *(if formatted { &mut slot.formatted } else { &mut slot.raw }) = Some(layout.clone());
        self.cached += 1;
        layout
    }

    /// Line `index` laid out as it shows now.
    pub(super) fn shown_layout(&mut self, index: usize) -> Arc<Layout> {
        let formatted = self.is_formatted(index);
        self.layout(index, formatted)
    }

    fn rows_in(&mut self, index: usize, shown: bool) -> usize {
        let layout = if shown { self.shown_layout(index) } else { self.layout(index, false) };
        layout.row_count()
    }

    /// Moves `delta` visual rows from `from` (clamped to the document), with
    /// the layouts as shown (`shown`) or raw; answers where it got and how
    /// many rows it moved (negative: up).
    pub(super) fn walk(&mut self, from: Row, delta: i64, shown: bool) -> (Row, i64) {
        let line = from.0.min(self.lines.len() - 1);
        let (mut line, mut row) = (line, from.1.min(self.rows_in(line, shown) - 1));
        let mut moved = 0i64;
        if delta >= 0 {
            let mut left = delta;
            loop {
                let rows = self.rows_in(line, shown) as i64;
                let room = rows - 1 - row as i64;
                if left <= room {
                    row += left as usize;
                    moved += left;
                    break;
                }
                if line + 1 >= self.lines.len() {
                    moved += room;
                    row = rows as usize - 1;
                    break;
                }
                moved += room + 1;
                left -= room + 1;
                line += 1;
                row = 0;
            }
        } else {
            let mut left = delta.saturating_neg();
            loop {
                if left <= row as i64 {
                    row -= left as usize;
                    moved -= left;
                    break;
                }
                if line == 0 {
                    moved -= row as i64;
                    row = 0;
                    break;
                }
                moved -= row as i64 + 1;
                left -= row as i64 + 1;
                line -= 1;
                row = self.rows_in(line, shown) - 1;
            }
        }
        ((line, row), moved)
    }

    /// Keeps the top row inside the document.
    pub(super) fn clamp_top(&mut self) {
        let line = self.top.0.min(self.lines.len() - 1);
        let rows = self.rows_in(line, true);
        self.top = (line, self.top.1.min(rows - 1));
    }

    /// The caret's visual row and its x (cells) in that row.
    pub(super) fn caret_visual(&mut self) -> (Row, usize) {
        let layout = self.shown_layout(self.cursor.line);
        let d = layout.to_shown(self.cursor.col);
        ((self.cursor.line, layout.row_of(d)), layout.x_of(d))
    }

    /// Scrolls so the caret's row is among the `height` rows from the top,
    /// without leaving rows empty below the end when the text is longer.
    pub(super) fn reveal(&mut self, height: usize) {
        let height = height.max(1) as i64;
        self.clamp_top();
        let (caret, _) = self.caret_visual();
        if caret < self.top {
            self.top = caret;
        } else {
            let (bottom, _) = self.walk(self.top, height - 1, true);
            if caret > bottom {
                self.top = self.walk(caret, -(height - 1), true).0;
            }
        }
        self.fill_view(height);
    }

    /// When the text ends before the last row, scrolls up to fill the view.
    fn fill_view(&mut self, height: i64) {
        let (end, moved) = self.walk(self.top, height - 1, true);
        if moved < height - 1 {
            self.top = self.walk(end, -(height - 1), true).0;
        }
    }

    /// Scrolls the view by `rows` visual rows (not past the end); the caret
    /// moves to the nearest visible row when it would leave the view.
    pub(super) fn scroll_rows(&mut self, rows: i64) {
        let height = self.view.area.height as i64;
        self.clamp_top();
        self.top = self.walk(self.top, rows, true).0;
        if height == 0 {
            return;
        }
        self.fill_view(height);
        let (caret, x) = self.caret_visual();
        let bottom = self.walk(self.top, height - 1, true).0;
        let target = if caret < self.top { self.top } else if caret > bottom { bottom } else { return };
        let goal = self.goal.unwrap_or(x);
        let layout = self.shown_layout(target.0);
        let pos = Pos::new(target.0, layout.to_raw(layout.at_x(target.1, goal)));
        self.move_to(pos, self.anchor.is_some());
        self.goal = Some(goal);
    }

    /// Up/Down by `delta` visual rows, keeping the preferred column.
    pub(super) fn vertical(&mut self, delta: i64, extend: bool) -> Outcome {
        let (caret, x) = self.caret_visual();
        let goal = self.goal.unwrap_or(x);
        let ((line, row), moved) = self.walk(caret, delta, false);
        if moved == 0 {
            // Up on the first row goes to the start, Down on the last to the end.
            let pos = if delta < 0 { Pos::default() } else { Pos::new(usize::MAX, usize::MAX) };
            return self.move_to(pos, extend);
        }
        let layout = self.layout(line, false);
        let pos = Pos::new(line, layout.to_raw(layout.at_x(row, goal)));
        self.move_to(pos, extend);
        self.goal = Some(goal);
        Outcome::Handled
    }

    /// PgUp/PgDn: the view and the caret move by the visible rows.
    pub(super) fn page(&mut self, down: bool, extend: bool) -> Outcome {
        let rows = (self.view.area.height as i64).max(1);
        let delta = if down { rows } else { -rows };
        self.clamp_top();
        self.top = self.walk(self.top, delta, true).0;
        self.vertical(delta, extend)
    }

    /// Home: the visual row's start; there, the line's first non-space
    /// character, and from there the line start.
    pub(super) fn home(&mut self, ctrl: bool, extend: bool) -> Outcome {
        if ctrl {
            return self.move_to(Pos::default(), extend);
        }
        let Pos { line, col } = self.cursor;
        let layout = self.layout(line, false);
        let start = layout.rows[layout.row_of(col)];
        let indent = indent_of(&self.lines[line]);
        let target = if col != start { start } else if col != indent { indent } else { 0 };
        self.move_to(Pos::new(line, target), extend)
    }

    /// End: the visual row's end (before the break of a wrapped row); there,
    /// the line end.
    pub(super) fn end(&mut self, ctrl: bool, extend: bool) -> Outcome {
        if ctrl {
            return self.move_to(Pos::new(usize::MAX, usize::MAX), extend);
        }
        let Pos { line, col } = self.cursor;
        let layout = self.layout(line, false);
        let row = layout.row_of(col);
        let (start, stop) = layout.row_range(row);
        let row_end = if row + 1 < layout.row_count() && stop > start { stop - 1 } else { layout.raw_len };
        let target = if col != row_end { row_end } else { layout.raw_len };
        self.move_to(Pos::new(line, target), extend)
    }

    /// Moves the caret (`extend`: keeps or starts the selection).
    pub(super) fn move_to(&mut self, pos: Pos, extend: bool) -> Outcome {
        if !extend {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        self.cursor = self.clamp(pos);
        self.goal = None;
        self.history.seal();
        Outcome::Handled
    }

    /// Left/Right by a character or a word; without Shift a selection
    /// collapses to its start or end first.
    pub(super) fn horizontal(&mut self, right: bool, word: bool, extend: bool) -> Outcome {
        if !extend
            && !word
            && let Some((start, end)) = self.selection()
        {
            return self.move_to(if right { end } else { start }, false);
        }
        let target = self.step_pos(self.cursor, right, word);
        self.move_to(target, extend)
    }

    /// One character or word left or right of `pos`, across line ends.
    pub(super) fn step_pos(&self, pos: Pos, right: bool, word: bool) -> Pos {
        let pos = self.clamp(pos);
        let line = &self.lines[pos.line];
        let len = char_count(line);
        if right && pos.col >= len {
            return if pos.line + 1 < self.lines.len() { Pos::new(pos.line + 1, 0) } else { pos };
        }
        if !right && pos.col == 0 {
            return if pos.line > 0 { Pos::new(pos.line - 1, char_count(&self.lines[pos.line - 1])) } else { pos };
        }
        if !word {
            return Pos::new(pos.line, if right { pos.col + 1 } else { pos.col - 1 });
        }
        let chars: Vec<char> = line.chars().collect();
        Pos::new(pos.line, if right { word_right(&chars, pos.col) } else { word_left(&chars, pos.col) })
    }

    /// Selects the word (or the run of spaces or punctuation) at `pos`.
    pub(super) fn select_word(&mut self, pos: Pos) {
        let pos = self.clamp(pos);
        let chars: Vec<char> = self.lines[pos.line].chars().collect();
        let (start, end) = word_at(&chars, pos.col);
        self.anchor = Some(Pos::new(pos.line, start));
        self.cursor = Pos::new(pos.line, end);
        self.goal = None;
        self.history.seal();
    }

    /// Line-number gutter width for an area `width` cells wide.
    pub(super) fn gutter_for(&self, width: u16) -> u16 {
        if !self.line_numbers {
            return 0;
        }
        let digits = self.lines.len().to_string().len().max(3) as u16;
        if digits + 2 >= width { 0 } else { digits + 1 }
    }

    /// The text position under a cell, as laid out by the last draw
    /// (`drag`: a row just outside the area means the row beyond the view).
    pub(super) fn pos_at(&mut self, area: Rect, column: u16, row: u16, drag: bool) -> Pos {
        if self.view.area != area {
            let gutter = self.gutter_for(area.width);
            self.view.area = area;
            self.view.gutter = gutter;
            self.view.width = area.width.saturating_sub(gutter) as usize;
            self.view.shown.clear();
        }
        let height = area.height as i64;
        let r = row as i64 - area.y as i64;
        let r = if drag { r.clamp(-1, height) } else { r.clamp(0, (height - 1).max(0)) };
        let x = (column as i64 - area.x as i64 - self.view.gutter as i64).max(0) as usize;
        let snapshot = usize::try_from(r).ok().and_then(|r| self.view.shown.get(r)).filter(|s| s.line < self.lines.len());
        let (line, vrow, layout) = match snapshot {
            Some(shown) => (shown.line, shown.row, shown.layout.clone()),
            None => {
                self.clamp_top();
                let ((line, vrow), _) = self.walk(self.top, r, true);
                (line, vrow, self.shown_layout(line))
            }
        };
        self.clamp(Pos::new(line, layout.to_raw(layout.at_x(vrow, x))))
    }
}
