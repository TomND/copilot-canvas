//! What the Tasks space does: its actions, showing todos and boards,
//! menus and pickers, and the longer flows (a session on a todo, its spec
//! note, linking boards and groups, the mirror and the JSON Canvas export).

use canvas_model::todos::{AddPolicy, AgentPolicy, LinkKind, Priority, Status, TodoOp, TodoType, TodoUpdate};

use super::book::{self, book, type_glyph};
use super::forms::{Ask, Checks, NewTodoForm};
use super::layout::Chip;
use super::{Action, Dialog, canvas};
use crate::app::{App, AppEvent};
use crate::dialogs::{Action as AppAction, Confirm, MenuItem, Modal, TextInput};
use crate::doc::{GRID_X, NodeKind};
use crate::paint::truncate;
use crate::side::RightDetail;
use crate::spaces::{NavTarget, Space};

fn item(label: &str, hint: &str, action: Action, enabled: bool) -> MenuItem {
    MenuItem { label: label.to_owned(), hint: hint.to_owned(), action: AppAction::Tasks(action), enabled }
}

fn op(op: TodoOp) -> Action {
    Action::Op(Box::new(op))
}

fn update(id: &str) -> TodoUpdate {
    TodoUpdate { id: id.to_owned(), ..Default::default() }
}

/// Where menus open from buttons: the pointer.
fn pointer(app: &App) -> (u16, u16) {
    (app.pointer.0.max(0.0) as u16, app.pointer.1.max(0.0) as u16)
}

pub(crate) fn push(app: &mut App, dialog: Dialog) {
    app.modals.push(Modal::Tasks(Box::new(dialog)));
    app.needs_draw = true;
}

pub fn run(app: &mut App, action: Action) {
    match action {
        Action::OpenGroup(group) => {
            app.tasks.last_group = Some(group.clone());
            match book(app).and_then(|b| b.boards_of_group(&group).first().map(|b| b.id.clone())) {
                Some(board) => app.navigate(Space::Tasks, NavTarget::Board(board)),
                None => link_boards(app, &group),
            }
        }
        Action::OpenTodo(id) => app.navigate(Space::Tasks, NavTarget::Todo(id)),
        Action::LinkBoards(group) => link_boards(app, &group),
        Action::Select(id) => show_todo(app, &id, true),
        Action::OpenBoard(id) => show_board(app, &id),
        Action::Op(o) => {
            book::apply(app, *o);
        }
        Action::NewBoard => ask(app, Ask::NewBoard { group: None, parent: None }),
        Action::NewTodo { lane, status } => push(app, Dialog::NewTodo(NewTodoForm::new(&lane, status))),
        Action::TodoMenu(id) => todo_menu(app, &id, pointer(app)),
        Action::PickType(id) => pick_type(app, &id),
        Action::PickPriority(id) => pick_priority(app, &id),
        Action::PickStatus(id) => pick_status(app, &id),
        Action::Ask(what) => ask(app, what),
        Action::ConfirmCancel(id) => confirm_cancel(app, &id),
        Action::EditBody(id) => edit_body(app, &id),
        Action::StartSession(id) => start_session(app, &id),
        Action::SpecNote(id) => spec_note(app, &id),
        Action::ClaimFor(id) => claim_for(app, &id),
        Action::BoardMenu(id) => board_menu(app, &id, pointer(app)),
        Action::LinkGroups(board) => link_groups(app, &board),
        Action::ConfirmDeleteBoard(board) => confirm_delete_board(app, &board),
        Action::Mirror => with_root(app, |app, root| ask(app, Ask::Mirror { default: root.map(|r| join(&r, &[canvas_model::notebook::TODOS, "Backlog.md"])) })),
        Action::Export(board) => {
            let name = book(app).and_then(|b| b.board(&board)).map(|b| canvas_model::notebook::safe_name(&b.name, "Board")).unwrap_or_else(|| "Board".into());
            with_root(app, move |app, root| ask(app, Ask::Export { board, default: root.map(|r| join(&r, &[canvas_model::notebook::TODOS, &format!("{name}.canvas")])) }));
        }
        Action::ViewSession(id) => {
            if app.doc.node(&id).is_some() {
                app.navigate(Space::Canvas, NavTarget::Node(id));
            } else {
                app.notify("That session is no longer on this canvas.", true);
            }
        }
        Action::OpenNote(path) => app.navigate(Space::Notebook, NavTarget::Note(path)),
        Action::OpenUrl(url) => app.modals.push(Modal::Confirm(Confirm {
            title: "Open this link?".into(),
            body: vec![truncate(&url, 100), "It opens in your browser on this machine.".into()],
            buttons: vec![("Open".into(), AppAction::Report(crate::reports::ReportAction::OpenUrl(url))), ("Cancel".into(), AppAction::Nothing)],
            sel: 0,
        })),
        Action::Zoom(delta) => canvas::zoom(app, delta),
        Action::Fit => canvas::fit(app),
        Action::TypeFilter(kind) => {
            app.tasks.kind = kind;
            app.tasks.side_sel = 0;
            app.tasks.side_scroll = 0;
        }
        Action::PickTypeFilter => {
            let mut items = vec![item("All types", if app.tasks.kind.is_none() { "●" } else { "" }, Action::TypeFilter(None), true)];
            for kind in TodoType::ALL {
                items.push(item(&format!("{} {}", type_glyph(kind), kind.word()), if app.tasks.kind == Some(kind) { "●" } else { "" }, Action::TypeFilter(Some(kind)), true));
            }
            let at = pointer(app);
            app.open_menu(at.0, at.1, ("Show todos of type".into(), items));
        }
        Action::Peek(id) => push(app, Dialog::Peek { id }),
        Action::CloseDetail => {
            if matches!(app.right_detail, Some(RightDetail::Todo(_))) {
                app.right_detail = None;
            }
            // The pane would show the selected todo again (its ✕ would do nothing): the selection goes, as with Esc.
            app.tasks.selected = None;
        }
    }
    app.needs_draw = true;
}

