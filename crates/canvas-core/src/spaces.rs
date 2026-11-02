//! The three spaces of a window (docs/NOTEBOOK-TASKS.md, "Spaces"): the
//! session canvas, the notebook and the todo boards, with navigation between them.
//!
//! - [`Space`]: what the main area shows (`App::space`, kept in the window's
//!   preferences). The toolbar's tabs and `Ctrl+1/2/3` switch.
//! - [`History`]: where this window has been (`App::nav`, in memory);
//!   `Alt+←` / `Alt+→` (or `Alt+,` / `Alt+.`, which Windows Terminal leaves
//!   alone; the GUI's mouse back / forward buttons) go back and forward.
//! - [`App::navigate`]: the one way to jump to a place (chips, badges, Go to,
//!   links between notes and todos): it records the place left and the place
//!   reached, switches the space and shows the place through the space's
//!   `open` (the canvas's here). Viewing never starts a session.
//! - Go to (`Ctrl+P`, [`GoTo`]): one fuzzy search ([`fuzzy`]) over groups and
//!   sessions ([`goto_items`]), `notebook::goto_items` and `tasks::goto_items`.
//!
//! Keys of this file work outside terminals and dialogs ([`App::space_key`]);
//! the Notebook and Tasks modules get the keyboard and the mouse of their main
//! area and sidebar ([`App::space_has_keys`]).

use crate::app::{App, Focus};
use crate::dialogs::{Modal, TextInput};
use crate::doc::NodeKind;
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseEvent};
use crate::modals::Outcome;
use crate::side::{RightDetail, SideView};

/// What a window's main area shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Space {
    /// The session canvas.
    #[default]
    Canvas,
    Notebook,
    Tasks,
}

impl Space {
    /// In the toolbar's order (`Ctrl+1`, `Ctrl+2`, `Ctrl+3`).
    pub const ALL: [Space; 3] = [Space::Canvas, Space::Notebook, Space::Tasks];

    pub fn glyph(self) -> &'static str {
        match self {
            Space::Canvas => "◫",
            Space::Notebook => "✎",
            Space::Tasks => "☑",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Space::Canvas => "Canvas",
            Space::Notebook => "Notebook",
            Space::Tasks => "Tasks",
        }
    }

    /// Its key in the window's preferences.
    pub fn key(self) -> &'static str {
        match self {
            Space::Canvas => "canvas",
            Space::Notebook => "notebook",
            Space::Tasks => "tasks",
        }
    }

    pub fn parse(key: &str) -> Option<Space> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }

    /// Slot 1 of the activity bar in this space: its icon and its sidebar's title.
    pub fn side(self) -> (&'static str, &'static str) {
        match self {
            Space::Canvas => ("▤", "SESSIONS"),
            Space::Notebook => ("✎", "EXPLORER"),
            Space::Tasks => ("☑", "TASKS"),
        }
    }
}

/// A place within a space.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum NavTarget {
    /// The space as it was left.
    #[default]
    None,
    /// A canvas group (id): on the canvas, in the notebook (its folder) or in
    /// Tasks (its boards).
    Group(String),
    /// A box on the canvas (node id).
    Node(String),
    /// A note (its path on the service's machine).
    Note(String),
    /// A notebook folder (its path).
    Folder(String),
    /// A todo board (id).
    Board(String),
    /// A todo (id, e.g. `DEC-031`).
    Todo(String),
}

/// Most entries a window's history keeps.
pub const HISTORY_CAP: usize = 100;

/// A window's navigation history: `(space, place)` entries with a current
/// one. A push after going back drops the entries ahead; a push equal to the
/// entry before it is not added again; the oldest go beyond [`HISTORY_CAP`].
#[derive(Clone, Debug, Default)]
pub struct History {
    entries: Vec<(Space, NavTarget)>,
    at: usize,
}

impl History {
    pub fn push(&mut self, space: Space, target: NavTarget) {
        let entry = (space, target);
        if !self.entries.is_empty() {
            self.entries.truncate(self.at + 1);
            if self.entries.last() == Some(&entry) {
                return;
            }
        }
        self.entries.push(entry);
        if self.entries.len() > HISTORY_CAP {
            let extra = self.entries.len() - HISTORY_CAP;
            self.entries.drain(..extra);
        }
        self.at = self.entries.len() - 1;
    }

    /// The entry the window is at.
    pub fn current(&self) -> Option<&(Space, NavTarget)> {
        self.entries.get(self.at)
    }

    /// Changes the current entry's place (the window moved within its space
    /// without a jump), keeping the entries ahead.
    pub fn replace_current(&mut self, target: NavTarget) {
        let Some(entry) = self.entries.get_mut(self.at) else { return };
        entry.1 = target;
        // No two equal entries in a row (a step back or forward always goes somewhere).
        if self.at + 1 < self.entries.len() && self.entries[self.at + 1] == self.entries[self.at] {
            self.entries.remove(self.at + 1);
        }
        if self.at > 0 && self.entries[self.at - 1] == self.entries[self.at] {
            self.entries.remove(self.at);
            self.at -= 1;
        }
    }

    pub fn can_back(&self) -> bool {
        self.at > 0 && !self.entries.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        self.at + 1 < self.entries.len()
    }

    /// Steps back: the entry before the current one.
    pub fn back(&mut self) -> Option<(Space, NavTarget)> {
        if !self.can_back() {
            return None;
        }
        self.at -= 1;
        self.entries.get(self.at).cloned()
    }

    /// Steps forward again.
    pub fn forward(&mut self) -> Option<(Space, NavTarget)> {
        if !self.can_forward() {
            return None;
        }
        self.at += 1;
        self.entries.get(self.at).cloned()
    }

    pub fn entries(&self) -> &[(Space, NavTarget)] {
        &self.entries
    }
}

// ---------- Go to ----------

/// One thing Go to finds.
#[derive(Clone, Debug, PartialEq)]
pub struct GoItem {
    /// What is matched and shown ("Payments", "DEC-031 Fix the upload").
    pub label: String,
    /// Shown dimmed after it (where it is, its state); matched only when the label is not.
    pub detail: String,
    /// Its kind, one cell wide (`▣` group, a session's status icon, `✎` note, `☑` todo).
    pub glyph: &'static str,
    /// Where Enter goes.
    pub space: Space,
    pub target: NavTarget,
}

/// Most matches Go to lists.
pub const GOTO_MAX: usize = 200;
/// The score of a match in an item's detail (below any label match).
const DETAIL_MATCH: i32 = i32::MIN / 8;

/// The Go to dialog: a query, everything it searches (read when it opened)
/// and the matches, best first.
#[derive(Clone, Debug)]
pub struct GoTo {
    pub input: TextInput,
    pub items: Vec<GoItem>,
    /// (index in `items`, matched character positions in its label).
    pub results: Vec<(usize, Vec<usize>)>,
    pub sel: usize,
}

