//! A multi-line text editor for notes (docs/NOTEBOOK-TASKS.md, "Editor"),
//! drawn in character cells so the TUI and the GUI share it.
//!
//! The editor knows nothing about the app: the caller feeds it keys, text
//! (pastes and IME commits) and mouse positions, draws it into a cell
//! buffer, and acts on the [`Outcome`]s it returns (clipboard, saving,
//! opening find). Positions are logical: a line index and a character
//! (`char`) column within that line.
//!
//! Behaviour notes for integrators:
//! - Lines wrap softly at the drawn width (after spaces when possible).
//!   Up/Down, PgUp/PgDn and [`Editor::scroll`] move by visual (wrapped)
//!   rows. [`Editor::draw`] always keeps the caret visible, so scrolling the
//!   caret out of view moves it along to the nearest visible row (the
//!   selection anchor stays, so a selection grows).
//! - Home goes to the start of the visual row; pressed there it toggles
//!   between the line's first non-space character and the line start. End
//!   goes to the end of the visual row (on a wrapped row: before the row's
//!   last character, where it breaks); pressed there it goes to the line end.
//! - Undo (Ctrl+Z; redo Ctrl+Y or Ctrl+Shift+Z): typed characters group into
//!   one step until the caret moves, as do consecutive Backspace/Delete
//!   presses; pastes, cuts, newlines and indents are steps of their own.
//!   Undo and redo restore the cursor and selection; undoing back to the
//!   saved text makes the editor clean again.
//! - Live mode shows every line except the cursor's line and the selected
//!   lines formatted, its Markdown syntax hidden; clicks on a formatted line
//!   map back to the raw column (and the line then shows raw).
//! - Find is case-insensitive; every match is highlighted, the current one
//!   (the selected match) more strongly.
//! - Ctrl+V is not handled (`Unhandled`): the caller pastes with
//!   [`Editor::insert_text`].

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::input::{KeyCode, KeyEvent, KeyEventKind, Modifiers};

mod draw;
mod edit;
mod find;
mod history;
mod layout;
mod markdown;
mod text;
mod view;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_view;

use history::History;
use layout::Layout;
use markdown::{BlockState, LineKind};
use text::{byte_at, char_count};

/// How lines are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Markdown source with syntax styling.
    #[default]
    Source,
    /// Live preview: lines without the cursor are shown formatted, their
    /// Markdown syntax hidden; the cursor's line is shown raw.
    Live,
}

/// What a key or text input did. The caller handles the last four.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not an editor key (the caller may use it).
    Unhandled,
    /// The cursor, selection or scroll changed; the text did not.
    Handled,
    /// The text changed.
    Changed,
    /// Put this text on the clipboard.
    Copy(String),
    /// Put this text on the clipboard (it was removed from the text).
    Cut(String),
    /// Ctrl+S: save now.
    Save,
    /// Ctrl+F: open the find bar.
    Find,
}

/// A logical position: line index and character column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Cached layouts of one logical line.
#[derive(Clone, Debug, Default)]
struct LineCache {
    raw: Option<Arc<Layout>>,
    formatted: Option<Arc<Layout>>,
}

/// A visual row drawn last time (for mapping the mouse).
#[derive(Clone, Debug)]
struct Shown {
    line: usize,
    row: usize,
    layout: Arc<Layout>,
}

/// What the last [`Editor::draw`] used.
#[derive(Clone, Debug, Default)]
struct View {
    area: Rect,
    gutter: u16,
    /// Text width in cells (0 before the first draw: no wrapping).
    width: usize,
    shown: Vec<Shown>,
}

/// The find query and its matches (kept up to date while editing).
#[derive(Clone, Debug)]
struct Find {
    lower: String,
    matches: Vec<(Pos, Pos)>,
}

