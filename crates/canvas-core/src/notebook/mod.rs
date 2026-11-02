//! The Notebook space (docs/NOTEBOOK-TASKS.md, "Notebook"): its explorer,
//! reader, editor and info panel.
//!
//! The spaces foundation calls the entry points below (their dispatch lives
//! in `app.rs`, `events.rs`, `actions.rs`, `modals.rs`, `ui.rs`, `ui_side.rs`
//! and `ui_modals.rs`); the work is in the submodules:
//!
//! - `tree`: the explorer's roots (the notebook root, group folders outside
//!   it, the canvas-wide folder, mounts, the virtual Reports section) and its
//!   rows (filters, hidden counts), pure functions of [`State`].
//! - `load`: everything that asks the backend (on background threads; the
//!   answers continue on the UI thread through `AppEvent::Run`): the
//!   notebook's folders, listings, reading, saving, change polling, searches.
//! - `explorer` (the sidebar), `reader` (drawing the tabs, header, rendered
//!   view, editor, banner, find bar and `[[` completion), `tabs` (open notes:
//!   loading, saving, autosave, conflicts, find, completion, links, the
//!   reader's keys), `info` (the right pane), `dialogs` (the module's
//!   dialogs) and `ops` (file operations, quick capture, *Ask agent to
//!   revise…*, the scripts switch, settings).
//!
//! Keys of this space: `Alt+H` cycles the Human | All | AI filter (works in
//! terminals, where Ctrl+Shift+H is not told apart), `Alt+N` is quick capture
//! anywhere outside terminals (`events.rs` asks [`is_capture_key`]; the GUI
//! keeps Ctrl+Shift+N for a new window), `Ctrl+E` Rendered ⇄ Source,
//! `Ctrl+W` closes a tab, `Ctrl+Tab` / `Ctrl+Shift+Tab` (also `Ctrl+PgDn` /
//! `Ctrl+PgUp`) switch tabs, `Ctrl+S` saves, `Ctrl+F` finds (`F3` / `Enter`
//! next, `Shift+F3` / `Shift+Enter` previous), `Ctrl+N` a new note in the
//! explorer's folder, Ctrl+click opens the `[[link]]` or todo id under the
//! caret; in the explorer `F2` renames, `Del` trashes, `/` filters names.

mod dialogs;
mod explorer;
mod info;
mod load;
mod ops;
mod reader;
mod tabs;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_saving;
mod tree;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::{App, Focus};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseEvent};
use crate::modals::Outcome;
use crate::spaces::{GoItem, NavTarget, Space};
use crate::ui_modals::Ctx;

pub use canvas_model::notebook::{Entry, Info, Kind, NoteText, Place, Provenance};
pub use dialogs::{Dialog, NamePurpose};
pub use tabs::{Find, LinkAt, Tab, View};
pub use tree::{Filter, Root, RootKind, Row, RowKind};

/// How often expanded folders are listed again while the Notebook shows.
pub const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the open notes' revisions are polled.
pub const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// Autosave: this long after the last change.
pub const AUTOSAVE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
/// The known-notes list (Go to, `[[` completion) is walked again this often.
pub const KNOWN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// Most notes the known-notes list keeps.
pub const KNOWN_MAX: usize = 2_000;

/// A background request's state.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Load {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

/// One listed folder (`NotebookList`).
#[derive(Clone, Debug, Default)]
pub struct Folder {
    /// The folder's path as asked for.
    pub path: String,
    pub entries: Vec<Entry>,
    /// The explorer root it was listed under.
    pub root: String,
    pub loading: bool,
    pub error: Option<String>,
    /// When the last listing arrived (`None`: never).
    pub at: Option<Instant>,
}

/// A note the window knows of (listings, opened notes): Go to and `[[` completion.
#[derive(Clone, Debug, PartialEq)]
pub struct Known {
    pub path: String,
    pub name: String,
    pub title: String,
    pub ai: bool,
}

/// Backlinks of one note (`NoteSearch` for `[[stem`).
#[derive(Clone, Debug, Default)]
pub struct Backlinks {
    pub hits: Vec<canvas_model::notebook::Hit>,
    pub loading: bool,
    pub at: Option<Instant>,
}

/// `RightDetail::Note(path)` for a note that is not open: its text, loaded once.
#[derive(Clone, Debug)]
pub struct Detail {
    pub path: String,
    pub text: Option<Result<NoteText, String>>,
}

