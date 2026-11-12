//! The todo book this window shows and changes: the service's copy
//! (`app.bus.todos`, sent on connect and after every change) or, in demo
//! mode, a small demo book; operations applied at once here (optimistic)
//! and sent to the service ([`apply`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use canvas_model::todos::{self, Actor, Applied, Ctx, Status, Todo, TodoBook, TodoOp, TodoType};
use ratatui::style::Color;

use crate::app::App;
use crate::doc::NodeKind;
use crate::spaces::{GoItem, NavTarget, Space};

/// The book the window shows: the service's, else the demo book in demo mode.
pub fn book(app: &App) -> Option<&TodoBook> {
    app.bus.todos.as_ref().or_else(|| app.demo.then(|| app.tasks.demo.get_or_init(todos::demo_book)))
}

/// The book to change (demo mode: the demo book becomes the window's own).
fn book_mut(app: &mut App) -> Option<&mut TodoBook> {
    if app.bus.todos.is_none() && app.demo {
        app.bus.todos = Some(app.tasks.demo.get().cloned().unwrap_or_else(todos::demo_book));
    }
    app.bus.todos.as_mut()
}

/// How long a change sent to the service is put back on top of the books it
/// sends that do not show it yet; then the service's word stands.
pub(crate) const PENDING: Duration = Duration::from_secs(5);

/// A change applied here and sent to the service ([`apply`]).
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub op: TodoOp,
    /// The book's revision with it applied here. The service applies a
    /// window's changes in order, one revision each, so its book has this
    /// change only from this revision on (other windows' changes may get
    /// there first: such a book is taken as having it).
    pub after: u64,
    pub sent: Instant,
}

/// A book from the service: shown with your changes it cannot have yet put
/// back on top (in the order they were made), so a book sent before the
/// service had them (the broadcast after an earlier change, or the bus
/// loop's pass that read the book a moment before) never undoes them for a
/// moment. Changes it refuses come with an error first ([`refused`]).
pub fn receive(app: &mut App, served: Option<TodoBook>) -> Option<TodoBook> {
    app.tasks.book_gen += 1;
    expire(app);
    if app.tasks.pending.is_empty() || served.is_none() {
        app.tasks.pending.clear();
        app.tasks.served = None;
        return served;
    }
    app.tasks.served = served.clone();
    let shown = served.map(|book| rebase(app, book));
    if app.tasks.pending.is_empty() {
        app.tasks.served = None;
    }
    shown
}

/// `book` with the pending changes it does not have put on top (one that
/// no longer applies is dropped: the service's book stands).
fn rebase(app: &mut App, mut book: TodoBook) -> TodoBook {
    let ctx = ctx(app);
    let rev = book.rev;
    app.tasks.pending.retain(|p| p.after <= rev || book.apply(p.op.clone(), &Actor::human(), &ctx).is_ok());
    book
}

/// The service refused a change of this window (its error comes before the
/// book as it is): the pending changes go and the window shows the
/// service's last book at once.
pub fn refused(app: &mut App) {
    app.tasks.pending.clear();
    if let Some(served) = app.tasks.served.take() {
        app.bus.todos = Some(served);
        app.tasks.book_gen += 1;
        app.needs_draw = true;
    }
}

/// Drops changes sent too long ago (the service never sent them back, or
/// its books had them long since). True when some went.
fn expire(app: &mut App) -> bool {
    let before = app.tasks.pending.len();
    app.tasks.pending.retain(|p| p.sent.elapsed() < PENDING);
    app.tasks.pending.len() != before
}

/// Every UI pass: changes the service never confirmed go after a while,
/// and the window shows its last book with the rest.
pub(crate) fn tick(app: &mut App) {
    if app.tasks.pending.is_empty() || !expire(app) {
        return;
    }
    let shown = match app.tasks.served.clone() {
        Some(served) if !app.tasks.pending.is_empty() => Some(rebase(app, served)),
        served => served,
    };
    if app.tasks.pending.is_empty() {
        app.tasks.served = None;
    }
    if shown.is_some() {
        app.bus.todos = shown;
        app.tasks.book_gen += 1;
        app.needs_draw = true;
    }
}

