//! The Tasks space (docs/NOTEBOOK-TASKS.md, "Tasks space UI"): its sidebar
//! (needs you, ready, claimed by sessions, the boards), the todo canvas
//! (board frames with columns, swimlanes and cards at three zoom levels)
//! and a todo's details, plus what the session canvas shows of the todos
//! (a group's chip counts, a session's `☑ DEC-031` badge).
//!
//! The spaces foundation calls the entry points below (the dispatch lives
//! in `app.rs`, `events.rs`, `actions.rs`, `modals.rs`, `ui.rs`,
//! `ui_side.rs`, `ui_modals.rs`, `folders.rs` and `ui_canvas.rs`):
//!
//! - [`State`] is `App::tasks` (this window's Tasks space).
//! - [`Action`] runs as `dialogs::Action::Tasks(..)` through [`run`]: menus,
//!   chips and buttons register `Hit::Toolbar(Action::Tasks(..))` or
//!   `Hit::Tasks(Hit::Do(..))`.
//! - [`Hit`] is registered as `dialogs::Hit::Tasks(..)`; a press on it comes
//!   to [`click`]. The todo canvas itself is hit-tested from its last layout
//!   ([`layout::Scene`]) in [`on_mouse`], which gets every press there.
//! - [`Dialog`] opens as `Modal::Tasks(Box::new(..))`; pickers, menus and
//!   confirmations are the app's own `Modal::Menu` / `Modal::Confirm`.
//! - Data: the book is the service's (`app.bus.todos`, [`book`]); every
//!   change is applied here at once and sent as an operation
//!   ([`book::apply`]); books the service sends before it has the change
//!   get it put back on top ([`book::receive`]) until one has it, or a
//!   refusal (an error notice, [`book::refused`]) or 5 s say otherwise.
//!   Demo mode works on a local demo book.
//! - Viewing never starts a session: *Start a session on this…* opens the
//!   new-session dialog, which is the explicit start.

use std::sync::OnceLock;

use canvas_model::todos::{Status, TodoBook, TodoOp, TodoType};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::{App, Focus};
use crate::dialogs::TextInput;
use crate::doc::{Node, NodeKind};
use crate::input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use crate::modals::Outcome;
use crate::spaces::{GoItem, NavTarget, Space};
use crate::ui_modals::Ctx;

mod book;
mod canvas;
mod detail;
mod flows;
mod forms;
pub mod layout;
mod side;
#[cfg(test)]
pub(crate) mod tests;

pub use book::{apply, book, receive, refused, session_gone, type_color, type_glyph};
pub use canvas_model::todos::TodoCounts;
pub use forms::{Ask, Checks, ChecksFor, NewTodoForm, Prompt};

/// This window's Tasks space (`App::tasks`).
#[derive(Clone, Debug)]
pub struct State {
    /// The place last opened.
    pub target: Option<NavTarget>,
    /// The selected todo (its card is lit; arrows move from it).
    pub selected: Option<String>,
    /// The selected column of a swimlane: (lane board id, status); `N` adds there.
    pub cell: Option<(String, Status)>,
    /// The selected top-level board.
    pub board: Option<String>,
    /// The world cell at the todo canvas's top-left corner.
    pub pan: (i32, i32),
    /// 0: 100 %, 1: 50 %, 2: 25 %.
    pub zoom: u8,
    /// The sidebar's `/` filter (the canvas shows only matching cards too).
    pub filter: TextInput,
    pub filter_active: bool,
    /// The type filter.
    pub kind: Option<TodoType>,
    /// The canvas group viewed last (READY lists its boards' todos).
    pub last_group: Option<String>,
    pub side_scroll: usize,
    /// The highlighted sidebar row (keyboard), among its selectable rows.
    pub side_sel: usize,
    pub detail_scroll: u16,
    /// The press being dragged in the todo canvas.
    pub(crate) press: Option<canvas::Press>,
    /// A card or board to bring into view at the next drawing.
    pub(crate) reveal: Option<canvas::Reveal>,
    /// The todo canvas as last drawn (hit testing).
    pub(crate) scene: layout::Scene,
    /// What `scene` was laid out from (it is kept while that stays).
    pub(crate) scene_key: Option<canvas::SceneKey>,
    /// Layouts made (tests: the scene is kept between frames).
    pub(crate) layouts: u64,
    /// Counts changes of the book shown (`scene` is laid out again after one).
    pub(crate) book_gen: u64,
    /// Your changes sent to the service and not yet in a book it sent back
    /// ([`book::receive`]), oldest first.
    pub(crate) pending: Vec<book::Pending>,
    /// The service's last book while changes are pending.
    pub(crate) served: Option<TodoBook>,
    /// The session canvas's badges and chip counts for the book shown.
    pub(crate) links: std::cell::RefCell<book::Links>,
    /// The sidebar's rows as last drawn.
    pub(crate) side: Vec<side::Row>,
    /// The body being edited (todo id, editor) and where it was drawn.
    pub(crate) body: Option<(String, crate::editor::Editor)>,
    pub(crate) body_area: Rect,
    /// The demo book until the first change (demo mode).
    pub(crate) demo: OnceLock<TodoBook>,
}