/// `[[` completion: the typed prefix and its matching note names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    pub prefix: String,
    pub items: Vec<String>,
    pub sel: usize,
    /// The name search that ran for this `[[` (its query; longer prefixes
    /// containing it need no new one).
    pub searched: String,
}

/// This window's notebook (`App::notebook`).
#[derive(Debug, Default)]
pub struct State {
    /// The place last opened.
    pub target: Option<NavTarget>,
    /// The resolved folders (`NotebookInfo`).
    pub info: Option<Info>,
    pub info_load: Load,
    /// The settings `info` was resolved for: (notebook root, canvas id).
    info_key: Option<(Option<String>, Option<String>)>,
    info_retry_at: Option<Instant>,
    /// Listed folders by path.
    pub folders: HashMap<String, Folder>,
    /// Expanded folders and roots (paths; `REPORTS` for the Reports section).
    pub expanded: HashSet<String>,
    pub filter: Filter,
    pub show_internal: bool,
    /// The explorer's `/` name filter and whether it has the keyboard.
    pub name_filter: crate::dialogs::TextInput,
    pub filtering: bool,
    /// The selected explorer row (its key).
    pub selected: Option<String>,
    pub scroll: usize,
    /// The next draw scrolls the selected row into view.
    follow: bool,
    /// A place to reveal once the notebook's folders are known.
    pending_reveal: Option<(String, bool)>,
    /// Quick capture waits for the notebook's folders.
    pending_capture: bool,
    /// The notebook root was expanded once (the first time it was known).
    root_opened: bool,
    /// Rows of the explorer as last drawn (visible height).
    side_rows: usize,
    /// Open notes and the shown one.
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// Closed tabs whose last changes are being written.
    pub closing: Vec<Tab>,
    pub find: Option<Find>,
    pub completion: Option<Completion>,
    pub backlinks: HashMap<String, Backlinks>,
    pub known: Vec<Known>,
    known_at: Option<Instant>,
    known_loading: bool,
    poll_at: Option<Instant>,
    polling: bool,
    refresh_at: Option<Instant>,
    pub detail: Option<Detail>,
    /// The last press in the reader's content (double-clicks).
    last_click: Option<(Instant, u16, u16)>,
    /// A press in the reader's content: its drag selects.
    dragging: bool,
    /// The reader's content area as last drawn (mouse mapping).
    content: Rect,
    /// The GUI's HTML pane shows the note (published at the last draw).
    pub pane: bool,
    /// The info panel's first line.
    pub info_scroll: usize,
    /// The info panel's reading of the note it shows (by path and version).
    info_cache: Option<(String, std::sync::Arc<info::Analysis>)>,
}

/// The key of the virtual Reports section in [`State::expanded`].
pub const REPORTS: &str = "reports:";

impl State {
    /// The shown tab.
    pub fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// The tab of a note (path compared like the service does).
    pub fn tab_index(&self, path: &str) -> Option<usize> {
        self.tabs.iter().position(|t| same_path(&t.path, path))
    }

    /// A tab by its id: open, or closed while its last changes are written.
    pub fn tab_by_id(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().chain(self.closing.iter_mut()).find(|t| t.id == id)
    }
}

/// A settings row of the Settings view (`side::ItemRef::Notebook`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingRow {
    /// The notebook root (empty: `<home>/notebook`).
    Folder,
    /// The extra folders the explorer shows.
    Mounts,
}

/// Notebook commands (`dialogs::Action::Notebook`): menus, chips, buttons.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// The notes of a group (id): its folder in the notebook.
    OpenGroup(String),
    /// A note or folder (path on the service's machine).
    OpenPath(String),
    /// Choose (or change) a group's notes folder (group id).
    LinkFolder(String),
    /// Asks a name, then creates a note in this folder.
    NewNote(String),
    /// Asks a name, then creates a folder in this one.
    NewFolder(String),
    /// Asks the new name of a note or folder.
    Rename(String),
    /// Asks where to move a note or folder.
    Move(String),
    /// Asks, then moves a note or folder to its root's trash.
    ConfirmTrash(String),
    Trash(String),
    CopyPath(String),
    /// Its default program (a folder: the file manager); remote: VS Code or the viewer.
    OpenExternally(String),
    /// Writes `reviewed: true` into a note.
    MarkReviewed(String),
    /// The group (id) on the canvas.
    ShowGroup(String),
    /// A new note in `<root>/Inbox`, opened in Source mode.
    Capture,
    /// A Settings row (`side::ItemRef::Notebook`).
    Setting(SettingRow),
    /// Lets an HTML note (path) run its scripts, or stops them.
    Scripts(String, bool),
}