/// The session that claimed a todo is gone (no longer on the canvas),
/// stopped or interrupted (once the sessions are known): its claim is stale.
pub fn session_gone(app: &App, session: &str) -> bool {
    match app.doc.node(session) {
        None => true,
        Some(node) => app.sessions_loaded && matches!(app.node_status(node), crate::status::Status::Stopped | crate::status::Status::Failed | crate::status::Status::Interrupted),
    }
}

/// A session works on something right now (its claim shows `⟳ name`).
pub fn session_working(app: &App, session: &str) -> bool {
    app.doc.node(session).is_some_and(|n| app.node_status(n).animated())
}

/// A session's name for people.
pub fn session_name(app: &App, todo: &Todo) -> String {
    let claim = todo.claimed_by.as_ref();
    match claim.and_then(|c| c.by.session.as_deref()).and_then(|s| app.doc.node(s)) {
        Some(node) => node.name.clone(),
        None => claim.map(|c| c.by.label()).unwrap_or_default(),
    }
}

/// The canvas's groups: id → name.
pub fn group_names(app: &App) -> BTreeMap<String, String> {
    app.doc.groups.iter().map(|g| (g.id.clone(), g.name.clone())).collect()
}

/// Where your operations run: now, the canvas's groups, its sessions (a
/// claim for one names it) and the sessions whose claims are live (as the
/// service decides).
pub fn ctx(app: &App) -> Ctx {
    let sessions = app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session);
    let live: BTreeSet<String> = sessions.clone().filter(|n| !session_gone(app, &n.id)).map(|n| n.id.clone()).collect();
    let names: BTreeMap<String, String> = sessions.map(|n| (n.id.clone(), n.name.clone())).collect();
    Ctx { now: crate::paths::now_ms(), group: None, groups: group_names(app), live: Some(live), sessions: names }
}

/// Applies an operation as you: on the window's book at once, then sent to
/// the service, which applies it and sends the book back (until then each
/// book it sends gets the change put back on top: [`receive`]; a refusal
/// comes as an error notice and the service's book undoes it). Demo mode
/// keeps it local. Refused here: nothing is sent.
pub fn apply(app: &mut App, op: TodoOp) -> Option<Applied> {
    let ctx = ctx(app);
    let local = app.demo;
    // The service's book, kept while changes are pending (what is left when they go).
    let first = !local && app.tasks.pending.is_empty();
    let Some(book) = book_mut(app) else {
        app.notify("The todo boards are not loaded yet (the service has not sent them).", true);
        return None;
    };
    let served = first.then(|| book.clone());
    let result = book.apply(op.clone(), &Actor::human(), &ctx);
    let after = book.rev;
    match result {
        Ok(applied) => {
            app.tasks.book_gen += 1;
            if !local {
                if served.is_some() {
                    app.tasks.served = served;
                }
                app.tasks.pending.push(Pending { op: op.clone(), after, sent: Instant::now() });
                app.bus_action(todos::client_action(&op));
            }
            let mut text = applied.message.clone();
            for warning in &applied.warnings {
                text.push_str(&format!(" ⚠ {warning}."));
            }
            app.notify(text, false);
            app.needs_draw = true;
            Some(applied)
        }
        Err(why) => {
            app.notify(why, true);
            None
        }
    }
}

/// A type's glyph on cards and rows (one cell wide).
pub fn type_glyph(kind: TodoType) -> &'static str {
    match kind {
        TodoType::Bug => "✱",
        TodoType::Feature => "✚",
        TodoType::Improvement => "▲",
        TodoType::Optimization => "»",
        TodoType::Chore => "∷",
        TodoType::Docs => "¶",
        TodoType::Research => "?",
    }
}