impl GoTo {
    pub fn new(items: Vec<GoItem>) -> Self {
        let mut goto = GoTo { input: TextInput::default(), items, results: Vec::new(), sel: 0 };
        goto.refilter();
        goto
    }

    /// Matches the query again: every item in provider order while it is
    /// empty, else by the label's best fuzzy score, shorter labels before
    /// longer ones; items whose detail holds the query as it is come after
    /// every label match.
    pub fn refilter(&mut self) {
        let query = self.input.text.trim();
        self.sel = 0;
        if query.is_empty() {
            self.results = (0..self.items.len().min(GOTO_MAX)).map(|i| (i, Vec::new())).collect();
            return;
        }
        let needle = query.to_lowercase();
        let mut scored: Vec<(i32, usize, Vec<usize>)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match fuzzy(query, &item.label) {
                Some((score, positions)) => Some((score, i, positions)),
                None => item.detail.to_lowercase().contains(&needle).then(|| (DETAIL_MATCH, i, Vec::new())),
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(self.items[a.1].label.chars().count().cmp(&self.items[b.1].label.chars().count())).then(a.1.cmp(&b.1)));
        scored.truncate(GOTO_MAX);
        self.results = scored.into_iter().map(|(_, i, positions)| (i, positions)).collect();
    }

    /// The highlighted match.
    pub fn selected(&self) -> Option<&GoItem> {
        self.results.get(self.sel).and_then(|(i, _)| self.items.get(*i))
    }
}

/// Fuzzy match scores.
const MATCH: i32 = 16;
/// A match at the start of a word (after a separator, a lower → upper case
/// change, or a letter → digit change).
const WORD_START: i32 = 20;
/// The match is the text's first character.
const FIRST: i32 = 8;
/// The match follows the previous one directly.
const RUN: i32 = 24;
/// The match skips characters after the previous one: this, and
/// `GAP_EACH` per character skipped.
const GAP: i32 = 6;
const GAP_EACH: i32 = 1;
/// At most this much is taken off for where the first match is.
const LEAD: i32 = 12;
const NONE: i32 = i32::MIN / 4;

/// Matches `query` (whitespace ignored) as a case-insensitive subsequence of
/// `text`: the best score (higher is better: word starts and contiguous runs
/// win, an early first match helps) and the matched character positions, or
/// `None` when it is not a subsequence. An empty query matches with 0.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).take(64).map(lower).collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    let t: Vec<char> = text.chars().take(512).collect();
    let tl: Vec<char> = t.iter().map(|c| lower(*c)).collect();
    let (m, n) = (q.len(), t.len());
    if m > n {
        return None;
    }
    // Most items do not match at all: one pass says so before the table.
    let mut rest = tl.iter();
    if !q.iter().all(|c| rest.any(|t| t == c)) {
        return None;
    }
    let bonus: Vec<i32> = (0..n)
        .map(|j| match j.checked_sub(1).map(|p| t[p]) {
            None => WORD_START + FIRST,
            Some(p) if !p.is_alphanumeric() => WORD_START,
            Some(p) if p.is_lowercase() && t[j].is_uppercase() => WORD_START,
            Some(p) if p.is_alphabetic() && t[j].is_ascii_digit() => WORD_START / 2,
            _ => 0,
        })
        .collect();
    // score[i][j]: the best score with query character i matched at j;
    // from[i][j]: where character i - 1 was then.
    let mut score = vec![vec![NONE; n]; m];
    let mut from = vec![vec![usize::MAX; n]; m];
    for j in 0..n {
        if tl[j] == q[0] {
            score[0][j] = MATCH + bonus[j] - (j as i32).min(LEAD);
        }
    }
    for i in 1..m {
        // The best of row i - 1 at positions k before j - 1 (a gap), as
        // score + k * GAP_EACH, so the gap's length is taken off at j.
        let (mut best, mut best_at) = (NONE, usize::MAX);
        for j in i..n {
            if j >= 2 && score[i - 1][j - 2] > NONE && score[i - 1][j - 2] + (j - 2) as i32 * GAP_EACH > best {
                best = score[i - 1][j - 2] + (j - 2) as i32 * GAP_EACH;
                best_at = j - 2;
            }
            if tl[j] != q[i] {
                continue;
            }
            let run = if score[i - 1][j - 1] > NONE { score[i - 1][j - 1] + RUN } else { NONE };
            let gap = if best > NONE { best - GAP - (j as i32 - 1) * GAP_EACH } else { NONE };
            let (prev, at) = if run >= gap { (run, j - 1) } else { (gap, best_at) };
            if prev > NONE {
                score[i][j] = prev + MATCH + bonus[j];
                from[i][j] = at;
            }
        }
    }
    let (end, best) = (0..n).filter(|&j| score[m - 1][j] > NONE).map(|j| (j, score[m - 1][j])).max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))?;
    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = j;
        if i > 0 {
            j = from[i][j];
        }
    }
    Some((best, positions))
}

/// Go to's own provider: the canvas's groups and sessions (the document).
pub fn goto_items(app: &App) -> Vec<GoItem> {
    let mut items = Vec::new();
    for group in &app.doc.groups {
        let members = app.doc.members(&group.id).len();
        let detail = match &group.folder {
            Some(folder) => format!("group · {members} · {folder}"),
            None => format!("group · {members}"),
        };
        items.push(GoItem { label: group.name.clone(), detail, glyph: crate::ui_canvas::group_icon(group), space: Space::Canvas, target: NavTarget::Group(group.id.clone()) });
    }
    for node in app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
        let status = app.node_status(node);
        let mut detail = vec![status.label().to_owned()];
        if let Some(group) = app.doc.group_of(node) {
            detail.push(group.name.clone());
        }
        if let Some(cwd) = node.cwd.as_deref().filter(|c| !c.is_empty()) {
            detail.push(cwd.to_owned());
        }
        items.push(GoItem { label: node.name.clone(), detail: detail.join(" · "), glyph: status.icon(0), space: Space::Canvas, target: NavTarget::Node(node.id.clone()) });
    }
    items
}

impl App {
    /// The space's module has the keyboard and the mouse: its main area
    /// (`Focus::Canvas`) or its sidebar (slot 1 of the activity bar).
    pub fn space_has_keys(&self) -> bool {
        self.space != Space::Canvas && (self.focus == Focus::Canvas || (self.focus == Focus::Sidebar && self.side_view == SideView::Sessions))
    }