/// Click targets the module registers (`dialogs::Hit::Notebook`).
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    /// An explorer row (its key).
    Row(String),
    /// The Human | All | AI segment.
    Filter(Filter),
    /// *Show internal files*.
    Internal,
    /// `+ Capture`.
    Capture,
    /// `+ Note` (in the selected folder).
    NewNote,
    /// `⟳` lists the expanded folders again.
    Refresh,
    /// The `/` name filter field.
    NameFilter,
    /// Ask for the notebook's folders again.
    Retry,
    Tab(usize),
    CloseTab(usize),
    Mode(View),
    /// *View session* (node id): views it on the canvas, never starts it.
    ViewSession(String),
    MarkReviewed,
    Print,
    OpenExternally,
    AskAgent,
    /// The HTML note's scripts switch.
    Scripts,
    Compare,
    KeepMine,
    TakeTheirs,
    FindPrev,
    FindNext,
    FindClose,
    /// A link in the rendered note (cells).
    Link(String),
    /// An outline heading (line index of the note).
    Outline(usize),
    /// A link of the info panel to another space or place.
    Go(Space, NavTarget),
    /// A note (path or `[[name]]`).
    OpenNote(String),
    /// A `[[` completion item.
    Completion(usize),
    /// A row of a dialog's list.
    Pick(usize),
    /// The reader without a note: says how to open one.
    Empty,
}

/// What the reader says when no note is open.
pub const EMPTY: &str = "No note is open.";
/// What a click on [`EMPTY`] says.
pub const EMPTY_HINT: &str = "Pick a note in the explorer, Ctrl+P to find one, or Alt+N for a quick note.";

/// Runs a command. Opening goes through `app.navigate`, which records the
/// history and calls [`open`].
pub fn run(app: &mut App, action: Action) {
    app.needs_draw = true;
    match action {
        Action::OpenGroup(group) => app.navigate(Space::Notebook, NavTarget::Group(group)),
        Action::OpenPath(path) => app.navigate(Space::Notebook, NavTarget::Note(path)),
        Action::LinkFolder(group) => app.ask_group_folder(&group),
        Action::NewNote(folder) => dialogs::ask_name(app, NamePurpose::NewNote { folder }),
        Action::NewFolder(folder) => dialogs::ask_name(app, NamePurpose::NewFolder { folder }),
        Action::Rename(path) => dialogs::ask_name(app, NamePurpose::Rename { path }),
        Action::Move(path) => dialogs::ask_move(app, path),
        Action::ConfirmTrash(path) => ops::confirm_trash(app, &path),
        Action::Trash(path) => ops::trash(app, path),
        Action::CopyPath(path) => app.copy_to_clipboard(&path),
        Action::OpenExternally(path) => ops::open_externally(app, &path),
        Action::MarkReviewed(path) => ops::mark_reviewed(app, path),
        Action::ShowGroup(group) => app.navigate(Space::Canvas, NavTarget::Group(group)),
        Action::Capture => ops::capture(app),
        Action::Setting(row) => dialogs::setting(app, row),
        Action::Scripts(path, on) => ops::set_scripts(app, &path, on),
    }
}