impl Default for State {
    fn default() -> Self {
        State {
            target: None,
            selected: None,
            cell: None,
            board: None,
            pan: (-2, -1),
            zoom: 0,
            filter: TextInput::default(),
            filter_active: false,
            kind: None,
            last_group: None,
            side_scroll: 0,
            side_sel: 0,
            detail_scroll: 0,
            press: None,
            reveal: None,
            scene: layout::Scene::default(),
            scene_key: None,
            layouts: 0,
            book_gen: 0,
            pending: Vec::new(),
            served: None,
            links: Default::default(),
            side: Vec::new(),
            body: None,
            body_area: Rect::default(),
            demo: OnceLock::new(),
        }
    }
}

/// Tasks commands (`dialogs::Action::Tasks`).
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// The boards linked to a group (id); none: link or create one.
    OpenGroup(String),
    /// A todo's card (id).
    OpenTodo(String),
    /// Create a board for a group (id) or link existing ones.
    LinkBoards(String),
    /// Selects a todo, shows its details and pans to its card.
    Select(String),
    /// Shows a board (pans to it).
    OpenBoard(String),
    /// Applies an operation as you (at once here, then sent).
    Op(Box<TodoOp>),
    /// Asks for a new board's name.
    NewBoard,
    /// The new-todo dialog for a lane's column.
    NewTodo { lane: String, status: Status },
    /// A todo's menu and its pickers (at the pointer).
    TodoMenu(String),
    PickType(String),
    PickPriority(String),
    PickStatus(String),
    /// Asks for a line of text (a title, a reason, a path…).
    Ask(Ask),
    /// Asks before canceling a todo.
    ConfirmCancel(String),
    EditBody(String),
    /// The new-session dialog, prefilled for a todo (the dialog starts it).
    StartSession(String),
    /// Creates the todo's spec note when missing, links it and opens it.
    SpecNote(String),
    /// Picks who claims a todo.
    ClaimFor(String),
    /// A board's menu.
    BoardMenu(String),
    /// Picks the canvas groups a board is linked to.
    LinkGroups(String),
    ConfirmDeleteBoard(String),
    /// Asks for the Markdown mirror's path.
    Mirror,
    /// Asks where to export a board as JSON Canvas.
    Export(String),
    /// Views a session on the canvas (never starts it).
    ViewSession(String),
    /// Opens a note in the Notebook.
    OpenNote(String),
    /// A web link: asks before the browser.
    OpenUrl(String),
    /// +1 zooms in, -1 out.
    Zoom(i32),
    /// Shows every board.
    Fit,
    TypeFilter(Option<TodoType>),
    PickTypeFilter,
    /// A quick look at a card.
    Peek(String),
    CloseDetail,
}

/// Click targets the module registers (`dialogs::Hit::Tasks`).
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    /// A todo's row or link (left: select it; right: its menu).
    Todo(String),
    /// A board's row (left: show it; right: its menu).
    Board(String),
    /// A button, chip or row that runs an action (left button).
    Do(Action),
    /// The sidebar's filter field.
    Filter,
}

/// The module's dialogs (`Modal::Tasks`).
#[derive(Clone, Debug)]
pub enum Dialog {
    /// A message with an OK button.
    Info { title: String, lines: Vec<String> },
    /// One line of text.
    Prompt(Prompt),
    /// A new todo: title, type, priority.
    NewTodo(NewTodoForm),
    /// Checkboxes: boards of a group, groups of a board.
    Checks(Checks),
    /// A todo's body in the editor (`State::body`); Ctrl+S or Save keeps it.
    Body { id: String },
    /// Space on a card: a quick look.
    Peek { id: String },
}