/// Selects a todo: its card is lit, `N` adds next to it; `details` shows
/// them on the right, `reveal` pans its card into view.
pub fn select_todo(app: &mut App, id: &str, details: bool, reveal_card: bool) {
    let found = book(app).and_then(|b| b.todo(id).map(|t| (t.id.clone(), t.board.clone(), t.status, b.root(&t.board).map(|r| r.id.clone()))));
    let id = match found {
        Some((id, lane, status, root)) => {
            app.tasks.cell = Some((lane, status));
            app.tasks.board = root;
            id
        }
        None => id.to_owned(),
    };
    app.tasks.selected = Some(id.clone());
    if details {
        if app.right_detail.as_ref() != Some(&RightDetail::Todo(id.clone())) {
            app.tasks.detail_scroll = 0;
        }
        app.right_detail = Some(RightDetail::Todo(id.clone()));
    }
    if reveal_card {
        reveal(app, &id);
    }
    app.needs_draw = true;
}

/// Selects a todo and shows its details.
pub fn show_todo(app: &mut App, id: &str, reveal_card: bool) {
    select_todo(app, id, true, reveal_card);
}

/// Pans a todo's card (else its board) into view when the canvas is drawn next.
pub fn reveal(app: &mut App, id: &str) {
    app.tasks.reveal = Some(canvas::Reveal::Todo(id.to_owned()));
}

/// Shows a board: selected, its frame in view.
pub fn show_board(app: &mut App, id: &str) {
    let Some(root) = book(app).and_then(|b| b.root(id)).map(|b| b.id.clone()) else {
        return app.notify(format!("There is no todo board {id} on this canvas."), true);
    };
    app.tasks.board = Some(root.clone());
    app.tasks.reveal = Some(canvas::Reveal::Board(root));
    app.needs_draw = true;
}

/// A chip on a board's title row.
pub fn chip(app: &mut App, board: &str, chip: Chip) {
    match chip {
        Chip::Group(group) => app.navigate(Space::Canvas, NavTarget::Group(group)),
        Chip::NoGroup => link_groups(app, board),
        Chip::Notes(path) => app.navigate(Space::Notebook, NavTarget::Folder(path)),
        Chip::NoNotes => ask(app, Ask::Notes(board.to_owned())),
    }
}