/// A press on one of the module's hits (`mouse`: the press, with its button).
pub fn click(app: &mut App, hit: Hit, mouse: MouseEvent) {
    app.needs_draw = true;
    match hit {
        Hit::Row(key) => explorer::click_row(app, key, mouse),
        Hit::Filter(filter) => explorer::set_filter(app, filter),
        Hit::Internal => {
            app.notebook.show_internal = !app.notebook.show_internal;
            explorer::keep_selection_visible(app);
        }
        Hit::Capture => ops::capture(app),
        Hit::NewNote => explorer::new_note_here(app),
        Hit::Refresh => load::refresh_all(app),
        Hit::NameFilter => app.notebook.filtering = true,
        Hit::Retry => load::retry_info(app),
        Hit::Empty => app.notify(app.key_policy.label(EMPTY_HINT).into_owned(), false),
        Hit::Tab(index) if mouse.kind == crate::input::MouseEventKind::Down(crate::input::MouseButton::Middle) => tabs::close_tab(app, index),
        Hit::Tab(index) => tabs::show_tab(app, index),
        Hit::CloseTab(index) => tabs::close_tab(app, index),
        Hit::Mode(view) => tabs::set_view(app, view),
        Hit::ViewSession(node) => app.navigate(Space::Canvas, NavTarget::Node(node)),
        Hit::MarkReviewed => {
            if let Some(path) = app.notebook.tab().map(|t| t.path.clone()) {
                ops::mark_reviewed(app, path);
            }
        }
        Hit::Print => app.html_print = true,
        Hit::OpenExternally => {
            if let Some(path) = app.notebook.tab().map(|t| t.path.clone()) {
                ops::open_externally(app, &path);
            }
        }
        Hit::AskAgent => dialogs::ask_agent(app),
        Hit::Scripts => {
            if let Some(path) = app.notebook.tab().map(|t| t.path.clone()) {
                let on = !app.doc.notebook_scripts.iter().any(|p| same_path(p, &path));
                ops::set_scripts(app, &path, on);
            }
        }
        Hit::Compare => dialogs::compare(app),
        Hit::KeepMine => {
            if let Some(path) = app.notebook.tab().map(|t| t.path.clone()) {
                tabs::keep_mine(app, &path);
            }
        }
        Hit::TakeTheirs => {
            if let Some(path) = app.notebook.tab().map(|t| t.path.clone()) {
                tabs::take_theirs(app, &path);
            }
        }
        Hit::FindPrev => tabs::find_step(app, false),
        Hit::FindNext => tabs::find_step(app, true),
        Hit::FindClose => tabs::close_find(app),
        Hit::Link(href) => tabs::follow_link(app, &href),
        Hit::Outline(line) => tabs::jump_to_line(app, line),
        Hit::Go(space, target) => app.navigate(space, target),
        Hit::OpenNote(path) => app.navigate(Space::Notebook, NavTarget::Note(path)),
        Hit::Completion(index) => tabs::complete(app, Some(index)),
        // Dialog rows come to `dialog_mouse`.
        Hit::Pick(_) => {}
    }
}

/// Shows a place in the Notebook (the space is already switched; no history).
pub fn open(app: &mut App, target: NavTarget) {
    app.needs_draw = true;
    load::ensure_info(app);
    if target != NavTarget::None {
        app.notebook.target = Some(target.clone());
    }
    match target {
        NavTarget::Group(id) => match app.doc.group(&id).map(|g| (g.name.clone(), g.folder.clone())) {
            Some((_, Some(folder))) => explorer::reveal(app, &folder, true),
            Some((name, None)) => app.notify(format!("“{name}” has no notes folder yet: ✎ Set folder… on its title chooses one."), false),
            None => app.notify("That group is no longer on the canvas.", false),
        },
        NavTarget::Folder(path) => explorer::reveal(app, &path, true),
        // A known folder, or a path without an extension not listed yet.
        NavTarget::Note(path) if is_absolute(&path) && (explorer::is_folder(app, &path) || !file_name(&path).contains('.')) => explorer::reveal(app, &path, true),
        NavTarget::Note(path) if is_absolute(&path) => tabs::open_note(app, &path),
        NavTarget::Note(name) => tabs::open_name(app, &name),
        _ => {}
    }
}

/// The sidebar (slot 1 of the activity bar: ✎ EXPLORER).
pub fn draw_side(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    load::ensure_info(app);
    explorer::draw(app, buf, area)
}

/// The main area: the reader and editor.
pub fn draw_main(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    load::ensure_info(app);
    reader::draw(app, buf, area)
}

/// The right pane when no terminals or details take it: outline, links,
/// backlinks, properties and provenance of the open note.
pub fn draw_info(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let path = app.notebook.tab().map(|t| t.path.clone());
    info::draw(app, buf, area, path.as_deref())
}

/// `RightDetail::Note(path)`: a note's details in the right pane.
pub fn draw_detail(app: &mut App, buf: &mut Buffer, area: Rect, path: &str) -> Option<(u16, u16)> {
    info::draw(app, buf, area, Some(path))
}

/// Draws a dialog; registers its hits with `ctx.hit` (only the top-most
/// dialog's count). Returns its rectangle and text cursor.
pub fn draw_dialog(app: &mut App, buf: &mut Buffer, screen: Rect, dialog: &Dialog, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    dialogs::draw(app, buf, screen, dialog, ctx)
}

/// A key in one of the module's dialogs.
pub fn dialog_key(app: &mut App, dialog: &mut Dialog, key: KeyEvent) -> Outcome {
    dialogs::key(app, dialog, key)
}

