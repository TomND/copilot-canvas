//! The Tasks space's dialogs: one-line prompts ([`Prompt`] for an [`Ask`]),
//! a new todo ([`NewTodoForm`]), checkboxes ([`Checks`]: the boards of a
//! group, the groups of a board), a todo's body in the editor and a quick
//! look at a card. Pickers, menus and confirmations use the app's own.

use canvas_model::todos::{NewTodo, Priority, Status, TodoOp, TodoType, TodoUpdate};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::book::{self, book, type_color, type_glyph};
use super::{Dialog, flows};
use crate::app::App;
use crate::dialogs::{Hit as AppHit, TextInput};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::modals::Outcome;
use crate::paint::{ACCENT, GRAY, MODAL, MUTED, NEGATIVE, PANEL_HI, SELECT, TEXT, bold, fg, fill, put, text_width, truncate};
use crate::ui_modals::{Ctx, button, centered, field, panel};

/// What a prompt asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    RenameTodo(String),
    /// Why a todo is blocked (empty: not blocked any more).
    Blocked(String),
    AddCheck(String),
    /// A note for a todo's log.
    Note(String),
    /// Declines a proposal with a reason.
    Decline(String),
    /// A board linked to a group, or a swimlane of a board.
    NewBoard { group: Option<String>, parent: Option<String> },
    RenameBoard(String),
    Prefix(String),
    Wip(String),
    /// A board's notes folder.
    Notes(String),
    /// The book's Markdown mirror (`default`: `<notebook root>/Todos/Backlog.md`).
    Mirror { default: Option<String> },
    /// Where to write a board as JSON Canvas.
    Export { board: String, default: Option<String> },
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub ask: Ask,
    pub title: String,
    pub hint: String,
    pub input: TextInput,
    pub error: Option<String>,
}

impl Prompt {
    /// The prompt for `ask` with the current value (`None`: what it is about is gone).
    pub fn new(app: &App, ask: Ask) -> Option<Prompt> {
        let book = book(app);
        let todo = |id: &str| book.and_then(|b| b.todo(id)).cloned();
        let board = |id: &str| book.and_then(|b| b.board(id)).cloned();
        let (title, hint, text) = match &ask {
            Ask::RenameTodo(id) => (format!("Rename {id}"), "Its title.".to_owned(), todo(id)?.title),
            Ask::Blocked(id) => (format!("Why is {id} blocked?"), "Empty: not blocked any more.".into(), todo(id)?.blocked.unwrap_or_default()),
            Ask::AddCheck(id) => (format!("Checklist item for {id}"), "Something to tick off.".into(), String::new()),
            Ask::Note(id) => (format!("Note on {id}"), "Added to its log.".into(), String::new()),
            Ask::Decline(id) => (format!("Decline {id}"), "Why (optional): kept in its log.".into(), String::new()),
            Ask::NewBoard { group, parent } => {
                let group_name = group.as_deref().and_then(|g| app.doc.group(g)).map(|g| g.name.clone());
                match (parent.as_deref().and_then(board), group_name) {
                    (Some(parent), _) => (format!("New swimlane of “{}”", parent.name), "Its name; it shares the board's columns and prefix.".into(), String::new()),
                    (None, Some(name)) => ("New todo board".into(), format!("Its name; linked to the group “{name}”."), name),
                    (None, None) => ("New todo board".into(), "Its name; the id prefix comes from it (Decomps: DEC).".into(), String::new()),
                }
            }
            Ask::RenameBoard(id) => ("Rename board".into(), "Its name.".into(), board(id)?.name),
            Ask::Prefix(id) => {
                let b = board(id)?;
                (format!("Prefix of “{}”", b.name), "2 to 5 letters A-Z; existing todos keep their ids.".into(), b.prefix)
            }
            Ask::Wip(id) => {
                let b = board(id)?;
                (format!("WIP limit of “{}”", b.name), "Todos in progress at most (a warning, not a rule); 0: none.".into(), b.wip.map(|w| w.to_string()).unwrap_or_default())
            }
            Ask::Notes(id) => {
                let b = board(id)?;
                (format!("Notes folder of “{}”", b.name), "A folder on the service's machine; empty: none.".into(), b.notes.unwrap_or_default())
            }
            Ask::Mirror { default } => {
                let current = book.and_then(|b| b.settings.mirror.clone());
                ("Markdown mirror of the todos".into(), "Rewritten by the service after every change; empty: no mirror.".into(), current.or(default.clone()).unwrap_or_default())
            }
            Ask::Export { board: id, default } => (format!("Export “{}” as JSON Canvas", board(id)?.name), "A .canvas file inside the notebook's folders (replaced if there).".into(), default.clone().unwrap_or_default()),
        };
        Some(Prompt { ask, title, hint, input: TextInput::new(&text), error: None })
    }
}