/// A todo's menu (right-click on its card or row, `≡` in its details).
pub fn todo_menu(app: &mut App, id: &str, at: (u16, u16)) {
    let Some(todo) = book(app).and_then(|b| b.todo(id)).cloned() else { return };
    let id = todo.id.clone();
    let (status, proposal) = (todo.status, todo.status == Status::Proposed);
    let open = !status.closed() && !proposal;
    let mut items = vec![item("Show details", "Enter", Action::Select(id.clone()), true), item("Peek", "Space", Action::Peek(id.clone()), true)];
    if proposal {
        items.push(item("Accept", "A", op(TodoOp::Accept { id: id.clone(), status: None }), true));
        items.push(item("Decline", "D", op(TodoOp::Decline { id: id.clone(), reason: None }), true));
        items.push(item("Decline with a reason…", "", Action::Ask(Ask::Decline(id.clone())), true));
    }
    items.extend([
        item("Start a session on this…", "", Action::StartSession(id.clone()), open),
        item("Open spec note", "", Action::SpecNote(id.clone()), true),
        item("Claim for…", "", Action::ClaimFor(id.clone()), open),
        item("Release", "", op(TodoOp::Release { id: id.clone() }), todo.claimed_by.is_some()),
        item("Done", "", op(TodoOp::Done { id: id.clone(), note: None }), open),
        item("Cancel…", "Del", Action::ConfirmCancel(id.clone()), !status.closed()),
        item("Reopen", "", op(TodoOp::Reopen { id: id.clone() }), status.closed()),
        item("Rename…", "R", Action::Ask(Ask::RenameTodo(id.clone())), true),
        item("Edit body…", "E", Action::EditBody(id.clone()), true),
        item("Type…", "", Action::PickType(id.clone()), true),
        item("Priority…", "", Action::PickPriority(id.clone()), true),
        item("Status…", "", Action::PickStatus(id.clone()), !proposal),
        item("Add a note…", "", Action::Ask(Ask::Note(id.clone())), true),
    ]);
    app.open_menu(at.0, at.1, (format!("{} {}", id, truncate(&todo.title, 30)), items));
}

fn pick_type(app: &mut App, id: &str) {
    let Some(current) = book(app).and_then(|b| b.todo(id)).map(|t| t.kind) else { return };
    let items = TodoType::ALL.iter().map(|k| item(&format!("{} {}", type_glyph(*k), k.word()), if *k == current { "●" } else { "" }, op(TodoOp::Update(Box::new(TodoUpdate { kind: Some(*k), ..update(id) }))), true)).collect();
    let at = pointer(app);
    app.open_menu(at.0, at.1, (format!("Type of {id}"), items));
}

fn pick_priority(app: &mut App, id: &str) {
    let Some(current) = book(app).and_then(|b| b.todo(id)).map(|t| t.priority) else { return };
    let items = Priority::ALL
        .iter()
        .map(|p| {
            let label = if p.marks().is_empty() { p.word().to_owned() } else { format!("{} {}", p.marks(), p.word()) };
            item(&label, if *p == current { "●" } else { "" }, op(TodoOp::Update(Box::new(TodoUpdate { priority: Some(*p), ..update(id) }))), true)
        })
        .collect();
    let at = pointer(app);
    app.open_menu(at.0, at.1, (format!("Priority of {id}"), items));
}

fn pick_status(app: &mut App, id: &str) {
    let Some((current, names)) = book(app).and_then(|b| b.todo(id).map(|t| (t.status, b.root(&t.board).cloned()))) else { return };
    let name = |s: Status| names.as_ref().map(|b| b.status_name(s)).unwrap_or_else(|| s.label().to_owned());
    let items = [Status::Backlog, Status::Todo, Status::InProgress, Status::InReview, Status::Done, Status::Canceled]
        .into_iter()
        .map(|s| {
            let action = match s {
                Status::Canceled => Action::ConfirmCancel(id.to_owned()),
                Status::Done => op(TodoOp::Done { id: id.to_owned(), note: None }),
                _ => op(TodoOp::Move { id: id.to_owned(), status: Some(s), board: None, after: None, before: None, rank: None }),
            };
            item(&name(s), if s == current { "●" } else { "" }, action, s != current)
        })
        .collect();
    let at = pointer(app);
    app.open_menu(at.0, at.1, (format!("Status of {id}"), items));
}