/// A mouse event while one of the module's dialogs is on top (every kind;
/// `hit`: the top-most click target under the pointer).
pub fn dialog_mouse(app: &mut App, dialog: &mut Dialog, mouse: MouseEvent, hit: Option<crate::dialogs::Hit>) -> Outcome {
    dialogs::mouse(app, dialog, mouse, hit)
}

/// Pasted or IME text while one of the module's dialogs is on top.
pub fn dialog_text(app: &mut App, dialog: &mut Dialog, text: &str) {
    dialogs::text(app, dialog, text)
}

/// A key while the module has the keyboard; false leaves it to the global keys.
pub fn on_key(app: &mut App, key: KeyEvent) -> bool {
    app.needs_draw = true;
    let m = key.modifiers;
    let ctrl = m.contains(Modifiers::CONTROL) && !m.contains(Modifiers::ALT);
    let alt = app.key_policy.alt_shortcut(&key) && !m.contains(Modifiers::CONTROL);
    let shift = m.contains(Modifiers::SHIFT);
    // The name filter keeps its editing keys (Ctrl+W deletes a word there).
    if app.focus == Focus::Sidebar && app.notebook.filtering {
        return explorer::key(app, key);
    }
    match key.code {
        KeyCode::Char('h' | 'H') if alt => {
            explorer::set_filter(app, app.notebook.filter.next());
            return true;
        }
        KeyCode::Char('w' | 'W') if ctrl => {
            tabs::close_tab(app, app.notebook.active);
            return true;
        }
        KeyCode::Tab if ctrl => {
            tabs::step_tab(app, !shift);
            return true;
        }
        KeyCode::BackTab if ctrl => {
            tabs::step_tab(app, false);
            return true;
        }
        KeyCode::PageDown | KeyCode::PageUp if ctrl => {
            tabs::step_tab(app, key.code == KeyCode::PageDown);
            return true;
        }
        KeyCode::Char('e' | 'E') if ctrl => {
            tabs::toggle_rendered(app);
            return true;
        }
        // A new note in the explorer's selected folder (from the reader too).
        KeyCode::Char('n' | 'N') if ctrl && !shift => {
            explorer::new_note_here(app);
            return true;
        }
        // Quitting is the global key's (`App::save_now` writes what was typed in the last second).
        KeyCode::Char('q' | 'Q') if ctrl => return false,
        _ => {}
    }
    if app.focus == Focus::Sidebar { explorer::key(app, key) } else { tabs::key(app, key) }
}

/// A mouse event in the main area or the module's sidebar (and the drag and
/// release of a press there).
pub fn on_mouse(app: &mut App, mouse: MouseEvent) {
    if crate::paint::rect_contains(app.layout.sidebar, mouse.column, mouse.row) && !matches!(mouse.kind, crate::input::MouseEventKind::Drag(_) | crate::input::MouseEventKind::Up(_)) {
        explorer::mouse(app, mouse);
    } else {
        tabs::mouse(app, mouse);
    }
}

/// A trackpad's scroll by whole `rows` (positive: further down) of what is
/// at `(x, y)`: the explorer, the info panel or the note.
pub(crate) fn scroll(app: &mut App, x: u16, y: u16, rows: i32) {
    if crate::paint::rect_contains(app.layout.sidebar, x, y) {
        explorer::scroll(app, rows);
    } else {
        tabs::scroll(app, x, y, rows);
    }
}

/// IME text or a paste while the module has the keyboard; false: not taken.
pub fn on_text(app: &mut App, text: &str) -> bool {
    app.needs_draw = true;
    if app.focus == Focus::Sidebar { explorer::text(app, text) } else { tabs::text(app, text) }
}

/// Every UI pass, in every space: autosave and change polling of open
/// notes; while the Notebook shows, its folders are listed again now and then.
pub fn tick(app: &mut App) {
    tabs::autosave(app);
    load::poll(app);
    if app.space == Space::Notebook {
        load::ensure_info(app);
        load::refresh(app);
        load::refresh_known(app);
    }
}

/// Alt+N, quick capture (`events.rs` runs [`capture`] for it outside
/// terminals and dialogs, in every space). AltGr (Ctrl+Alt) is not it.
pub fn is_capture_key(key: &KeyEvent) -> bool {
    let m = key.modifiers;
    m.contains(Modifiers::ALT) && !m.contains(Modifiers::CONTROL) && !m.contains(Modifiers::SHIFT) && matches!(key.code, KeyCode::Char('n' | 'N'))
}

/// Quick capture: `<root>/Inbox/<yyyy-mm-dd HHMM>.md`, opened in Source mode.
pub fn capture(app: &mut App) {
    ops::capture(app);
}