/// Applies a prompt's answer; `Err`: why not (the prompt stays open).
pub fn submit(app: &mut App, ask: &Ask, text: &str) -> Result<(), String> {
    let text = text.trim().to_owned();
    let needed = |what: &str| if text.is_empty() { Err(format!("Type {what} first.")) } else { Ok(()) };
    let update = |id: &str| TodoUpdate { id: id.to_owned(), ..Default::default() };
    let board_op = |board: &str, name: Option<String>, prefix: Option<String>, wip: Option<u32>, notes: Option<String>| TodoOp::UpdateBoard { board: board.to_owned(), name, prefix, wip, color: None, notes, policy: None, statuses: None };
    let op = match ask {
        Ask::RenameTodo(id) => {
            needed("a title")?;
            TodoOp::Update(Box::new(TodoUpdate { title: Some(text), ..update(id) }))
        }
        Ask::Blocked(id) => TodoOp::Update(Box::new(TodoUpdate { blocked: Some(text), ..update(id) })),
        Ask::AddCheck(id) => {
            needed("the item")?;
            TodoOp::AddCheck { id: id.clone(), text }
        }
        Ask::Note(id) => {
            needed("the note")?;
            TodoOp::Note { id: id.clone(), text }
        }
        Ask::Decline(id) => TodoOp::Decline { id: id.clone(), reason: Some(text).filter(|t| !t.is_empty()) },
        Ask::NewBoard { group, parent } => {
            needed("a name")?;
            TodoOp::AddBoard { name: text, prefix: None, parent: parent.clone(), groups: group.iter().cloned().collect(), x: None, y: None }
        }
        Ask::RenameBoard(id) => {
            needed("a name")?;
            board_op(id, Some(text), None, None, None)
        }
        Ask::Prefix(id) => {
            needed("a prefix")?;
            board_op(id, None, Some(text.to_ascii_uppercase()), None, None)
        }
        Ask::Wip(id) => {
            let wip = if text.is_empty() { 0 } else { text.parse::<u32>().map_err(|_| "A number of todos, like 3 (0: no limit).".to_owned())? };
            board_op(id, None, None, Some(wip), None)
        }
        Ask::Notes(id) => board_op(id, None, None, None, Some(text)),
        Ask::Mirror { .. } => TodoOp::SetMirror { path: Some(text).filter(|t| !t.is_empty()) },
        Ask::Export { board, .. } => {
            needed("a path")?;
            flows::export(app, board, text);
            return Ok(());
        }
    };
    match book::apply(app, op) {
        Some(applied) => {
            if let (Ask::NewBoard { .. }, Some(created)) = (ask, applied.created) {
                flows::show_board(app, &created);
            }
            Ok(())
        }
        None => Err(app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_else(|| "Refused.".into())),
    }
}

/// The new-todo dialog: a title, a type and a priority, for a lane's column.
#[derive(Clone, Debug)]
pub struct NewTodoForm {
    /// The board or sub-board.
    pub lane: String,
    pub status: Status,
    pub title: TextInput,
    pub kind: TodoType,
    pub priority: Priority,
    /// 0 title, 1 type, 2 priority, 3 Create, 4 Cancel.
    pub focus: usize,
    pub error: Option<String>,
}

impl NewTodoForm {
    pub fn new(lane: &str, status: Status) -> Self {
        NewTodoForm { lane: lane.to_owned(), status, title: TextInput::default(), kind: TodoType::Feature, priority: Priority::None, focus: 0, error: None }
    }