/// A multi-line editor (see the module docs).
#[derive(Clone, Debug)]
pub struct Editor {
    lines: Vec<String>,
    cursor: Pos,
    anchor: Option<Pos>,
    /// Preferred visual column (cells) for vertical moves.
    goal: Option<usize>,
    crlf: bool,
    mode: Mode,
    read_only: bool,
    placeholder: String,
    line_numbers: bool,
    /// First visible visual row: (line, row of that line's layout).
    top: (usize, usize),
    history: History,
    find: Option<Find>,
    cache: Vec<LineCache>,
    /// Layouts created since the cache was last emptied (bounds memory).
    cached: usize,
    /// Block kinds of the first lines (valid prefix) and the state after each.
    kinds: Vec<(LineKind, BlockState)>,
    view: View,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

impl Editor {
    /// An editor holding `text` (not dirty, cursor at the start).
    pub fn new(text: &str) -> Self {
        let mut editor = Self {
            lines: vec![String::new()],
            cursor: Pos::default(),
            anchor: None,
            goal: None,
            crlf: false,
            mode: Mode::Source,
            read_only: false,
            placeholder: String::new(),
            line_numbers: false,
            top: (0, 0),
            history: History::default(),
            find: None,
            cache: Vec::new(),
            cached: 0,
            kinds: Vec::new(),
            view: View::default(),
        };
        editor.set_text(text);
        editor
    }

    /// The whole text, with the line ending it was loaded with.
    pub fn text(&self) -> String {
        self.lines.join(if self.crlf { "\r\n" } else { "\n" })
    }

    /// Replaces the text (cursor at the start, history cleared, not dirty).
    pub fn set_text(&mut self, text: &str) {
        self.crlf = text.contains("\r\n");
        self.lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor = Pos::default();
        self.anchor = None;
        self.goal = None;
        self.top = (0, 0);
        self.history.clear();
        self.reset_caches();
    }

    /// Replaces the text with a newer version of the same file, keeping the
    /// cursor and scroll as close as possible (history cleared, not dirty).
    pub fn reload(&mut self, text: &str) {
        let (cursor, top) = (self.cursor, self.top);
        self.set_text(text);
        self.cursor = self.clamp(cursor);
        self.top = (top.0.min(self.lines.len() - 1), top.1);
    }

    /// The app rewrote some lines (front matter) right after a change you
    /// made: the rewrite becomes part of that change's undo step, and the
    /// caret, the selection and the view stay on the same text. Answers
    /// whether the text changed.
    pub fn amend(&mut self, text: &str) -> bool {
        self.replace_changed_lines(text, true)
    }

    /// As [`Editor::amend`], as an undo step of its own (a mark the app writes).
    pub fn apply(&mut self, text: &str) -> bool {
        self.replace_changed_lines(text, false)
    }

    /// Whether the text changed since it was loaded or last saved.
    pub fn is_dirty(&self) -> bool {
        self.history.dirty()
    }

    /// The text was saved: it is no longer dirty.
    pub fn mark_saved(&mut self) {
        self.history.mark_saved();
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.view.shown.clear();
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Read only: moving, selecting and copying work, editing does not.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Text shown (dimmed) while the buffer is empty.
    pub fn set_placeholder(&mut self, text: &str) {
        self.placeholder = text.to_owned();
    }

    pub fn set_line_numbers(&mut self, on: bool) {
        self.line_numbers = on;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Line `index` (empty past the end).
    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map(String::as_str).unwrap_or("")
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    /// Moves the cursor (clamped to the text) and clears the selection.
    pub fn set_cursor(&mut self, pos: Pos) {
        self.cursor = self.clamp(pos);
        self.anchor = None;
        self.goal = None;
        self.history.seal();
    }

    /// Moves the cursor to the start of `line` and scrolls it into view.
    pub fn goto_line(&mut self, line: usize) {
        self.set_cursor(Pos::new(line, 0));
        self.top = (self.cursor.line, 0);
    }

    /// The selection, start before end (`None` when nothing is selected).
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| if anchor < self.cursor { (anchor, self.cursor) } else { (self.cursor, anchor) })
    }