/// Runs a command. Jumps go through `app.navigate`, which records the
/// history and calls [`open`].
pub fn run(app: &mut App, action: Action) {
    flows::run(app, action);
    app.needs_draw = true;
}

/// A press on one of the module's hits (`mouse`: the press, with its button).
pub fn click(app: &mut App, hit: Hit, mouse: MouseEvent) {
    let right = mouse.kind == MouseEventKind::Down(MouseButton::Right);
    if crate::paint::rect_contains(app.layout.sidebar, mouse.column, mouse.row) {
        side::follow(app, mouse.row);
    }
    if hit != Hit::Filter {
        app.tasks.filter_active = false;
    }
    match hit {
        Hit::Todo(id) if right => flows::todo_menu(app, &id, (mouse.column, mouse.row)),
        Hit::Todo(id) => flows::show_todo(app, &id, true),
        Hit::Board(id) if right => flows::board_menu(app, &id, (mouse.column, mouse.row)),
        Hit::Board(id) => flows::show_board(app, &id),
        Hit::Do(_) if right => {}
        Hit::Do(action) => run(app, action),
        Hit::Filter => {
            app.tasks.filter_active = true;
            app.set_focus(Focus::Sidebar);
        }
    }
    app.needs_draw = true;
}

/// Shows a place in Tasks (the space is already switched; no history): a
/// todo (selected, its details on the right, its card in view), a board,
/// or a group's first linked board (none: offers to link or create one).
pub fn open(app: &mut App, target: NavTarget) {
    match &target {
        NavTarget::Todo(id) => flows::show_todo(app, id, true),
        NavTarget::Board(id) => flows::show_board(app, id),
        NavTarget::Group(id) => {
            app.tasks.last_group = Some(id.clone());
            match book(app).and_then(|b| b.boards_of_group(id).first().map(|b| b.id.clone())) {
                Some(board) => flows::show_board(app, &board),
                None => flows::link_boards(app, id),
            }
        }
        _ => {}
    }
    if target != NavTarget::None {
        app.tasks.target = Some(target);
    }
    app.needs_draw = true;
}

/// The sidebar (slot 1 of the activity bar: ☑ TASKS).
pub fn draw_side(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    side::draw(app, buf, area)
}

/// The main area: the todo canvas.
pub fn draw_main(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    canvas::draw(app, buf, area)
}

/// The right pane when no terminals or details take it: the selected
/// todo's details, else the selected board's summary and the keys.
pub fn draw_info(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    detail::draw_info(app, buf, area)
}

/// `RightDetail::Todo(id)`: a todo's details in the right pane.
pub fn draw_detail(app: &mut App, buf: &mut Buffer, area: Rect, id: &str) -> Option<(u16, u16)> {
    detail::draw(app, buf, area, id)
}

/// Draws a dialog; registers its hits with `ctx.hit` (only the top-most
/// dialog's count). Returns its rectangle and text cursor.
pub fn draw_dialog(app: &mut App, buf: &mut Buffer, screen: Rect, dialog: &Dialog, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    forms::draw(app, buf, screen, dialog, ctx)
}

/// A key in one of the module's dialogs.
pub fn dialog_key(app: &mut App, dialog: &mut Dialog, key: KeyEvent) -> Outcome {
    forms::key(app, dialog, key)
}

/// A mouse event while one of the module's dialogs is on top (every kind;
/// `hit`: the top-most click target under the pointer).
pub fn dialog_mouse(app: &mut App, dialog: &mut Dialog, mouse: MouseEvent, hit: Option<crate::dialogs::Hit>) -> Outcome {
    forms::mouse(app, dialog, mouse, hit)
}

/// Pasted or IME text while one of the module's dialogs is on top.
pub fn dialog_text(app: &mut App, dialog: &mut Dialog, text: &str) {
    forms::text(app, dialog, text);
}

/// A key while the module has the keyboard; false leaves it to the global keys.
pub fn on_key(app: &mut App, key: KeyEvent) -> bool {
    if key.kind == crate::input::KeyEventKind::Release {
        return false;
    }
    if app.focus != Focus::Sidebar {
        app.tasks.filter_active = false;
    }
    let handled = if app.tasks.filter_active { side::filter_key(app, key) } else if app.focus == Focus::Sidebar { side::key(app, key) } else { canvas::key(app, key) };
    if handled {
        app.needs_draw = true;
    }
    handled
}