    /// The `Add` operation it makes.
    pub fn op(&self) -> TodoOp {
        TodoOp::Add(Box::new(NewTodo { board: self.lane.clone(), title: self.title.text.trim().to_owned(), kind: self.kind, priority: Some(self.priority), status: Some(self.status), ..Default::default() }))
    }
}

/// What a checkbox dialog links.
#[derive(Clone, Debug, PartialEq)]
pub enum ChecksFor {
    /// The boards linked to a canvas group (id).
    GroupBoards(String),
    /// The canvas groups a board (id) is linked to.
    BoardGroups(String),
}

#[derive(Clone, Debug)]
pub struct Checks {
    pub what: ChecksFor,
    pub title: String,
    /// (board or group id, label, linked now, checked here).
    pub items: Vec<(String, String, bool, bool)>,
    /// The highlighted row (after the items: `+ New board for this group`).
    pub sel: usize,
    /// Offers a new board for the group.
    pub new_board: bool,
}

impl Checks {
    /// The boards of the book, checked when linked to `group`.
    pub fn group_boards(app: &App, group: &str) -> Option<Checks> {
        let book = book(app)?;
        let name = app.doc.group(group).map(|g| g.name.clone()).unwrap_or_else(|| group.to_owned());
        let items = book
            .boards
            .iter()
            .map(|b| {
                let label = match b.parent.as_deref().and_then(|p| book.board(p)) {
                    Some(parent) => format!("{} › {}", parent.name, b.name),
                    None => format!("{} · {}", b.name, b.prefix),
                };
                let linked = b.canvas_groups.iter().any(|g| g == group);
                (b.id.clone(), label, linked, linked)
            })
            .collect();
        Some(Checks { what: ChecksFor::GroupBoards(group.to_owned()), title: format!("Todo boards of “{name}”"), items, sel: 0, new_board: true })
    }

    /// The canvas's groups, checked when `board` is linked to them.
    pub fn board_groups(app: &App, board: &str) -> Option<Checks> {
        let b = book(app)?.board(board)?;
        let items = app.doc.groups.iter().filter(|g| !g.is_canvas_agents()).map(|g| (g.id.clone(), g.name.clone(), b.canvas_groups.contains(&g.id), b.canvas_groups.contains(&g.id))).collect();
        Some(Checks { what: ChecksFor::BoardGroups(b.id.clone()), title: format!("Canvas groups of “{}”", b.name), items, sel: 0, new_board: false })
    }

    /// Rows: the items, then the new-board row.
    fn rows(&self) -> usize {
        self.items.len() + usize::from(self.new_board)
    }

    /// The `LinkBoard` operations for what changed.
    pub fn ops(&self) -> Vec<TodoOp> {
        self.items
            .iter()
            .filter(|(_, _, was, now)| was != now)
            .map(|(id, _, _, now)| match &self.what {
                ChecksFor::GroupBoards(group) => TodoOp::LinkBoard { board: id.clone(), group: group.clone(), unlink: !now },
                ChecksFor::BoardGroups(board) => TodoOp::LinkBoard { board: board.clone(), group: id.clone(), unlink: !now },
            })
            .collect()
    }
}

/// `+ New board for this group`: a board named after the group, linked to it
/// (when the name is taken: a prompt for another).
fn new_group_board(app: &mut App, group: &str) {
    let name = app.doc.group(group).map(|g| g.name.clone()).unwrap_or_else(|| "Todos".into());
    let op = TodoOp::AddBoard { name, prefix: None, parent: None, groups: vec![group.to_owned()], x: None, y: None };
    match book::apply(app, op) {
        Some(applied) => {
            if let Some(created) = applied.created {
                app.navigate(crate::spaces::Space::Tasks, crate::spaces::NavTarget::Board(created));
            }
        }
        None => flows::ask(app, Ask::NewBoard { group: Some(group.to_owned()), parent: None }),
    }
}

fn text_at(buf: &mut Buffer, x: i32, y: i32, text: &str, style: Style, clip: Rect) -> i32 {
    put(buf, x, y, text, style.bg(MODAL), clip)
}