    /// The selected text, lines joined with `\n`.
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.range_text(start, end))
    }

    pub fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        self.anchor = Some(Pos::default());
        self.cursor = Pos::new(last, char_count(&self.lines[last]));
        self.goal = None;
        self.history.seal();
    }

    /// Takes back the last change (Ctrl+Z); answers whether there was one.
    pub fn undo(&mut self) -> bool {
        !self.read_only && self.apply_history(false)
    }

    /// Does an undone change again (Ctrl+Y); answers whether there was one.
    pub fn redo(&mut self) -> bool {
        !self.read_only && self.apply_history(true)
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// The first logical line visible at the top (as of the last draw or scroll).
    pub fn first_visible_line(&self) -> usize {
        self.top.0.min(self.lines.len() - 1)
    }

    fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        Pos::new(line, pos.col.min(char_count(&self.lines[line])))
    }

    fn range_text(&self, start: Pos, end: Pos) -> String {
        let (start, end) = (self.clamp(start), self.clamp(end));
        if start >= end {
            return String::new();
        }
        if start.line == end.line {
            let line = &self.lines[start.line];
            return line[byte_at(line, start.col)..byte_at(line, end.col)].to_owned();
        }
        let first = &self.lines[start.line];
        let mut out = first[byte_at(first, start.col)..].to_owned();
        for line in &self.lines[start.line + 1..end.line] {
            out.push('\n');
            out.push_str(line);
        }
        let last = &self.lines[end.line];
        out.push('\n');
        out.push_str(&last[..byte_at(last, end.col)]);
        out
    }
}

impl Editor {
    /// Inserts text at the cursor (replacing the selection); line breaks
    /// split lines. Pastes and IME commits come here. A single character
    /// groups with typing in the undo history; longer text is a step of its own.
    pub fn insert_text(&mut self, text: &str) -> Outcome {
        if self.read_only {
            return Outcome::Handled;
        }
        let clean = text::clean(text);
        if clean.is_empty() {
            return Outcome::Handled;
        }
        let kind = if clean != "\n" && clean.chars().nth(1).is_none() { history::Kind::Type } else { history::Kind::Other };
        self.change(kind, |e| {
            e.delete_selection();
            e.insert_at_cursor(&clean);
        })
    }