/// A mouse event in the main area, the sidebar or the details (and the
/// drag and release of a press there).
pub fn on_mouse(app: &mut App, mouse: MouseEvent) {
    let (x, y) = (mouse.column, mouse.row);
    let wheel = matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight);
    if matches!(mouse.kind, MouseEventKind::Down(_)) {
        // A press elsewhere ends typing the filter.
        app.tasks.filter_active = false;
    }
    if app.tasks.press.is_none() && crate::paint::rect_contains(app.layout.sidebar, x, y) {
        if wheel {
            side::wheel(app, mouse.kind);
        }
        return;
    }
    if app.tasks.press.is_none() && crate::paint::rect_contains(app.layout.terms, x, y) {
        if wheel {
            detail::wheel(app, mouse.kind);
        }
        return;
    }
    canvas::mouse(app, mouse);
}

/// The todo canvas (not the sidebar or the details) is at `(x, y)`.
pub(crate) fn board_at(app: &App, x: u16, y: u16) -> bool {
    let inside = |r| crate::paint::rect_contains(r, x, y);
    !inside(app.layout.sidebar) && !inside(app.layout.terms) && inside(app.layout.canvas)
}

/// A trackpad's scroll by whole `rows` (positive: down) of the sidebar or
/// the details at `(x, y)` (the board pans: [`pan_board`]).
pub(crate) fn scroll(app: &mut App, x: u16, y: u16, rows: i32) {
    if crate::paint::rect_contains(app.layout.sidebar, x, y) {
        side::scroll(app, rows);
    } else if crate::paint::rect_contains(app.layout.terms, x, y) {
        detail::scroll(app, rows);
    }
}

/// Moves the view over the todo canvas by whole cells (positive: right and down).
pub(crate) fn pan_board(app: &mut App, dx: i32, dy: i32) {
    canvas::pan(app, dx, dy);
}

/// The todo canvas's next zoom level in (`step` > 0) or out.
pub(crate) fn zoom_board(app: &mut App, step: i32) {
    canvas::zoom(app, step);
}

/// Every board in view (the todo canvas's *fit*).
pub(crate) fn fit_board(app: &mut App) {
    canvas::fit(app);
}

/// IME text or a paste while the module has the keyboard; false: not taken.
pub fn on_text(app: &mut App, text: &str) -> bool {
    if !app.tasks.filter_active || app.focus != Focus::Sidebar {
        return false;
    }
    app.tasks.filter.insert(text);
    app.tasks.side_sel = 0;
    app.tasks.side_scroll = 0;
    app.needs_draw = true;
    true
}

/// Every UI pass, in every space: changes the service never sent back go
/// after a while ([`book::PENDING`]); on the canvas, remembers the group you
/// looked at last (READY lists its boards' todos).
pub fn tick(app: &mut App) {
    book::tick(app);
    if app.space != Space::Canvas {
        return;
    }
    let group = app.selected_group.clone().or_else(|| app.selection.last().and_then(|id| app.doc.node(id)).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()));
    if group.is_some() && group != app.tasks.last_group {
        app.tasks.last_group = group;
    }
}

/// Go to's boards and todos.
pub fn goto_items(app: &App) -> Vec<GoItem> {
    book::goto_items(app)
}

/// Where Tasks is (the navigation history records it when you leave): the
/// selected todo, else the selected board, else nothing.
pub fn current_target(app: &App) -> NavTarget {
    match (&app.tasks.selected, &app.tasks.board) {
        (Some(id), _) => NavTarget::Todo(id.clone()),
        (None, Some(board)) => NavTarget::Board(board.clone()),
        (None, None) => NavTarget::None,
    }
}

/// The todos of the boards linked to a group (id); `None` when no board is
/// linked (its chip offers to create or link one).
pub fn group_counts(app: &App, group: &str) -> Option<TodoCounts> {
    book::group_counts(app, group)
}

/// The todo a session's box shows (`☑ DEC-031`): the one it claimed.
pub fn session_badge(app: &App, node: &Node) -> Option<String> {
    if node.kind != NodeKind::Session {
        return None;
    }
    book::session_badge(app, &node.id)
}