/// A board's menu (right-click on its frame, `≡` in its panel).
pub fn board_menu(app: &mut App, board: &str, at: (u16, u16)) {
    let Some((b, todos)) = book(app).and_then(|bk| bk.board(board).map(|b| (b.clone(), bk.todos_in(board).count()))) else { return };
    let top = b.parent.is_none();
    let policy = b.policy;
    let proposals = policy.agent_adds == AddPolicy::Proposed;
    let set_policy = |p: AgentPolicy| op(TodoOp::UpdateBoard { board: b.id.clone(), name: None, prefix: None, wip: None, color: None, notes: None, policy: Some(p), statuses: None });
    let items = vec![
        item("New todo…", "N", Action::NewTodo { lane: b.id.clone(), status: Status::Backlog }, true),
        item("Rename…", "", Action::Ask(Ask::RenameBoard(b.id.clone())), true),
        item("Prefix…", &b.prefix, Action::Ask(Ask::Prefix(b.id.clone())), top),
        item("WIP limit…", &b.wip.map(|w| w.to_string()).unwrap_or_default(), Action::Ask(Ask::Wip(b.id.clone())), top),
        item(if proposals { "Agents' todos: proposals" } else { "Agents' todos: added directly" }, "switch", set_policy(AgentPolicy { agent_adds: if proposals { AddPolicy::Direct } else { AddPolicy::Proposed }, ..policy }), top),
        item(if policy.agents_may_close { "Agents may close todos: yes" } else { "Agents may close todos: no" }, "switch", set_policy(AgentPolicy { agents_may_close: !policy.agents_may_close, ..policy }), top),
        item("Link canvas groups…", "", Action::LinkGroups(b.id.clone()), true),
        item("Link notes folder…", "", Action::Ask(Ask::Notes(b.id.clone())), true),
        item("New swimlane…", "", Action::Ask(Ask::NewBoard { group: None, parent: Some(b.id.clone()) }), top),
        item("Mirror file…", "", Action::Mirror, true),
        item("Export JSON Canvas…", "", Action::Export(b.id.clone()), true),
        item("Delete board", if todos > 0 { "empty boards only" } else { "" }, Action::ConfirmDeleteBoard(b.id.clone()), todos == 0),
    ];
    app.tasks.board = Some(book(app).and_then(|bk| bk.root(board)).map_or(b.id.clone(), |r| r.id.clone()));
    app.open_menu(at.0, at.1, (b.name.clone(), items));
}

/// Right-click on the todo canvas's background.
pub fn canvas_menu(app: &mut App, at: (u16, u16)) {
    let items = vec![
        item("New board…", "", Action::NewBoard, true),
        item("Show all boards", "F", Action::Fit, true),
        item("Zoom in", "+", Action::Zoom(1), app.tasks.zoom > 0),
        item("Zoom out", "-", Action::Zoom(-1), app.tasks.zoom < 2),
        item("Mirror file…", "", Action::Mirror, book(app).is_some()),
    ];
    app.open_menu(at.0, at.1, ("Todo boards".into(), items));
}

fn confirm_cancel(app: &mut App, id: &str) {
    let Some(title) = book(app).and_then(|b| b.todo(id)).map(|t| t.title.clone()) else { return };
    app.modals.push(Modal::Confirm(Confirm {
        title: format!("Cancel {id}?"),
        body: vec![truncate(&title, 70), "It moves out of the board (Reopen brings it back).".into()],
        buttons: vec![("Cancel todo".into(), AppAction::Tasks(op(TodoOp::Cancel { id: id.to_owned(), reason: None, resolution: None }))), ("Keep it".into(), AppAction::Nothing)],
        sel: 1,
    }));
}

fn confirm_delete_board(app: &mut App, board: &str) {
    let Some((name, todos)) = book(app).and_then(|b| b.board(board).map(|x| (x.name.clone(), b.todos_in(board).count()))) else { return };
    if todos > 0 {
        return app.notify(format!("“{name}” has {todos} todo(s): move or delete them first."), true);
    }
    app.modals.push(Modal::Confirm(Confirm {
        title: format!("Delete the board “{name}”?"),
        body: vec!["It is empty; its swimlanes go with it.".into()],
        buttons: vec![("Delete board".into(), AppAction::Tasks(op(TodoOp::DeleteBoard { board: board.to_owned(), with_todos: false }))), ("Keep it".into(), AppAction::Nothing)],
        sel: 1,
    }));
}

/// Who claims a todo: you, or a session of the canvas you assign it to
/// (its claim; taking it over from another session if need be). Nothing
/// starts: tell the session, or start one on it.
fn claim_for(app: &mut App, id: &str) {
    let claim = |session: Option<String>| op(TodoOp::Claim { id: id.to_owned(), force: true, session });
    let holder = book(app).and_then(|b| b.todo(id)).and_then(|t| t.claim_session().map(str::to_owned));
    let mut items = vec![item("You", "claim it now", claim(None), true)];
    for node in app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && n.passive.is_none()) {
        let hint = if holder.as_deref() == Some(node.id.as_str()) { "has it" } else { "" };
        items.push(item(&format!("◫ {}", truncate(&node.name, 36)), hint, claim(Some(node.id.clone())), hint.is_empty()));
    }
    let at = pointer(app);
    app.open_menu(at.0, at.1, (format!("Claim {id} for"), items));
}