/// A type's colour (the card's edge and glyph).
pub fn type_color(kind: TodoType) -> Color {
    match kind {
        TodoType::Bug => crate::paint::NEGATIVE,
        TodoType::Feature => crate::paint::POSITIVE,
        TodoType::Improvement => crate::paint::LOOP,
        TodoType::Optimization => Color::Rgb(240, 228, 66),
        TodoType::Chore => crate::paint::GRAY,
        TodoType::Docs => crate::paint::FINISHED,
        TodoType::Research => crate::paint::PURPLE,
    }
}

/// A status's short name (25 % summaries).
pub fn status_short(status: Status) -> &'static str {
    match status {
        Status::Proposed => "Prop",
        Status::Backlog => "Back",
        Status::Todo => "Todo",
        Status::InProgress => "Prog",
        Status::InReview => "Rev",
        Status::Done => "Done",
        Status::Canceled => "Canc",
    }
}

/// The group's chip: the boards linked to it.
pub fn group_counts(app: &App, group: &str) -> Option<super::TodoCounts> {
    let book = book(app)?;
    with_links(app, book, |links| *links.counts.entry(group.to_owned()).or_insert_with(|| book.group_counts(group)))
}

/// The todo a session claimed (its box's `☑ DEC-031`).
pub fn session_badge(app: &App, session: &str) -> Option<String> {
    let book = book(app)?;
    with_links(app, book, |links| links.claims.get(session).map(|(_, id)| id.clone()))
}

/// What the session canvas shows of the book (every box's badge, every
/// group's chip, each frame), worked out once per book rather than by a
/// pass over every todo for each box and group.
#[derive(Clone, Debug, Default)]
pub(crate) struct Links {
    /// The book it was made from (`State::book_gen`, its revision and sizes).
    key: Option<(u64, u64, usize, usize)>,
    /// Session → (claimed at, todo id): its newest claim of a todo not closed.
    claims: HashMap<String, (u64, String)>,
    /// Group → its chip's counts (as they are asked for).
    counts: HashMap<String, Option<super::TodoCounts>>,
}

fn with_links<T>(app: &App, book: &TodoBook, then: impl FnOnce(&mut Links) -> T) -> T {
    let key = Some((app.tasks.book_gen, book.rev, book.todos.len(), book.boards.len()));
    let mut links = app.tasks.links.borrow_mut();
    if links.key != key {
        let mut claims: HashMap<String, (u64, String)> = HashMap::new();
        // As `TodoBook::claimed_by_session`: the newest claim (the later of equals).
        for todo in book.todos.iter().filter(|t| !t.status.closed()) {
            if let (Some(session), Some(claim)) = (todo.claim_session(), todo.claimed_by.as_ref())
                && claims.get(session).is_none_or(|(ms, _)| claim.ms >= *ms)
            {
                claims.insert(session.to_owned(), (claim.ms, todo.id.clone()));
            }
        }
        *links = Links { key, claims, counts: HashMap::new() };
    }
    then(&mut links)
}

/// Go to's boards and todos.
pub fn goto_items(app: &App) -> Vec<GoItem> {
    let Some(book) = book(app) else { return Vec::new() };
    let mut items = Vec::new();
    for board in &book.boards {
        let open = book.todos.iter().filter(|t| t.board == board.id && t.status.open()).count();
        let place = match board.parent.as_deref().and_then(|p| book.board(p)) {
            Some(parent) => format!("swimlane of {}", parent.name),
            None => format!("board · {}", board.prefix),
        };
        items.push(GoItem { label: board.name.clone(), detail: format!("{place} · {open} open"), glyph: "▦", space: Space::Tasks, target: NavTarget::Board(board.id.clone()) });
    }
    for todo in &book.todos {
        let board = book.board(&todo.board).map(|b| b.name.as_str()).unwrap_or_default();
        let status = book.root(&todo.board).map(|b| b.status_name(todo.status)).unwrap_or_else(|| todo.status.label().to_owned());
        items.push(GoItem { label: format!("{} {}", todo.id, todo.title), detail: format!("{board} · {status}"), glyph: "☑", space: Space::Tasks, target: NavTarget::Todo(todo.id.clone()) });
    }
    items
}