/// Writes every unsaved note now (blocking; for quitting).
pub fn flush(app: &mut App) {
    ops::flush(app);
}

/// Where the Notebook is (the navigation history records it when you
/// leave): the open note, else nothing.
pub fn current_target(app: &App) -> NavTarget {
    app.notebook.tab().map_or(NavTarget::None, |t| NavTarget::Note(t.path.clone()))
}

/// Go to's notes: the notes this window has listed or opened.
pub fn goto_items(app: &App) -> Vec<GoItem> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    let tabs = app.notebook.tabs.iter().map(|t| Known { path: t.path.clone(), name: t.name.clone(), title: t.title(), ai: t.provenance.audience == canvas_model::notebook::Audience::Ai });
    for known in tabs.chain(app.notebook.known.iter().cloned()) {
        if !seen.insert(norm(&known.path)) {
            continue;
        }
        let place = tree::place_label(app, &known.path);
        let audience = if known.ai { "◇ for agents" } else { "● for you" };
        let label = if known.title.is_empty() || known.title == canvas_model::notebook::stem(&known.name) { known.name.clone() } else { format!("{} ({})", known.title, known.name) };
        items.push(GoItem { label, detail: format!("note · {audience} · {place}"), glyph: "✎", space: Space::Notebook, target: NavTarget::Note(known.path) });
    }
    items
}

/// The group (id) has notes to open: it has a folder.
pub fn group_has_notes(app: &App, group: &str) -> bool {
    app.doc.group(group).is_some_and(|g| g.folder.as_deref().is_some_and(|f| !f.trim().is_empty()))
}

/// The Settings view's Notebook rows: (row, label, value).
pub fn setting_rows(app: &App) -> Vec<(SettingRow, String, String)> {
    let folder = match (&app.doc.notebook_root, &app.notebook.info) {
        (Some(root), _) if !root.trim().is_empty() => root.clone(),
        (_, Some(info)) => format!("{} (default)", info.root),
        _ => "<home>\\notebook (default)".into(),
    };
    let mounts = match app.doc.notebook_mounts.len() {
        0 => "none".to_owned(),
        1 => "1 folder".to_owned(),
        n => format!("{n} folders"),
    };
    vec![(SettingRow::Folder, "Notebook folder".into(), folder), (SettingRow::Mounts, "Notebook mounts…".into(), mounts)]
}

// ---------- paths on the service's machine ----------

/// A path written the Windows way (`C:\…`, `\\server\…`).
fn windows_style(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || path.starts_with("\\\\")
}

/// A path as a key: one separator kind, no trailing separator, and (Windows
/// paths) case-insensitive.
pub fn norm(path: &str) -> String {
    let path = path.trim();
    if windows_style(path) {
        let p = path.replace('/', "\\");
        let trimmed = p.trim_end_matches('\\');
        let trimmed = if trimmed.len() == 2 { format!("{trimmed}\\") } else { trimmed.to_owned() };
        trimmed.to_lowercase()
    } else {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() { "/".to_owned() } else { trimmed.to_owned() }
    }
}

/// Two paths name the same file.
pub fn same_path(a: &str, b: &str) -> bool {
    norm(a) == norm(b)
}

/// `path` is `dir` or inside it.
pub fn is_inside(path: &str, dir: &str) -> bool {
    let (p, d) = (norm(path), norm(dir));
    let sep = if windows_style(dir.trim()) { '\\' } else { '/' };
    p == d || p.strip_prefix(&d).is_some_and(|rest| rest.starts_with(sep) || d.ends_with(sep))
}

/// An absolute path (not a `[[note name]]`).
pub fn is_absolute(path: &str) -> bool {
    windows_style(path) || path.starts_with('/')
}

/// `dir` joined with `name` with the separator `dir` uses.
pub fn join(dir: &str, name: &str) -> String {
    let sep = if windows_style(dir) || dir.contains('\\') { '\\' } else { '/' };
    let dir = dir.trim_end_matches(['\\', '/']);
    format!("{dir}{sep}{name}")
}

/// The folder holding `path` (`None` for a root).
pub fn parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let at = trimmed.rfind(['\\', '/'])?;
    let parent = &trimmed[..at];
    Some(if parent.is_empty() || parent.ends_with(':') { format!("{parent}{}", &trimmed[at..at + 1]) } else { parent.to_owned() })
}

/// The last component of a path.
pub fn file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed).to_owned()
}