/// Checkboxes of the boards linked to a group, and a new board for it.
pub fn link_boards(app: &mut App, group: &str) {
    if app.doc.group(group).is_none() {
        return app.notify("That group is no longer on the canvas.", true);
    }
    match Checks::group_boards(app, group) {
        Some(checks) => push(app, Dialog::Checks(checks)),
        None => app.notify("The todo boards are not loaded yet (the service has not sent them).", true),
    }
}

fn link_groups(app: &mut App, board: &str) {
    if let Some(checks) = Checks::board_groups(app, board) {
        push(app, Dialog::Checks(checks));
    }
}

/// The body in the editor (a dialog; Ctrl+S or Save keeps it).
fn edit_body(app: &mut App, id: &str) {
    let Some((id, body)) = book(app).and_then(|b| b.todo(id)).map(|t| (t.id.clone(), t.body.clone())) else { return };
    let mut editor = crate::editor::Editor::new(&body);
    editor.set_mode(crate::editor::Mode::Source);
    app.tasks.body = Some((id.clone(), editor));
    push(app, Dialog::Body { id });
}

/// Opens a prompt for `what`.
pub fn ask(app: &mut App, what: Ask) {
    if let Some(prompt) = super::forms::Prompt::new(app, what) {
        push(app, Dialog::Prompt(prompt));
    }
}

/// The first prompt of a session started on a todo.
pub fn start_prompt(id: &str, title: &str) -> String {
    format!("Work on {id} ({title}). Run `copilot-canvas todo show {id}`, then `copilot-canvas todo claim {id}`.")
}

/// A free spot for a new session box in a group.
fn group_slot(app: &App, group: &str) -> Option<(i32, i32)> {
    let g = app.doc.group(group)?;
    let columns = ((g.w - 2) / GRID_X).max(1);
    Some(app.doc.free_slot(g.x + 2, g.y + 2, columns, NodeKind::Session))
}

/// *Start a session on this…*: the new-session dialog, in the board's
/// linked group, named after the todo, with a first prompt to show and
/// claim it. Nothing starts before you confirm the dialog.
pub fn start_session(app: &mut App, id: &str) {
    let Some((id, title, group)) = book(app).and_then(|b| b.todo(id).map(|t| (t.id.clone(), t.title.clone(), b.event_group(&t.board).map(str::to_owned)))) else {
        return app.notify(format!("There is no todo {id}."), true);
    };
    let group = group.filter(|g| app.doc.group(g).is_some());
    let at = group.as_deref().and_then(|g| group_slot(app, g));
    app.open_new_session(at, None);
    if let Some(Modal::NewSession(dialog)) = app.modals.last_mut() {
        dialog.name = TextInput::new(&format!("{id} {title}").chars().take(60).collect::<String>());
        dialog.prompt = TextInput::new(&start_prompt(&id, &title));
        if group.is_some() {
            dialog.group = group;
        }
    }
}

/// `root` joined with more parts, with the separator `root` uses.
pub fn join(root: &str, parts: &[&str]) -> String {
    let sep = if root.contains('/') && !root.contains('\\') { "/" } else { "\\" };
    let mut out = root.trim_end_matches(['\\', '/']).to_owned();
    for part in parts {
        out.push_str(sep);
        out.push_str(part);
    }
    out
}

/// A todo's spec note: `<root>/Todos/<ID> - <title>.md`.
pub fn spec_note_path(root: &str, id: &str, title: &str) -> String {
    join(root, &[canvas_model::notebook::TODOS, &format!("{}.md", canvas_model::notebook::safe_name(&format!("{id} - {title}"), id))])
}

/// Runs `then` with the notebook's root on the service's machine (asked on
/// a thread; `None` when the backend cannot say).
fn with_root(app: &mut App, then: impl FnOnce(&mut App, Option<String>) + Send + 'static) {
    let setting = app.doc.notebook_root.clone().filter(|r| !r.trim().is_empty());
    if app.demo {
        return then(app, setting);
    }
    let (backend, tx, canvas) = (app.backend.clone(), app.app_tx.clone(), app.doc.canvas_id.clone());
    std::thread::spawn(move || {
        let root = backend.notebook_info(setting.as_deref(), canvas.as_deref()).ok().map(|i| i.root).or(setting);
        let _ = tx.send(AppEvent::Run(Box::new(move |app: &mut App| then(app, root))));
    });
}