    /// Handles a key (presses and repeats; releases are ignored).
    pub fn key(&mut self, key: &KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Unhandled;
        }
        let m = key.modifiers;
        // AltGr compositions arrive with Ctrl+Alt: they are text, not commands.
        let ctrl = m.contains(Modifiers::CONTROL) && !m.contains(Modifiers::ALT);
        let other = (m.contains(Modifiers::ALT) && !m.contains(Modifiers::CONTROL)) || m.contains(Modifiers::SUPER);
        let shift = m.contains(Modifiers::SHIFT);
        // With Ctrl these are the caller's (Ctrl+Tab switches tabs, ...).
        let callers = matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown);
        match key.code {
            KeyCode::Char(c) if ctrl => self.command(c.to_ascii_lowercase(), shift),
            _ if other || (ctrl && callers) => Outcome::Unhandled,
            KeyCode::Char('\n' | '\r') | KeyCode::Enter => self.newline(),
            KeyCode::Char(c) if c.is_control() && c != '\t' => Outcome::Unhandled,
            KeyCode::Char(c) => self.type_char(c),
            KeyCode::Tab => self.indent(shift),
            KeyCode::BackTab => self.indent(true),
            KeyCode::Backspace => self.backspace(ctrl),
            KeyCode::Delete => self.delete_forward(ctrl),
            KeyCode::Left => self.horizontal(false, ctrl, shift),
            KeyCode::Right => self.horizontal(true, ctrl, shift),
            KeyCode::Up => self.vertical(-1, shift),
            KeyCode::Down => self.vertical(1, shift),
            KeyCode::PageUp => self.page(false, shift),
            KeyCode::PageDown => self.page(true, shift),
            KeyCode::Home => self.home(ctrl, shift),
            KeyCode::End => self.end(ctrl, shift),
            _ => Outcome::Unhandled,
        }
    }

    /// Ctrl + a letter.
    fn command(&mut self, c: char, shift: bool) -> Outcome {
        let changed = |done: bool| if done { Outcome::Changed } else { Outcome::Handled };
        match c {
            's' => Outcome::Save,
            'f' => Outcome::Find,
            'a' => {
                self.select_all();
                Outcome::Handled
            }
            'c' => self.selected_text().map(Outcome::Copy).unwrap_or(Outcome::Handled),
            'x' => self.cut(),
            'z' if shift => changed(self.redo()),
            'z' => changed(self.undo()),
            'y' => changed(self.redo()),
            _ => Outcome::Unhandled,
        }
    }

    /// A click in `area`: places the caret (`extend`: Shift-click selects).
    pub fn click(&mut self, area: Rect, column: u16, row: u16, extend: bool) {
        let pos = self.pos_at(area, column, row, false);
        self.move_to(pos, extend);
    }

    /// The pointer moved with the button held: extends the selection (above
    /// or below the area it scrolls by a row).
    pub fn drag(&mut self, area: Rect, column: u16, row: u16) {
        let pos = self.pos_at(area, column, row, true);
        self.move_to(pos, true);
    }

    /// A double-click selects the word under the pointer.
    pub fn double_click(&mut self, area: Rect, column: u16, row: u16) {
        let pos = self.pos_at(area, column, row, false);
        self.select_word(pos);
    }

    /// Scrolls by `rows` visual rows (negative: up); the caret moves along
    /// when it would leave the view.
    pub fn scroll(&mut self, rows: i32) {
        self.scroll_rows(rows as i64);
    }

    /// Sets (or clears) the find query.
    pub fn set_find(&mut self, query: Option<&str>) {
        self.find_set(query);
    }

    /// Selects the next (or previous) match, wrapping around, and scrolls it
    /// into view; answers whether there is one.
    pub fn find_next(&mut self, forward: bool) -> bool {
        self.find_step(forward)
    }

    /// The current match (1-based; 0 when the selection is not a match) and
    /// how many there are.
    pub fn find_status(&self) -> Option<(usize, usize)> {
        let find = self.find.as_ref()?;
        let current = self.selection().and_then(|sel| find.matches.binary_search(&sel).ok()).map(|i| i + 1).unwrap_or(0);
        Some((current, find.matches.len()))
    }

    /// The text after an unclosed `[[` before the caret on its line (a
    /// wiki-link being typed), for completion.
    pub fn completion_prefix(&self) -> Option<String> {
        let line = &self.lines[self.cursor.line];
        let before = &line[..byte_at(line, self.cursor.col)];
        let start = before.rfind("[[")?;
        let partial = &before[start + 2..];
        (!partial.contains("]]")).then(|| partial.to_owned())
    }

    /// Replaces the `chars` characters before the caret with `text`
    /// (completing a wiki-link); one undo step.
    pub fn replace_before_cursor(&mut self, chars: usize, text: &str) {
        if self.read_only {
            return;
        }
        let clean = text::clean(text);
        let Pos { line, col } = self.cursor;
        self.change(history::Kind::Other, |e| {
            e.anchor = None;
            e.cursor = e.edit(Pos::new(line, col.saturating_sub(chars)), Pos::new(line, col), &clean);
        });
    }

    /// Draws the visible rows into `area` (the caret kept visible); answers
    /// the caret's cell when `focused` and the caret is visible.
    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, focused: bool) -> Option<(u16, u16)> {
        self.draw_into(buf, area, focused)
    }
}