    /// Ctrl+1/2/3 (switch spaces), Alt+← / Alt+→ or Alt+, / Alt+. (back,
    /// forward), Ctrl+P (Go to): outside terminals and dialogs. True when the
    /// key was one of them. Windows Terminal keeps Alt+arrows (pane focus)
    /// and Ctrl+Alt+← for itself by default, so Alt+, and Alt+. (`<` `>`
    /// without Shift; unbound in Windows Terminal, conhost and the GUI) do
    /// the same everywhere; the GUI adds the mouse's back and forward buttons
    /// ([`App::nav_button`]).
    pub fn space_key(&mut self, key: &KeyEvent) -> bool {
        let m = key.modifiers;
        let ctrl = m.contains(Modifiers::CONTROL) && !m.contains(Modifiers::ALT);
        // On a Mac only ⌘[ / ⌘] (Option+arrows move by words there).
        let alt = self.key_policy.alt_shortcut(key) && !m.contains(Modifiers::CONTROL);
        match key.code {
            KeyCode::Char(c @ '1'..='3') if ctrl => self.switch_space(Space::ALL[(c as u8 - b'1') as usize]),
            KeyCode::Left | KeyCode::Char(',') if alt => self.nav_back(),
            KeyCode::Right | KeyCode::Char('.') if alt => self.nav_forward(),
            // The console may report Ctrl+P as its control character.
            KeyCode::Char('p' | 'P') if ctrl && !m.contains(Modifiers::SHIFT) => self.open_goto(),
            KeyCode::Char('\x10') if m.is_empty() || ctrl => self.open_goto(),
            _ => return false,
        }
        true
    }

    /// Shows `space` as it was left (a jump: the history records it).
    pub fn switch_space(&mut self, space: Space) {
        if space == self.space {
            if self.focus == Focus::Terminal {
                self.set_focus(Focus::Canvas);
            }
            self.needs_draw = true;
            return;
        }
        self.navigate(space, NavTarget::None);
    }

    /// Goes to `target` in `space`: records the place left and this one in
    /// the history, switches the space and shows the place (the canvas flies
    /// to a group or box; Notebook and Tasks through their `open`). Never
    /// starts a session. A todo the loaded boards do not have (a note's
    /// `ISO-8601` looks like an id) is said, and the window stays where it is.
    pub fn navigate(&mut self, space: Space, target: NavTarget) {
        if let NavTarget::Todo(id) = &target
            && crate::tasks::book(self).is_some_and(|b| b.todo(id).is_none())
        {
            return self.notify(format!("There is no todo {id} on this canvas's boards."), false);
        }
        self.record_here();
        self.nav.push(space, target.clone());
        self.go(space, target);
    }

    /// Alt+←: the place before (it records where the window is first, so
    /// Alt+→ comes back to it).
    pub fn nav_back(&mut self) {
        self.record_here();
        match self.nav.back() {
            Some((space, target)) => self.go(space, target),
            None => self.notify(self.key_policy.label("Nothing to go back to (Alt+→ goes forward).").into_owned(), false),
        }
    }

    /// Alt+→: the place gone back from.
    pub fn nav_forward(&mut self) {
        self.record_here();
        match self.nav.forward() {
            Some((space, target)) => self.go(space, target),
            None => self.notify(self.key_policy.label("Nothing to go forward to (Alt+← goes back).").into_owned(), false),
        }
    }

    /// The mouse's back / forward button (the GUI hands them over; the
    /// core's mouse events do not carry them): the history, as Alt+← / Alt+→,
    /// also while a terminal has the keyboard; not while a dialog is open.
    pub fn nav_button(&mut self, forward: bool) {
        if !self.modals.is_empty() {
            return;
        }
        self.stop_glide();
        if forward { self.nav_forward() } else { self.nav_back() }
        self.needs_draw = true;
    }

    /// Ctrl+P: the Go to dialog over everything the window knows.
    pub fn open_goto(&mut self) {
        let mut items = goto_items(self);
        items.extend(crate::notebook::goto_items(self));
        items.extend(crate::tasks::goto_items(self));
        self.modals.push(Modal::GoTo(Box::new(GoTo::new(items))));
        self.needs_draw = true;
    }

    /// Where the window is: the canvas's selected group or box, the
    /// Notebook's open note, Tasks' selected todo or board.
    fn nav_here(&self) -> (Space, NavTarget) {
        let target = match self.space {
            Space::Canvas => match (&self.selected_group, self.selection.last()) {
                (Some(group), _) => NavTarget::Group(group.clone()),
                (None, Some(node)) => NavTarget::Node(node.clone()),
                (None, None) => NavTarget::None,
            },
            Space::Notebook => crate::notebook::current_target(self),
            Space::Tasks => crate::tasks::current_target(self),
        };
        (self.space, target)
    }

    /// Records where the window is before it moves: a new entry when the
    /// current one is of another space (or there is none), else the current
    /// entry follows a move within the space (the canvas's selection, the
    /// note shown, the todo selected).
    fn record_here(&mut self) {
        let (space, target) = self.nav_here();
        match self.nav.current() {
            Some((current, _)) if *current == space => {
                if target != NavTarget::None {
                    self.nav.replace_current(target);
                }
            }
            _ => self.nav.push(space, target),
        }
    }

    /// Shows a place without touching the history.
    fn go(&mut self, space: Space, target: NavTarget) {
        self.set_space(space);
        match space {
            Space::Canvas => self.open_on_canvas(target),
            Space::Notebook => crate::notebook::open(self, target),
            Space::Tasks => crate::tasks::open(self, target),
        }
    }

    /// The canvas flies to a group (selects it) or a box (views it: its
    /// running terminal, else its details; never starts it).
    fn open_on_canvas(&mut self, target: NavTarget) {
        match target {
            NavTarget::Group(id) if self.doc.group(&id).is_some() => {
                self.select_group(&id);
                self.reveal_group(&id);
            }
            NavTarget::Node(id) if self.doc.node(&id).is_some() => {
                self.reveal_node(&id);
                self.view_node(&id);
            }
            NavTarget::Group(_) | NavTarget::Node(_) => self.notify("It is no longer on the canvas.", false),
            _ => {}
        }
    }

    /// Switches the main area to `space` (no history): the keyboard leaves a
    /// terminal for the main area, details of another space close, and the
    /// window's preferences remember it.
    pub fn set_space(&mut self, space: Space) {
        self.needs_draw = true;
        if self.focus == Focus::Terminal {
            self.set_focus(Focus::Canvas);
        }
        if space == self.space {
            return;
        }
        self.space = space;
        self.side_filter_active = false;
        self.drag = None;
        let keep = match &self.right_detail {
            Some(RightDetail::Session(_)) => space == Space::Canvas,
            Some(RightDetail::Note(_)) => space == Space::Notebook,
            Some(RightDetail::Todo(_)) => space == Space::Tasks,
            _ => true,
        };
        if !keep {
            self.right_detail = None;
        }
        self.prefs_dirty_at.get_or_insert_with(std::time::Instant::now);
    }

    /// A mouse event for the space's module (its main area or sidebar).
    pub fn space_mouse(&mut self, mouse: MouseEvent) {
        match self.space {
            Space::Notebook => crate::notebook::on_mouse(self, mouse),
            Space::Tasks => crate::tasks::on_mouse(self, mouse),
            Space::Canvas => {}
        }
    }