/// *Open spec note*: the linked note, else `<root>/Todos/<ID> - <title>.md`
/// (created when missing, with the todo's body), linked, then opened.
pub fn spec_note(app: &mut App, id: &str) {
    let Some(todo) = book(app).and_then(|b| b.todo(id)).cloned() else { return };
    if let Some(note) = todo.note.clone() {
        return app.navigate(Space::Notebook, NavTarget::Note(note));
    }
    if app.demo {
        return app.notify("The demo has no notebook to keep spec notes in.", false);
    }
    let (backend, tx) = (app.backend.clone(), app.app_tx.clone());
    let (setting, canvas, mounts) = (app.doc.notebook_root.clone(), app.doc.canvas_id.clone(), app.doc.notebook_mounts.clone());
    std::thread::spawn(move || {
        let made = (|| -> Result<String, String> {
            use canvas_model::notebook::{Kind, NewNote, Saved, new_note_text};
            let info = backend.notebook_info(setting.as_deref(), canvas.as_deref()).map_err(|e| format!("{e:#}"))?;
            let path = spec_note_path(&info.root, &todo.id, &todo.title);
            let title = format!("{} - {}", todo.id, todo.title);
            let note = NewNote { title, created: crate::paths::rfc3339_now(), extra: vec![("todo".into(), todo.id.clone())], ..Default::default() };
            let text = new_note_text(Kind::Markdown, &todo.body, &note);
            let mut roots = vec![info.root.clone()];
            roots.extend(mounts);
            // A note there already (a conflict for a new one) is linked as it is.
            match backend.note_write(&path, &text, None, &roots).map_err(|e| format!("{e:#}"))? {
                Saved::Written(_) | Saved::Conflict(_) => Ok(path),
            }
        })();
        let id = todo.id.clone();
        let _ = tx.send(AppEvent::Run(Box::new(move |app: &mut App| match made {
            Ok(path) => {
                book::apply(app, TodoOp::Link { id, kind: LinkKind::Note, target: path.clone(), title: Some("spec".into()) });
                app.navigate(Space::Notebook, NavTarget::Note(path));
            }
            Err(why) => app.notify(format!("Could not create the spec note: {why}"), true),
        })));
    });
}

/// Writes a board as JSON Canvas to `path` on the service's machine (inside
/// the notebook's folders; an existing file is replaced).
pub fn export(app: &mut App, board: &str, path: String) {
    let json = match book(app).ok_or_else(|| "the todo boards are not loaded".to_owned()).and_then(|b| canvas_model::todos::json_canvas(b, Some(board))) {
        Ok(json) => serde_json::to_string_pretty(&json).unwrap_or_default(),
        Err(why) => return app.notify(format!("Could not export: {why}"), true),
    };
    if app.demo {
        return app.notify("The demo has no notebook to export to.", false);
    }
    let (backend, tx) = (app.backend.clone(), app.app_tx.clone());
    let (setting, canvas, mounts) = (app.doc.notebook_root.clone(), app.doc.canvas_id.clone(), app.doc.notebook_mounts.clone());
    std::thread::spawn(move || {
        let wrote = (|| -> Result<(), String> {
            use canvas_model::notebook::Saved;
            let info = backend.notebook_info(setting.as_deref(), canvas.as_deref()).map_err(|e| format!("{e:#}"))?;
            let mut roots = vec![info.root];
            roots.extend(mounts);
            match backend.note_write(&path, &json, None, &roots).map_err(|e| format!("{e:#}"))? {
                Saved::Written(_) => Ok(()),
                Saved::Conflict(current) => match backend.note_write(&path, &json, Some(&current.rev), &roots).map_err(|e| format!("{e:#}"))? {
                    Saved::Written(_) => Ok(()),
                    Saved::Conflict(_) => Err("the file changed while it was written".into()),
                },
            }
        })();
        let notice = match wrote {
            Ok(()) => AppEvent::Notice { text: format!("Exported the board to {path}."), error: false },
            Err(why) => AppEvent::Notice { text: format!("Could not export to {path}: {why} (JSON Canvas files go inside the notebook's folders)."), error: true },
        };
        let _ = tx.send(notice);
    });
}