pub fn draw(app: &mut App, buf: &mut Buffer, screen: Rect, dialog: &Dialog, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    match dialog {
        Dialog::Info { title, lines } => {
            let width = lines.iter().map(|l| text_width(l)).max().unwrap_or(20).max(title.len()).max(40) as u16 + 6;
            let rect = centered(screen, width, lines.len() as u16 + 6);
            panel(buf, rect, title);
            for (i, line) in lines.iter().enumerate() {
                text_at(buf, rect.x as i32 + 3, rect.y as i32 + 2 + i as i32, line, fg(TEXT), rect);
            }
            button(buf, ctx, rect.x as i32 + 3, rect.bottom() as i32 - 2, "OK", true, true, 0, rect);
            (rect, None)
        }
        Dialog::Prompt(prompt) => {
            let rect = centered(screen, 72, 8);
            panel(buf, rect, &truncate(&prompt.title, 64));
            let (x, w) = (rect.x as i32 + 3, rect.width as i32 - 6);
            let cursor = field(buf, x, rect.y as i32 + 2, w, &prompt.input, true, "", rect);
            match &prompt.error {
                Some(error) => text_at(buf, x, rect.y as i32 + 3, &truncate(error, w as usize), fg(NEGATIVE), rect),
                None => text_at(buf, x, rect.y as i32 + 3, &truncate(&prompt.hint, w as usize), fg(MUTED), rect),
            };
            let y = rect.bottom() as i32 - 2;
            let next = button(buf, ctx, x, y, "OK", false, true, 0, rect);
            button(buf, ctx, next, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Dialog::NewTodo(form) => draw_new_todo(app, buf, screen, form, ctx),
        Dialog::Checks(checks) => draw_checks(buf, screen, checks, ctx),
        Dialog::Body { id } => {
            let rect = centered(screen, screen.width.saturating_sub(8).min(110), screen.height.saturating_sub(4).min(36));
            panel(buf, rect, &app.key_policy.label(&format!("Body of {id} · Ctrl+S saves · Esc closes")));
            let area = Rect::new(rect.x + 2, rect.y + 1, rect.width.saturating_sub(4), rect.height.saturating_sub(4));
            app.tasks.body_area = area;
            let cursor = match app.tasks.body.as_mut() {
                Some((_, editor)) => editor.draw(buf, area, true),
                None => None,
            };
            let y = rect.bottom() as i32 - 2;
            let next = button(buf, ctx, rect.x as i32 + 3, y, "Save", false, true, 0, rect);
            button(buf, ctx, next, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Dialog::Peek { id } => (draw_peek(app, buf, screen, id), None),
    }
}

fn draw_new_todo(app: &App, buf: &mut Buffer, screen: Rect, form: &NewTodoForm, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rect = centered(screen, 74, 13);
    let place = book(app)
        .and_then(|b| b.board(&form.lane).map(|lane| (lane.name.clone(), b.root(&form.lane).map(|r| r.status_name(form.status)).unwrap_or_else(|| form.status.label().to_owned()))))
        .map(|(lane, column)| format!("New todo · {lane} › {column}"))
        .unwrap_or_else(|| "New todo".into());
    panel(buf, rect, &truncate(&place, 66));
    let (x, y0) = (rect.x as i32 + 3, rect.y as i32 + 2);
    let label = |buf: &mut Buffer, y: i32, text: &str, focused: bool| text_at(buf, x, y, text, if focused { bold(ACCENT) } else { fg(GRAY) }, rect);
    label(buf, y0, "Title", form.focus == 0);
    let cursor = field(buf, x + 10, y0, rect.width as i32 - 16, &form.title, form.focus == 0, "What is to be done", rect);
    ctx.hit(Rect::new((x + 10) as u16, y0 as u16, (rect.width as i32 - 16).max(0) as u16, 1), AppHit::Toggle("title"));
    label(buf, y0 + 2, "Type", form.focus == 1);
    let mut cx = x + 10;
    let mut cy = y0 + 2;
    for (i, kind) in TodoType::ALL.iter().enumerate() {
        let text = format!(" {} {} ", type_glyph(*kind), kind.word());
        let w = text_width(&text) as i32;
        if cx + w > rect.right() as i32 - 3 {
            cx = x + 10;
            cy += 1;
        }
        let on = *kind == form.kind;
        let style = if on { bold(ratatui::style::Color::Black).bg(type_color(*kind)) } else { fg(type_color(*kind)).bg(PANEL_HI) };
        put(buf, cx, cy, &text, style, rect);
        ctx.hit(Rect::new(cx as u16, cy as u16, w as u16, 1), AppHit::ListRow(i));
        cx += w + 1;
    }
    let py = cy + 2;
    label(buf, py, "Priority", form.focus == 2);
    let mut cx = x + 10;
    for (i, priority) in Priority::ALL.iter().enumerate() {
        let text = if priority.marks().is_empty() { format!(" {} ", priority.word()) } else { format!(" {} {} ", priority.marks(), priority.word()) };
        let on = *priority == form.priority;
        let style = if on { bold(ratatui::style::Color::Black).bg(ACCENT) } else { fg(TEXT).bg(PANEL_HI) };
        let end = put(buf, cx, py, &text, style, rect);
        ctx.hit(Rect::new(cx as u16, py as u16, (end - cx) as u16, 1), AppHit::Chip(i));
        cx = end + 1;
    }
    if let Some(error) = &form.error {
        text_at(buf, x, py + 2, &truncate(error, (rect.width as usize).saturating_sub(6)), fg(NEGATIVE), rect);
    } else {
        text_at(buf, x, py + 2, "Tab: next field · ←/→: type, priority · Enter: create", fg(MUTED), rect);
    }
    let by = rect.bottom() as i32 - 2;
    let next = button(buf, ctx, x, by, "Create", form.focus == 3, true, 0, rect);
    button(buf, ctx, next, by, "Cancel", form.focus == 4, false, 1, rect);
    (rect, cursor)
}

fn draw_checks(buf: &mut Buffer, screen: Rect, checks: &Checks, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rows = checks.rows().max(1);
    let visible = rows.min(screen.height.saturating_sub(10) as usize).max(1);
    let rect = centered(screen, 64, visible as u16 + 7);
    panel(buf, rect, &truncate(&checks.title, 56));
    let x = rect.x as i32 + 3;
    let w = (rect.width as usize).saturating_sub(6);
    let scroll = checks.sel.saturating_sub(visible - 1);
    if checks.items.is_empty() && !checks.new_board {
        text_at(buf, x, rect.y as i32 + 2, "Nothing to link yet.", fg(MUTED), rect);
    }
    for row in scroll..(scroll + visible).min(checks.rows()) {
        let y = rect.y as i32 + 2 + (row - scroll) as i32;
        let lit = row == checks.sel;
        let bg = if lit { PANEL_HI } else { MODAL };
        fill(buf, x - 1, y, w as i32 + 2, 1, Style::default().bg(bg), rect);
        let text = match checks.items.get(row) {
            Some((_, label, _, on)) => format!("{} {}", if *on { "[x]" } else { "[ ]" }, label),
            None => "+ New board for this group".to_owned(),
        };
        let style = if checks.items.get(row).is_none() { bold(ACCENT) } else if lit { bold(SELECT) } else { fg(TEXT) };
        put(buf, x, y, &truncate(&text, w), style.bg(bg), rect);
        ctx.hit(Rect::new((x - 1) as u16, y as u16, (w + 2) as u16, 1), AppHit::ListRow(row));
    }
    text_at(buf, x, rect.bottom() as i32 - 4, "Space or click: tick · Enter: save", fg(MUTED), rect);
    let by = rect.bottom() as i32 - 2;
    let next = button(buf, ctx, x, by, "Save", false, true, 0, rect);
    button(buf, ctx, next, by, "Cancel", false, false, 1, rect);
    (rect, None)
}

fn draw_peek(app: &App, buf: &mut Buffer, screen: Rect, id: &str) -> Rect {
    let rect = centered(screen, 64, 16);
    let Some(book) = book(app) else {
        panel(buf, rect, id);
        return rect;
    };
    let Some(todo) = book.todo(id) else {
        panel(buf, rect, id);
        text_at(buf, rect.x as i32 + 3, rect.y as i32 + 2, "This todo is gone.", fg(MUTED), rect);
        return rect;
    };
    let status = book.root(&todo.board).map(|b| b.status_name(todo.status)).unwrap_or_else(|| todo.status.label().to_owned());
    panel(buf, rect, &format!("{} {} · {} · {}", type_glyph(todo.kind), todo.id, todo.kind.word(), status));
    let (x, w) = (rect.x as i32 + 3, (rect.width as usize).saturating_sub(6));
    let mut y = rect.y as i32 + 2;
    for line in super::canvas::wrap(&todo.title, w, 2) {
        text_at(buf, x, y, &line, bold(TEXT), rect);
        y += 1;
    }
    let mut facts = Vec::new();
    if !todo.priority.marks().is_empty() {
        facts.push(format!("{} {}", todo.priority.marks(), todo.priority.word()));
    }
    let (done, all) = todo.checked();
    if all > 0 {
        facts.push(format!("☑ {done}/{all}"));
    }
    if todo.claimed_by.is_some() {
        facts.push(format!("claimed by {}", book::session_name(app, todo)));
    }
    if book.is_blocked(todo) {
        facts.push("⚑ blocked".into());
    }
    text_at(buf, x, y + 1, &truncate(&facts.join(" · "), w), fg(GRAY), rect);
    let body = if todo.body.trim().is_empty() { todo.proposal.as_ref().map(|p| p.why.clone()).unwrap_or_default() } else { todo.body.clone() };
    for (i, line) in body.lines().filter(|l| !l.trim().is_empty()).take(7).enumerate() {
        text_at(buf, x, y + 3 + i as i32, &truncate(line, w), fg(TEXT), rect);
    }
    text_at(buf, x, rect.bottom() as i32 - 2, "Enter: details · any other key closes", fg(MUTED), rect);
    rect
}

/// Submits a prompt: closes it, or keeps it with why not.
fn submit_prompt(app: &mut App, prompt: &mut Prompt) -> Outcome {
    let text = prompt.input.text.clone();
    match submit(app, &prompt.ask.clone(), &text) {
        Ok(()) => Outcome::Close,
        Err(why) => {
            prompt.error = Some(why);
            Outcome::Keep
        }
    }
}

/// Creates the todo: selects it and shows its card, or keeps the dialog with why not.
fn submit_new(app: &mut App, form: &mut NewTodoForm) -> Outcome {
    if form.title.text.trim().is_empty() {
        form.error = Some("Type a title first.".into());
        form.focus = 0;
        return Outcome::Keep;
    }
    match book::apply(app, form.op()) {
        Some(applied) => {
            if let Some(id) = applied.created {
                flows::select_todo(app, &id, false, true);
            }
            Outcome::Close
        }
        None => {
            form.error = app.notice.as_ref().map(|n| n.text.clone());
            Outcome::Keep
        }
    }
}

fn save_checks(app: &mut App, checks: &Checks) -> Outcome {
    for op in checks.ops() {
        book::apply(app, op);
    }
    Outcome::Close
}

/// Keeps the body: an `Update` with it (`close`: and closes the editor).
fn save_body(app: &mut App, id: &str, close: bool) -> Outcome {
    let Some(text) = app.tasks.body.as_ref().map(|(_, e)| e.text()) else { return Outcome::Close };
    if book::apply(app, TodoOp::Update(Box::new(TodoUpdate { id: id.to_owned(), body: Some(text), ..Default::default() }))).is_some() {
        if let Some((_, editor)) = app.tasks.body.as_mut() {
            editor.mark_saved();
        }
        if close {
            app.tasks.body = None;
            return Outcome::Close;
        }
    }
    Outcome::Keep
}

fn close_body(app: &mut App) -> Outcome {
    if app.tasks.body.take().is_some_and(|(_, e)| e.is_dirty()) {
        app.notify(app.key_policy.label("The body was not saved (Ctrl+S saves).").into_owned(), false);
    }
    Outcome::Close
}

pub fn key(app: &mut App, dialog: &mut Dialog, key: KeyEvent) -> Outcome {
    let back = key.code == KeyCode::BackTab || (key.code == KeyCode::Tab && key.modifiers.contains(Modifiers::SHIFT));
    match dialog {
        Dialog::Info { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter => Outcome::Close,
            _ => Outcome::Keep,
        },
        Dialog::Prompt(prompt) => match key.code {
            KeyCode::Esc => Outcome::Close,
            KeyCode::Enter => submit_prompt(app, prompt),
            _ => {
                prompt.input.handle(&key);
                prompt.error = None;
                Outcome::Keep
            }
        },
        Dialog::NewTodo(form) => {
            let step = |i: usize, n: usize, forward: bool| if forward { (i + 1) % n } else { (i + n - 1) % n };
            match key.code {
                KeyCode::Esc => return Outcome::Close,
                KeyCode::Enter if form.focus == 4 => return Outcome::Close,
                KeyCode::Enter => return submit_new(app, form),
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => form.focus = step(form.focus, 5, !back && key.code != KeyCode::Up),
                KeyCode::Left | KeyCode::Right if form.focus == 1 => {
                    let i = TodoType::ALL.iter().position(|k| *k == form.kind).unwrap_or(0);
                    form.kind = TodoType::ALL[step(i, TodoType::ALL.len(), key.code == KeyCode::Right)];
                }
                KeyCode::Left | KeyCode::Right if form.focus == 2 => {
                    let i = Priority::ALL.iter().position(|p| *p == form.priority).unwrap_or(0);
                    form.priority = Priority::ALL[step(i, Priority::ALL.len(), key.code == KeyCode::Right)];
                }
                KeyCode::Left | KeyCode::Right if form.focus >= 3 => form.focus = if form.focus == 3 { 4 } else { 3 },
                _ if form.focus == 0 => {
                    form.title.handle(&key);
                    form.error = None;
                }
                _ => {}
            }
            Outcome::Keep
        }
        Dialog::Checks(checks) => {
            let rows = checks.rows();
            match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Up => {
                    checks.sel = checks.sel.saturating_sub(1);
                    Outcome::Keep
                }
                KeyCode::Down | KeyCode::Tab => {
                    checks.sel = (checks.sel + 1).min(rows.saturating_sub(1));
                    Outcome::Keep
                }
                KeyCode::Char(' ') => {
                    let row = checks.sel;
                    toggle(checks, row);
                    Outcome::Keep
                }
                KeyCode::Enter if checks.new_board && checks.sel == checks.items.len() => {
                    let group = match &checks.what {
                        ChecksFor::GroupBoards(group) => group.clone(),
                        ChecksFor::BoardGroups(_) => return Outcome::Keep,
                    };
                    save_checks(app, checks);
                    new_group_board(app, &group);
                    Outcome::Close
                }
                KeyCode::Enter => save_checks(app, checks),
                _ => Outcome::Keep,
            }
        }
        Dialog::Body { id } => {
            let id = id.clone();
            if key.code == KeyCode::Esc {
                return close_body(app);
            }
            let policy = app.key_policy;
            let outcome = match app.tasks.body.as_mut() {
                Some((_, editor)) => editor.key(&policy.edit(key)),
                None => return Outcome::Close,
            };
            match outcome {
                crate::editor::Outcome::Save => save_body(app, &id, false),
                crate::editor::Outcome::Copy(text) | crate::editor::Outcome::Cut(text) => {
                    app.copy_to_clipboard(&text);
                    Outcome::Keep
                }
                _ => Outcome::Keep,
            }
        }
        Dialog::Peek { id } => {
            if key.code == KeyCode::Enter {
                flows::show_todo(app, &id.clone(), true);
            }
            Outcome::Close
        }
    }
}

/// Ticks or unticks a checkbox row.
fn toggle(checks: &mut Checks, row: usize) {
    if let Some(item) = checks.items.get_mut(row) {
        item.3 = !item.3;
    }
    checks.sel = row.min(checks.rows().saturating_sub(1));
}

pub fn mouse(app: &mut App, dialog: &mut Dialog, mouse: MouseEvent, hit: Option<AppHit>) -> Outcome {
    let down = mouse.kind == MouseEventKind::Down(MouseButton::Left);
    match dialog {
        Dialog::Info { .. } => match (down, hit) {
            (true, Some(AppHit::Button(0))) => Outcome::Close,
            _ => Outcome::Keep,
        },
        Dialog::Prompt(prompt) => match (down, hit) {
            (true, Some(AppHit::Button(0))) => submit_prompt(app, prompt),
            (true, Some(AppHit::Button(1))) => Outcome::Close,
            _ => Outcome::Keep,
        },
        Dialog::NewTodo(form) => {
            if !down {
                return Outcome::Keep;
            }
            match hit {
                Some(AppHit::Button(0)) => return submit_new(app, form),
                Some(AppHit::Button(1)) => return Outcome::Close,
                Some(AppHit::Toggle("title")) => form.focus = 0,
                Some(AppHit::ListRow(i)) => {
                    form.kind = TodoType::ALL.get(i).copied().unwrap_or(form.kind);
                    form.focus = 1;
                }
                Some(AppHit::Chip(i)) => {
                    form.priority = Priority::ALL.get(i).copied().unwrap_or(form.priority);
                    form.focus = 2;
                }
                _ => {}
            }
            Outcome::Keep
        }
        Dialog::Checks(checks) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                checks.sel = checks.sel.saturating_sub(1);
                Outcome::Keep
            }
            MouseEventKind::ScrollDown => {
                checks.sel = (checks.sel + 1).min(checks.rows().saturating_sub(1));
                Outcome::Keep
            }
            MouseEventKind::Down(MouseButton::Left) => match hit {
                Some(AppHit::Button(0)) => save_checks(app, checks),
                Some(AppHit::Button(1)) => Outcome::Close,
                Some(AppHit::ListRow(row)) if checks.new_board && row == checks.items.len() => {
                    let ChecksFor::GroupBoards(group) = checks.what.clone() else { return Outcome::Keep };
                    save_checks(app, checks);
                    new_group_board(app, &group);
                    Outcome::Close
                }
                Some(AppHit::ListRow(row)) => {
                    toggle(checks, row);
                    Outcome::Keep
                }
                _ => Outcome::Keep,
            },
            _ => Outcome::Keep,
        },
        Dialog::Body { id } => {
            let id = id.clone();
            let area = app.tasks.body_area;
            let inside = crate::paint::rect_contains(area, mouse.column, mouse.row);
            match (mouse.kind, hit) {
                (MouseEventKind::Down(MouseButton::Left), Some(AppHit::Button(0))) => return save_body(app, &id, true),
                (MouseEventKind::Down(MouseButton::Left), Some(AppHit::Button(1))) => return close_body(app),
                _ => {}
            }
            if let Some((_, editor)) = app.tasks.body.as_mut() {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) if inside => editor.click(area, mouse.column, mouse.row, mouse.modifiers.contains(Modifiers::SHIFT)),
                    MouseEventKind::Drag(MouseButton::Left) => editor.drag(area, mouse.column, mouse.row),
                    MouseEventKind::ScrollUp => editor.scroll(-3),
                    MouseEventKind::ScrollDown => editor.scroll(3),
                    _ => {}
                }
            }
            Outcome::Keep
        }
        Dialog::Peek { .. } => {
            if matches!(mouse.kind, MouseEventKind::Down(_)) { Outcome::Close } else { Outcome::Keep }
        }
    }
}

pub fn text(app: &mut App, dialog: &mut Dialog, text: &str) {
    match dialog {
        Dialog::Prompt(prompt) => {
            prompt.input.insert(text);
            prompt.error = None;
        }
        Dialog::NewTodo(form) => {
            form.focus = 0;
            form.title.insert(text);
            form.error = None;
        }
        Dialog::Body { .. } => {
            if let Some((_, editor)) = app.tasks.body.as_mut() {
                editor.insert_text(text);
            }
        }
        _ => {}
    }
}