    /// A cell the Notebook's or Tasks' module drew this frame: the main
    /// area, its sidebar (slot 1), its panel on the right.
    pub fn in_space_area(&self, x: u16, y: u16) -> bool {
        self.space != Space::Canvas && self.layout.space_areas.iter().any(|r| crate::paint::rect_contains(*r, x, y))
    }

    /// A press in the module's areas gives it the keyboard: its sidebar, or
    /// its main area (the panel on the right is the main area's).
    pub fn space_focus_at(&mut self, x: u16, y: u16) {
        if crate::paint::rect_contains(self.layout.sidebar, x, y) {
            self.set_focus(Focus::Sidebar);
        } else if self.in_space_area(x, y) {
            self.set_focus(Focus::Canvas);
        }
    }

    /// Keys the space's module did not take: the few global ones that make
    /// sense outside the canvas (quit, focus, help, sidebar, repaint).
    pub fn space_fallback_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        match key.code {
            KeyCode::Char('q' | 'Q') if ctrl => self.request_quit(),
            KeyCode::Char('q') if key.modifiers.is_empty() => self.request_quit(),
            KeyCode::F(6) => self.cycle_focus(),
            _ if crate::input::is_leave_terminal(&key) => {
                if !self.layout.tiles.is_empty() {
                    self.focused_term = self.focused_term.clone().or_else(|| self.layout.tiles.first().map(|t| t.0.clone()));
                    self.set_focus(Focus::Terminal);
                }
            }
            KeyCode::Char('?') => self.run(crate::dialogs::Action::Help),
            KeyCode::Char('l') if ctrl => {
                self.repaint_requested = true;
                self.needs_draw = true;
            }
            KeyCode::Char('b') if key.modifiers.is_empty() => {
                self.doc.ui.sidebar_visible = !self.doc.ui.sidebar_visible;
                if !self.doc.ui.sidebar_visible && self.focus == Focus::Sidebar {
                    self.set_focus(Focus::Canvas);
                }
                self.mark_dirty();
            }
            KeyCode::Tab | KeyCode::Esc if self.focus == Focus::Sidebar => self.set_focus(Focus::Canvas),
            _ => {}
        }
    }
}

/// A key in the Go to dialog: the query edits, ↑↓ PgUp PgDn move, Enter goes
/// (never starting a session), Esc closes.
pub fn goto_key(app: &mut App, goto: &mut GoTo, key: KeyEvent) -> Outcome {
    let len = goto.results.len();
    let step = |sel: usize, delta: i64| -> usize { if len == 0 { 0 } else { (sel as i64 + delta).clamp(0, len as i64 - 1) as usize } };
    match key.code {
        KeyCode::Esc => Outcome::Close,
        KeyCode::Enter => match goto.selected().cloned() {
            Some(item) => {
                // The dialog is off the stack meanwhile: the place shows without it.
                app.navigate(item.space, item.target);
                Outcome::Close
            }
            None => Outcome::Keep,
        },
        KeyCode::Up => {
            goto.sel = step(goto.sel, -1);
            Outcome::Keep
        }
        KeyCode::Down | KeyCode::Tab => {
            goto.sel = step(goto.sel, 1);
            Outcome::Keep
        }
        KeyCode::BackTab => {
            goto.sel = step(goto.sel, -1);
            Outcome::Keep
        }
        KeyCode::PageUp => {
            goto.sel = step(goto.sel, -10);
            Outcome::Keep
        }
        KeyCode::PageDown => {
            goto.sel = step(goto.sel, 10);
            Outcome::Keep
        }
        _ => {
            let before = goto.input.text.clone();
            goto.input.handle(&key);
            if goto.input.text != before {
                goto.refilter();
            }
            Outcome::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, Group, Node};

    fn app(doc: Doc) -> (App, crossbeam_channel::Receiver<crate::protocol::ClientMsg>) {
        let (tx, rx) = crossbeam_channel::bounded(256);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        (app, rx)
    }

    fn node(id: &str, name: &str, x: i32, y: i32) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y, "cwd": r"C:\work" })).unwrap()
    }

    fn doc() -> Doc {
        let mut doc = Doc::default();
        doc.groups.push(Group { id: "g-pay".into(), name: "Payments".into(), x: 0, y: 0, w: 70, h: 12, folder: Some(r"C:\work\pay".into()), ..Default::default() });
        doc.groups.push(Group { id: "g-docs".into(), name: "Docs".into(), x: 0, y: 20, w: 40, h: 8, ..Default::default() });
        doc.nodes.push(node("s-fix", "Fix flaky test", 2, 2));
        doc.nodes.push(node("s-spec", "Write the spec", 2, 22));
        doc.nodes.push(node("s-loose", "Notebook research", 80, 40));
        doc
    }

    fn key(app: &mut App, code: KeyCode, modifiers: Modifiers) {
        app.on_input(vec![crate::input::InputEvent::Key(KeyEvent::new(code, modifiers))]);
    }

    fn started(sent: &crossbeam_channel::Receiver<crate::protocol::ClientMsg>) -> usize {
        sent.try_iter().filter(|m| matches!(m, crate::protocol::ClientMsg::Launch { .. })).count()
    }

    #[test]
    fn spaces_have_single_width_glyphs_and_round_trip_their_keys() {
        for space in Space::ALL {
            assert_eq!(Space::parse(space.key()), Some(space));
            for glyph in [space.glyph(), space.side().0] {
                assert_eq!(unicode_width::UnicodeWidthStr::width(glyph), 1, "{glyph}");
            }
        }
        for glyph in ["◫", "✎", "☑", "●", "◇", "‼", "⚑", "⟳", "▸"] {
            assert_eq!(unicode_width::UnicodeWidthStr::width(glyph), 1, "{glyph}");
        }
        assert_eq!(Space::parse("garden"), None);
        assert_eq!(Space::Canvas.side(), ("▤", "SESSIONS"), "the canvas keeps its sessions sidebar");
    }

    #[test]
    fn history_goes_back_and_forward_and_drops_what_was_ahead() {
        let mut h = History::default();
        assert!(h.back().is_none() && h.forward().is_none() && h.current().is_none());
        h.push(Space::Canvas, NavTarget::None);
        h.push(Space::Notebook, NavTarget::Note("a.md".into()));
        h.push(Space::Notebook, NavTarget::Note("a.md".into()));
        assert_eq!(h.entries().len(), 2, "a consecutive duplicate collapses");
        h.push(Space::Tasks, NavTarget::Todo("DEC-031".into()));
        assert_eq!(h.back(), Some((Space::Notebook, NavTarget::Note("a.md".into()))));
        assert_eq!(h.back(), Some((Space::Canvas, NavTarget::None)));
        assert_eq!(h.back(), None, "nothing before the first");
        assert_eq!(h.forward(), Some((Space::Notebook, NavTarget::Note("a.md".into()))));
        assert!(h.can_forward());
        // A push after going back drops the entries ahead.
        h.push(Space::Canvas, NavTarget::Group("g".into()));
        assert!(!h.can_forward());
        assert_eq!(h.entries().len(), 3);
        assert_eq!(h.current(), Some(&(Space::Canvas, NavTarget::Group("g".into()))));
        // Replacing the current place changes only its target.
        h.replace_current(NavTarget::Note("a.md".into()));
        assert_eq!(h.current(), Some(&(Space::Canvas, NavTarget::Note("a.md".into()))));
        // It never leaves two equal entries in a row, and keeps the ones ahead.
        let mut h = History::default();
        h.push(Space::Canvas, NavTarget::None);
        h.push(Space::Canvas, NavTarget::Group("g".into()));
        h.push(Space::Notebook, NavTarget::None);
        h.back();
        h.replace_current(NavTarget::None);
        assert_eq!(h.entries(), [(Space::Canvas, NavTarget::None), (Space::Notebook, NavTarget::None)]);
        assert_eq!(h.current(), Some(&(Space::Canvas, NavTarget::None)));
        assert!(h.can_forward(), "the entries ahead stay");
    }

    #[test]
    fn a_place_changed_to_the_next_one_leaves_no_step_that_goes_nowhere() {
        // Two todos in a row; back to the first, then the second is selected by hand.
        let mut h = History::default();
        h.push(Space::Tasks, NavTarget::Todo("A".into()));
        h.push(Space::Tasks, NavTarget::Todo("B".into()));
        h.push(Space::Canvas, NavTarget::None);
        h.back();
        h.back();
        h.replace_current(NavTarget::Todo("B".into()));
        assert_eq!(h.entries(), [(Space::Tasks, NavTarget::Todo("B".into())), (Space::Canvas, NavTarget::None)]);
        assert_eq!(h.forward(), Some((Space::Canvas, NavTarget::None)), "forward goes somewhere else");
        // Through the app: Alt+→ after selecting the place ahead by hand says there is nothing ahead.
        let (mut app, _sent) = app(doc());
        app.select_node("s-fix", false);
        app.navigate(Space::Canvas, NavTarget::Node("s-spec".into()));
        key(&mut app, KeyCode::Left, Modifiers::ALT);
        assert_eq!(app.selection, ["s-fix"]);
        app.select_node("s-spec", false);
        key(&mut app, KeyCode::Right, Modifiers::ALT);
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("Nothing to go forward")), "{:?}", app.notice.as_ref().map(|n| n.text.clone()));
        assert_eq!(app.selection, ["s-spec"]);
    }

    #[test]
    fn history_keeps_the_newest_hundred() {
        let mut h = History::default();
        for i in 0..250 {
            h.push(Space::Notebook, NavTarget::Note(format!("{i}.md")));
        }
        assert_eq!(h.entries().len(), HISTORY_CAP);
        assert_eq!(h.current(), Some(&(Space::Notebook, NavTarget::Note("249.md".into()))));
        assert_eq!(h.entries()[0], (Space::Notebook, NavTarget::Note("150.md".into())));
        let mut steps = 0;
        while h.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY_CAP - 1);
    }

    #[test]
    fn fuzzy_matches_subsequences_and_prefers_word_starts_and_runs() {
        assert_eq!(fuzzy("", "anything"), Some((0, Vec::new())));
        assert!(fuzzy("xyz", "Notebook").is_none());
        assert!(fuzzy("notebooks", "Notebook").is_none(), "longer than the text");
        assert_eq!(fuzzy("NB", "notebook").map(|m| m.1), Some(vec![0, 4]), "case-insensitive");
        assert_eq!(fuzzy("n b", "notebook").map(|m| m.1), Some(vec![0, 4]), "spaces in the query are ignored");
        let score = |q: &str, t: &str| fuzzy(q, t).map(|m| m.0).unwrap_or(i32::MIN);
        // Word starts beat letters inside words.
        assert!(score("fl", "Fix flaky test") > score("fl", "Overflow handling"));
        assert_eq!(fuzzy("ft", "Fix flaky test").map(|m| m.1), Some(vec![0, 10]), "F of Fix, t of test (word starts)");
        // Contiguous runs beat scattered letters.
        assert!(score("spec", "Write the spec") > score("spec", "Some page elsewhere commit"));
        // An earlier match helps; camelCase and digits start words.
        assert!(score("pay", "Payments") > score("pay", "Repayments"));
        assert_eq!(fuzzy("ws", "writeSpec").map(|m| m.1), Some(vec![0, 5]));
        assert_eq!(fuzzy("d31", "DEC-031 Decoder").map(|m| m.1), Some(vec![0, 5, 6]));
    }

    #[test]
    fn go_to_lists_groups_and_sessions_best_first() {
        let (app, _sent) = app(doc());
        let items = goto_items(&app);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Payments", "Docs", "Fix flaky test", "Write the spec", "Notebook research"]);
        assert_eq!(items[0].target, NavTarget::Group("g-pay".into()));
        assert_eq!(items[0].glyph, "▣");
        assert!(items[0].detail.contains(r"C:\work\pay"));
        assert_eq!((items[2].space, items[2].target.clone()), (Space::Canvas, NavTarget::Node("s-fix".into())));
        assert!(items[2].detail.contains("Payments"), "a session names its group: {}", items[2].detail);
        let mut goto = GoTo::new(items);
        assert_eq!(goto.results.len(), 5, "everything while the query is empty");
        goto.input.set("s");
        goto.refilter();
        let order: Vec<&str> = goto.results.iter().map(|(i, _)| goto.items[*i].label.as_str()).collect();
        assert_eq!(order[0], "Write the spec", "a word start beats letters inside words: {order:?}");
        goto.input.set("pay");
        goto.refilter();
        let first = goto.selected().unwrap();
        assert_eq!(first.label, "Payments");
        assert_eq!(goto.results[0].1, vec![0, 1, 2]);
        // A session found by its group (the query in its detail) comes after label matches.
        let fix = goto.results.iter().position(|(i, positions)| goto.items[*i].label == "Fix flaky test" && positions.is_empty());
        assert_eq!(fix, Some(goto.results.len() - 1), "{:?}", goto.results);
        goto.input.set("fix fl");
        goto.refilter();
        assert_eq!(goto.selected().map(|i| i.label.as_str()), Some("Fix flaky test"));
        goto.input.set("zzz");
        goto.refilter();
        assert!(goto.results.is_empty() && goto.selected().is_none());
    }

    #[test]
    fn ctrl_p_opens_go_to_and_enter_jumps_without_starting_anything() {
        let (mut app, sent) = app(doc());
        key(&mut app, KeyCode::Char('p'), Modifiers::CONTROL);
        assert!(matches!(app.modals.last(), Some(Modal::GoTo(_))));
        for c in "spec".chars() {
            key(&mut app, KeyCode::Char(c), Modifiers::NONE);
        }
        let Some(Modal::GoTo(goto)) = app.modals.last() else { panic!("Go to") };
        assert_eq!(goto.selected().map(|i| i.label.as_str()), Some("Write the spec"));
        key(&mut app, KeyCode::Enter, Modifiers::NONE);
        assert!(app.modals.is_empty());
        assert_eq!((app.space, app.selection.clone()), (Space::Canvas, vec!["s-spec".to_owned()]));
        assert_eq!(started(&sent), 0, "Go to never starts a session");
        // To a group from another space: back on the canvas, the group selected.
        app.switch_space(Space::Tasks);
        key(&mut app, KeyCode::Char('p'), Modifiers::CONTROL);
        for c in "docs".chars() {
            key(&mut app, KeyCode::Char(c), Modifiers::NONE);
        }
        key(&mut app, KeyCode::Enter, Modifiers::NONE);
        assert_eq!((app.space, app.selected_group.as_deref()), (Space::Canvas, Some("g-docs")));
        // Esc closes it; the console's Ctrl+P control character opens it too.
        key(&mut app, KeyCode::Char('\x10'), Modifiers::NONE);
        assert!(matches!(app.modals.last(), Some(Modal::GoTo(_))));
        key(&mut app, KeyCode::Esc, Modifiers::NONE);
        assert!(app.modals.is_empty());
    }

    #[test]
    fn ctrl_digits_switch_spaces_and_alt_arrows_walk_the_history() {
        let (mut app, sent) = app(doc());
        app.select_node("s-fix", false);
        key(&mut app, KeyCode::Char('2'), Modifiers::CONTROL);
        assert_eq!(app.space, Space::Notebook);
        key(&mut app, KeyCode::Char('3'), Modifiers::CONTROL);
        assert_eq!(app.space, Space::Tasks);
        key(&mut app, KeyCode::Char('3'), Modifiers::CONTROL);
        assert_eq!(app.nav.entries().len(), 3, "the same space again is no jump: {:?}", app.nav.entries());
        key(&mut app, KeyCode::Left, Modifiers::ALT);
        assert_eq!(app.space, Space::Notebook);
        key(&mut app, KeyCode::Left, Modifiers::ALT);
        assert_eq!((app.space, app.selection.clone()), (Space::Canvas, vec!["s-fix".to_owned()]), "back to the canvas at the box it showed");
        key(&mut app, KeyCode::Left, Modifiers::ALT);
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("Nothing to go back")));
        key(&mut app, KeyCode::Right, Modifiers::ALT);
        key(&mut app, KeyCode::Right, Modifiers::ALT);
        assert_eq!(app.space, Space::Tasks);
        key(&mut app, KeyCode::Char('1'), Modifiers::CONTROL);
        assert_eq!(app.space, Space::Canvas);
        // AltGr (Ctrl+Alt) digits are not spaces.
        key(&mut app, KeyCode::Char('2'), Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(app.space, Space::Canvas);
        assert_eq!(started(&sent), 0);
    }

    #[test]
    fn alt_comma_and_period_and_the_mouse_buttons_walk_the_history_too() {
        let (mut app, sent) = app(doc());
        key(&mut app, KeyCode::Char('2'), Modifiers::CONTROL);
        key(&mut app, KeyCode::Char('3'), Modifiers::CONTROL);
        // Windows Terminal keeps Alt+arrows: Alt+, and Alt+. are back and forward.
        key(&mut app, KeyCode::Char(','), Modifiers::ALT);
        assert_eq!(app.space, Space::Notebook);
        key(&mut app, KeyCode::Char(','), Modifiers::ALT);
        assert_eq!(app.space, Space::Canvas);
        key(&mut app, KeyCode::Char('.'), Modifiers::ALT);
        assert_eq!(app.space, Space::Notebook);
        // Not without Alt, nor with AltGr (typing).
        key(&mut app, KeyCode::Char('.'), Modifiers::CONTROL | Modifiers::ALT);
        key(&mut app, KeyCode::Char(','), Modifiers::NONE);
        assert_eq!(app.space, Space::Notebook);
        // The GUI's mouse buttons, even from a terminal; never over a dialog.
        app.nav_button(true);
        assert_eq!(app.space, Space::Tasks);
        app.focus = Focus::Terminal;
        app.nav_button(false);
        assert_eq!(app.space, Space::Notebook);
        app.modals.push(Modal::Help);
        app.nav_button(false);
        assert_eq!((app.space, app.modals.len()), (Space::Notebook, 1));
        assert_eq!(started(&sent), 0);
    }

    #[test]
    fn terminals_keep_the_space_keys() {
        let (mut app, sent) = app(doc());
        app.terms.insert("s-fix".into(), crate::protocol::TermInfo { id: "s-fix".into(), alive: true, ..Default::default() });
        app.focused_term = Some("s-fix".into());
        app.focus = Focus::Terminal;
        let _ = sent.try_iter().count();
        key(&mut app, KeyCode::Char('2'), Modifiers::CONTROL);
        key(&mut app, KeyCode::Left, Modifiers::ALT);
        key(&mut app, KeyCode::Right, Modifiers::ALT);
        key(&mut app, KeyCode::Char('p'), Modifiers::CONTROL);
        assert_eq!(app.space, Space::Canvas);
        assert!(app.modals.is_empty());
        let keys: Vec<crate::protocol::Input> = sent
            .try_iter()
            .filter_map(|m| match m {
                crate::protocol::ClientMsg::Input { input, .. } => Some(input),
                _ => None,
            })
            .collect();
        use crate::protocol::{Input, Key, MOD_ALT, MOD_CTRL};
        assert_eq!(
            keys,
            [
                Input::Key { key: Key::Char('2'), mods: MOD_CTRL },
                Input::Key { key: Key::Left, mods: MOD_ALT },
                Input::Key { key: Key::Right, mods: MOD_ALT },
                Input::Key { key: Key::Char('p'), mods: MOD_CTRL }
            ],
            "the agent gets them"
        );
    }

    #[test]
    fn the_modules_get_the_main_area_and_canvas_keys_stay_on_the_canvas() {
        let (mut app, sent) = app(doc());
        app.select_node("s-fix", false);
        app.recent.walk = None;
        app.switch_space(Space::Notebook);
        assert_eq!(app.focus, Focus::Canvas);
        assert!(app.space_has_keys());
        let before = (app.doc.nodes.clone(), app.doc.view.clone());
        for code in [KeyCode::Delete, KeyCode::Right, KeyCode::Char(']'), KeyCode::Char('n'), KeyCode::Enter, KeyCode::Char('x'), KeyCode::Char('f')] {
            key(&mut app, code, Modifiers::NONE);
        }
        assert_eq!((app.doc.nodes.clone(), app.doc.view.clone()), before, "nothing on the canvas moved, went or started");
        assert!(app.modals.is_empty() && app.recent.walk.is_none(), "no dialog, no recent walk");
        assert_eq!(started(&sent), 0);
        // Help and focus keys still work there.
        key(&mut app, KeyCode::Char('?'), Modifiers::NONE);
        assert!(matches!(app.modals.last(), Some(Modal::Help)));
        app.modals.clear();
        // The sidebar's slot 1 is the module's too; other views keep their lists.
        app.set_view(SideView::Sessions);
        assert!(app.space_has_keys());
        app.set_view(SideView::Settings);
        assert!(!app.space_has_keys());
        app.set_focus(Focus::Canvas);
        app.switch_space(Space::Canvas);
        assert!(!app.space_has_keys());
    }

    #[test]
    fn switching_spaces_closes_details_of_the_other_spaces() {
        let (mut app, _sent) = app(doc());
        app.view_node("s-fix");
        assert_eq!(app.right_detail, Some(RightDetail::Session("s-fix".into())));
        app.switch_space(Space::Notebook);
        assert_eq!(app.right_detail, None, "a session's details are the canvas's");
        app.right_detail = Some(RightDetail::Note(r"C:\notes\a.md".into()));
        app.switch_space(Space::Tasks);
        assert_eq!(app.right_detail, None);
        app.right_detail = Some(RightDetail::Prs);
        app.switch_space(Space::Canvas);
        assert_eq!(app.right_detail, Some(RightDetail::Prs), "the pull requests panel is everyone's");
    }

    fn render(app: &mut App, width: u16) -> ratatui::buffer::Buffer {
        let area = ratatui::layout::Rect::new(0, 0, width, 40);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        crate::ui::render(app, &mut buf, area, None);
        buf
    }

    fn text(buf: &ratatui::buffer::Buffer, r: ratatui::layout::Rect) -> String {
        (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect()
    }

    fn mouse(app: &mut App, kind: crate::input::MouseEventKind, (x, y): (u16, u16)) {
        app.on_input(vec![crate::input::InputEvent::Mouse(MouseEvent::at_cell(kind, x, y, Modifiers::NONE))]);
    }

    fn click(app: &mut App, at: (u16, u16)) {
        use crate::input::{MouseButton, MouseEventKind};
        mouse(app, MouseEventKind::Down(MouseButton::Left), at);
        mouse(app, MouseEventKind::Up(MouseButton::Left), at);
    }

    #[test]
    fn a_macs_status_bar_names_its_keys_in_every_space() {
        let (mut app, _) = app(doc());
        app.key_policy = crate::input::KeyPolicy::CommandApp;
        app.render_mode = crate::view::RenderMode::Gpu;
        for space in [Space::Canvas, Space::Notebook, Space::Tasks] {
            app.switch_space(space);
            app.notice = None;
            let buf = render(&mut app, 240);
            let status = text(&buf, ratatui::layout::Rect::new(0, 39, 240, 1));
            assert!(!status.contains("Alt+") && !status.contains("Ctrl+1") && !status.contains("Ctrl+P"), "{space:?}: {status}");
            if space != Space::Canvas {
                assert!(status.contains("⌘1 Canvas · ⌘2 Notebook · ⌘3 Tasks · ⌘[ back · ⌘] forward · ⌘P go to"), "{space:?}: {status}");
            }
        }
    }

    #[test]
    fn the_toolbar_has_space_tabs_full_when_there_is_room() {
        use crate::dialogs::{Action, Hit};
        let (mut app, sent) = app(doc());
        let tabs = |app: &App| -> Vec<(ratatui::layout::Rect, Space)> {
            app.layout.hits.iter().filter_map(|(r, h)| if let Hit::Toolbar(Action::Space(s)) = h { Some((*r, *s)) } else { None }).collect()
        };
        let buf = render(&mut app, 200);
        let wide = tabs(&app);
        assert_eq!(wide.iter().map(|(_, s)| *s).collect::<Vec<_>>(), Space::ALL);
        assert_eq!(wide.iter().map(|(r, _)| text(&buf, *r)).collect::<Vec<_>>(), [" ◫ Canvas ", " ✎ Notebook ", " ☑ Tasks "]);
        assert!(wide.iter().all(|(r, _)| r.y == 0), "on the toolbar");
        let buf = render(&mut app, 150);
        let narrow = tabs(&app);
        assert_eq!(narrow.iter().map(|(r, _)| text(&buf, *r)).collect::<Vec<_>>(), [" ◫ ", " ✎ ", " ☑ "], "compact when narrow");
        assert!(crate::ui::compact_tabs(&app, 150) && !crate::ui::compact_tabs(&app, 200));
        // A click on a tab switches; the canvas's fitting and zoom buttons stay the canvas's.
        let (tasks, _) = narrow[2];
        click(&mut app, (tasks.x + 1, tasks.y));
        assert_eq!(app.space, Space::Tasks);
        let buf = render(&mut app, 150);
        let toolbar = text(&buf, ratatui::layout::Rect::new(0, 0, 150, 1));
        assert!(!toolbar.contains("Show all nodes") && toolbar.contains("+ New"), "{toolbar}");
        assert!(!app.layout.hits.iter().any(|(_, h)| matches!(h, Hit::Toolbar(Action::FitAll | Action::Zoom(_)))));
        assert_eq!(text(&buf, ratatui::layout::Rect::new(1, 2, 1, 1)), "☑", "slot 1 of the activity bar is the space's");
        let (canvas, _) = tabs(&app)[0];
        click(&mut app, (canvas.x, canvas.y));
        assert_eq!(app.space, Space::Canvas);
        let buf = render(&mut app, 150);
        assert_eq!(text(&buf, ratatui::layout::Rect::new(1, 2, 1, 1)), "▤");
        assert_eq!(started(&sent), 0);
    }

    #[test]
    fn the_main_area_and_sidebar_are_the_modules_and_the_right_pane_its_panel() {
        let (mut app, sent) = app(doc());
        app.switch_space(Space::Notebook);
        let buf = render(&mut app, 150);
        let main = app.layout.canvas;
        assert!(app.layout.space_areas.contains(&main) && app.layout.space_areas.contains(&app.layout.sidebar));
        assert!(app.layout.terms.width > 0 && app.layout.space_areas.contains(&app.layout.terms), "the info panel on the right");
        assert!(text(&buf, ratatui::layout::Rect::new(main.x, main.y + 3, main.width, 1)).contains(crate::notebook::EMPTY));
        // A press there is the module's (its drag and release too) and gives it the keyboard.
        app.set_focus(Focus::Sidebar);
        mouse(&mut app, crate::input::MouseEventKind::Down(crate::input::MouseButton::Left), (main.x + 20, main.y + 10));
        assert_eq!((app.focus, matches!(app.drag, Some(crate::app::Drag::Space))), (Focus::Canvas, true));
        mouse(&mut app, crate::input::MouseEventKind::Drag(crate::input::MouseButton::Left), (main.x + 25, main.y + 12));
        mouse(&mut app, crate::input::MouseEventKind::Up(crate::input::MouseButton::Left), (main.x + 25, main.y + 12));
        assert!(app.drag.is_none());
        let sidebar = app.layout.sidebar;
        click(&mut app, (sidebar.x + 3, sidebar.y + 8));
        assert_eq!(app.focus, Focus::Sidebar);
        // Its own click targets reach it.
        click(&mut app, (main.x + 3, main.y + 3));
        assert!(app.notice.as_ref().is_some_and(|n| n.text == crate::notebook::EMPTY_HINT));
        assert_eq!(app.focus, Focus::Canvas);
        // No plain-text links are looked for in its areas.
        assert!(app.layout.links.iter().all(|l| l.rects.iter().all(|r| !app.layout.space_areas.iter().any(|a| crate::paint::rect_contains(*a, r.x, r.y)))));
        // A running session's terminal shows there only while you are in it, or tiled.
        app.terms.insert("s-fix".into(), crate::protocol::TermInfo { id: "s-fix".into(), alive: true, ..Default::default() });
        app.focused_term = Some("s-fix".into());
        app.selection = vec!["s-fix".into()];
        assert!(app.visible_terminals().is_empty());
        app.focus = Focus::Terminal;
        assert_eq!(app.visible_terminals(), ["s-fix"]);
        app.focus = Focus::Canvas;
        app.doc.ui.tiled = true;
        app.tile_slots = vec!["s-fix".into()];
        assert_eq!(app.visible_terminals(), ["s-fix"]);
        render(&mut app, 150);
        assert!(!app.layout.space_areas.contains(&app.layout.terms) && app.layout.tiles.len() == 1, "the tile, not the panel");
        assert_eq!(started(&sent), 0);
    }

    #[test]
    fn help_shows_each_spaces_keys_in_the_canvas_helps_room() {
        use crate::paint::text_width;
        use crate::ui_modals::help_rows;
        let canvas = help_rows(Space::Canvas);
        assert!(canvas.iter().any(|(k, _)| k.contains("Alt+, / Alt+.")) && canvas.iter().any(|(k, _)| k.contains("Alt+N")), "the canvas's help names the space keys");
        for space in [Space::Notebook, Space::Tasks] {
            let rows = help_rows(space);
            assert!(rows.len() <= canvas.len(), "{space:?}: {} rows", rows.len());
            for (key, text) in &rows {
                // Keys from column 3 to 37, the text from 38 to the frame (78 wide).
                assert!(if text.is_empty() { text_width(key) <= 74 } else { text_width(key) <= 34 && text_width(text) <= 39 }, "{key:?} {text:?} fits");
            }
            let keys: String = rows.iter().map(|(k, t)| format!("{k} {t}\n")).collect();
            for wanted in ["Ctrl+1/2/3", "Alt+←/→ or Alt+, / Alt+.", "mouse back / forward", "Ctrl+P", "Alt+N", "Ctrl+Q"] {
                assert!(keys.contains(wanted), "{space:?} help names {wanted}");
            }
        }
        let notebook: String = help_rows(Space::Notebook).iter().map(|(k, _)| format!("{k}\n")).collect();
        for wanted in ["Alt+H", "Ctrl+E", "Ctrl+S", "Ctrl+F", "Ctrl+W", "Ctrl+Tab", "Ctrl+PgDn", "Ctrl+N", "F2", "Del"] {
            assert!(notebook.contains(wanted), "the Notebook's help names {wanted}");
        }
        let tasks: String = help_rows(Space::Tasks).iter().map(|(k, _)| format!("{k}\n")).collect();
        for wanted in ["arrows", "Enter", "Space", "N\n", "A / D", "E · R · Del", "+ / -", "F\n", "/ · t"] {
            assert!(tasks.contains(wanted), "the Tasks help names {wanted:?}");
        }
        // `?` there shows them.
        let (mut app, _sent) = app(doc());
        app.switch_space(Space::Tasks);
        key(&mut app, KeyCode::Char('?'), Modifiers::NONE);
        let buf = render(&mut app, 150);
        let rect = app.layout.modal_rect.expect("the help");
        let shown: String = (rect.y..rect.bottom()).map(|y| text(&buf, ratatui::layout::Rect::new(rect.x, y, rect.width, 1)) + "\n").collect();
        assert!(shown.contains("help · Tasks") && shown.contains("accept / decline a proposal") && shown.contains("Alt+←/→ or Alt+, / Alt+."), "{shown}");
    }

    #[test]
    fn module_dialogs_draw_and_close_through_the_module() {
        use crate::dialogs::Hit;
        let (mut app, _sent) = app(doc());
        app.switch_space(Space::Tasks);
        app.modals.push(Modal::Tasks(Box::new(crate::tasks::Dialog::Info { title: "Board".into(), lines: vec!["Line one".into()] })));
        let buf = render(&mut app, 150);
        let rect = app.layout.modal_rect.expect("drawn");
        assert!(text(&buf, ratatui::layout::Rect::new(rect.x, rect.y + 2, rect.width, 1)).contains("Line one"));
        let ok = app.layout.hits.iter().find(|(_, h)| *h == Hit::Button(0)).map(|(r, _)| *r).expect("its button");
        click(&mut app, (ok.x + 1, ok.y));
        assert!(app.modals.is_empty(), "the module closed it");
        app.modals.push(Modal::Notebook(Box::new(crate::notebook::Dialog::Info { title: "Note".into(), lines: Vec::new() })));
        render(&mut app, 150);
        key(&mut app, KeyCode::Char('2'), Modifiers::CONTROL);
        assert!(matches!(app.modals.last(), Some(Modal::Notebook(_))) && app.space == Space::Tasks, "a dialog keeps its keys");
        key(&mut app, KeyCode::Esc, Modifiers::NONE);
        assert!(app.modals.is_empty());
    }

    #[test]
    fn the_space_is_kept_in_the_windows_preferences() {
        let home = std::env::temp_dir().join(format!("cc-spaces-{}", uuid::Uuid::new_v4()));
        let path = crate::prefs::PrefsFile::path(&home, "default", "canvas-1");
        let open = || {
            let (tx, _rx) = crossbeam_channel::bounded(16);
            let (_service, service_rx) = crossbeam_channel::bounded(1);
            let (app_tx, _app_rx) = crossbeam_channel::unbounded();
            App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc()), false, ".".into(), app_tx, Some(path.clone()))
        };
        let mut app = open();
        assert_eq!(app.space, Space::Canvas);
        app.switch_space(Space::Tasks);
        assert!(app.prefs_dirty_at.is_some());
        app.save_prefs();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"space\": \"tasks\""), "{text}");
        assert_eq!(open().space, Space::Tasks);
        app.switch_space(Space::Canvas);
        app.save_prefs();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("space"), "the canvas is the default and not written");
        assert_eq!(open().space, Space::Canvas);
        let _ = std::fs::remove_dir_all(home);
    }
}
