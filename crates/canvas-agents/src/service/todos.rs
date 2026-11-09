//! The canvas's todo boards in the service (docs/NOTEBOOK-TASKS.md §3.2).
//!
//! The service holds the routed canvas's book and is its only writer:
//! `<home>/canvas-agents/<canvas id>/todos/book.json` (atomic writes) and
//! `log.jsonl` (one line per applied or refused operation). Operations come
//! from `atelier todo …` (`canvas.todo` requests through the bus
//! inbox, answered in `bus/answers`; the caller is the event's source, no
//! source: you, or the bot an automation names) and from windows
//! (`{"action": "todos", "op": …}`, applied at once as you; a refusal comes
//! back to that window as an error notice with the book as it is). Clients
//! get the book in `bus::Snapshot::todos`. Applied operations send bus
//! events (`todo.created`, `todo.claimed`, …) to the first canvas group
//! linked to the board, never back to the actor; a merged pull request
//! (`pr.merged` from the PR monitor or a session's history) finishes the
//! todos linked to it; the Markdown mirror is rewritten after every change;
//! the digests of passive runs get a `# Todos` section.
//!
//! Trust: as for `atelier pr` and `review` requests, an inbox request
//! is applied as the session it names and one without a session as you (or
//! the automation it names). Any process running as you can drop such a file,
//! so the rules keep agents that use the commands in their lane; they are no
//! sandbox. What only the service's own word may do stays out of the inbox:
//! `pr.merged` finishes todos only from the PR monitor or a session's history.
//!
//! Safety of the stored book: a book that cannot be read (damaged, or written
//! by a newer version) is never replaced (changes are refused until it reads
//! again); writes are flushed before they replace the old file; a change that
//! would grow the book beyond [`MAX_BOOK_BYTES`] is refused (every client gets
//! the whole book in one frame); a request is applied once even when its
//! inbox file is read twice.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use canvas_host::server::Client;
use canvas_model::todos::{self, Actor, Applied, Ctx, SCHEMA_VERSION, TodoBook, TodoOp};
use serde_json::json;

use super::{Shared, lock, log};
use crate::bus::{Event, Routing};
use crate::paths::now_ms;
use crate::protocol::ServiceMsg;
use crate::prs_cli::{Answer, write_answer};
use crate::todos_cli::REQUEST;

/// The log is renamed to `log.jsonl.old` beyond this size.
const MAX_LOG: u64 = 5 * 1024 * 1024;
/// The stored book's largest size: every client gets the whole book in the
/// bus snapshot, one frame of at most `protocol::MAX_FRAME` (32 MB). A
/// change that would make a book this large grow is refused.
const MAX_BOOK_BYTES: usize = 16 * 1024 * 1024;
/// Request ids remembered against applying an inbox file twice (when it
/// could not be removed after it was read).
const SERVED: usize = 512;

/// The routed canvas's book, held by the service, its only writer.
#[derive(Default)]
pub(super) struct Hub {
    canvas: String,
    book: TodoBook,
    saved: String,
    /// Why the stored book cannot be read: changes are refused (never an
    /// empty book written over it) and clients get none until it reads again.
    unreadable: Option<String>,
    /// Why the stored book must not be written: a newer version wrote it
    /// (clients get it; changes are refused).
    newer: Option<String>,
    /// The ids of the requests served last.
    served: VecDeque<String>,
    /// The size budget (`None`: [`MAX_BOOK_BYTES`]; tests make it small).
    budget: Option<usize>,
}

impl Hub {
    /// Holds `canvas`'s book (the one held before is saved first); a book
    /// that could not be read is read again.
    fn open(&mut self, home: &Path, canvas: &str) {
        if self.canvas == canvas && self.unreadable.is_none() {
            return;
        }
        if self.canvas != canvas {
            let _ = self.save(home);
        }
        self.canvas = canvas.to_owned();
        let read = if crate::doc::valid_canvas_id(canvas) { crate::todos_cli::try_load(home, canvas) } else { Ok(TodoBook::default()) };
        (self.book, self.unreadable, self.newer) = match read {
            // A newer version's book is shown but never written (this one would drop what it does not know).
            Ok(book) if book.version > todos::VERSION => {
                let why = format!("the todo book was written by a newer atelier (book version {}, this service knows {}): update the service to change todos", book.version, todos::VERSION);
                (book, None, Some(why))
            }
            Ok(book) => (book, None, None),
            Err(why) => (TodoBook::default(), Some(why), None),
        };
        self.saved = serde_json::to_string_pretty(&self.book).unwrap_or_default();
    }

    /// Writes the book when it changed (atomically).
    fn save(&mut self, home: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.book).map_err(|e| e.to_string())?;
        self.store(home, text)
    }

    /// Writes the book's text when it changed: atomically and flushed to the
    /// disk before it replaces the old file (never over a book it could not read).
    fn store(&mut self, home: &Path, text: String) -> Result<(), String> {
        if !crate::doc::valid_canvas_id(&self.canvas) || self.unreadable.is_some() || self.newer.is_some() || text == self.saved {
            return Ok(());
        }
        let path = crate::todos_cli::book_path(home, &self.canvas);
        write_durable(&path, text.as_bytes()).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        self.saved = text;
        Ok(())
    }

    /// A request seen before (`true`), else remembered.
    fn seen(&mut self, request: &str) -> bool {
        if request.is_empty() {
            return false;
        }
        if self.served.iter().any(|r| r == request) {
            return true;
        }
        if self.served.len() >= SERVED {
            self.served.pop_front();
        }
        self.served.push_back(request.to_owned());
        false
    }
}

/// A temporary file next to `path`, flushed, then renamed over it: a crash
/// leaves the old book or the new one, never a torn or empty file.
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    written.and_then(|()| std::fs::rename(&temp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// What applying an operation did.
pub(super) struct Outcome {
    pub result: Result<Applied, String>,
    /// The bus events to route.
    pub events: Vec<Event>,
}

/// The canvas's groups by id (names for links and digests).
fn group_names(routing: &Routing) -> BTreeMap<String, String> {
    routing.groups.iter().map(|g| (g.id.clone(), g.name.clone())).collect()
}

/// A group by id or name (case-insensitive); unknown text stays as it is (the model refuses it).
fn group_id(routing: &Routing, text: &str) -> String {
    let text = text.trim();
    routing.group(text).or_else(|| routing.groups.iter().find(|g| g.name.eq_ignore_ascii_case(text))).map(|g| g.id.clone()).unwrap_or_else(|| text.to_owned())
}

/// The canvas's sessions by id (names for claims made for a session): its
/// sessions and passive sessions.
fn session_names(routing: &Routing) -> BTreeMap<String, String> {
    let mut names = routing.names.clone();
    for p in &routing.passive {
        names.entry(p.session_id.clone()).or_insert_with(|| p.name.clone());
    }
    names
}

/// A session by id or name (as `atelier send --to` finds it, from
/// you); unknown text stays as it is (the model refuses it).
fn session_id(routing: &Routing, text: &str) -> String {
    let text = text.trim();
    if routing.names.contains_key(text) || routing.passive_by_session(text).is_some() {
        return text.to_owned();
    }
    match routing.resolve_from(text, "", "") {
        Ok(crate::bus::Target::Session(id)) => id,
        Ok(crate::bus::Target::Passive(key)) => routing.passive.iter().find(|p| p.key == key).map(|p| p.session_id.clone()).unwrap_or_else(|| text.to_owned()),
        Err(_) => text.to_owned(),
    }
}

/// Group and session names in an operation become ids.
fn resolve_groups(routing: &Routing, op: TodoOp) -> TodoOp {
    match op {
        TodoOp::LinkBoard { board, group, unlink } => TodoOp::LinkBoard { board, group: group_id(routing, &group), unlink },
        TodoOp::AddBoard { name, prefix, parent, groups, x, y } => TodoOp::AddBoard { name, prefix, parent, groups: groups.iter().map(|g| group_id(routing, g)).collect(), x, y },
        TodoOp::Claim { id, force, session: Some(session) } => TodoOp::Claim { id, force, session: Some(session_id(routing, &session)) },
        other => other,
    }
}

/// Who an inbox request comes from: its session (an agent), else you, or
/// the automation it names (`bot`).
fn actor_of(routing: &Routing, source: &str, bot: Option<&str>) -> Actor {
    if source.is_empty() {
        return match bot.map(str::trim).filter(|b| !b.is_empty()) {
            Some(name) => Actor::bot(name),
            None => Actor::human(),
        };
    }
    let name = routing.passive_by_session(source).map(|p| p.name.clone()).or_else(|| routing.names.get(source).cloned()).unwrap_or_default();
    Actor::agent(source, &name)
}

/// An applied operation's event on the bus: in the first canvas group
/// linked to the board, from the actor's session (so it never goes back to it).
fn bus_event(book: &TodoBook, routing: &Routing, actor: &Actor, event: &todos::TodoEvent, hop: u32) -> Event {
    Event {
        kind: event.kind.clone(),
        group: book.event_group(&event.board).unwrap_or_default().to_owned(),
        source: actor.session.clone().unwrap_or_default(),
        source_name: if actor.is_agent() { String::new() } else { actor.label() },
        canvas: routing.canvas.clone(),
        data: event.data.clone(),
        hop,
        ..Default::default()
    }
}

/// One line of `log.jsonl`.
fn append_log(home: &Path, canvas: &str, actor: &Actor, op: &TodoOp, result: &Result<Applied, String>, now: u64) {
    let path = crate::todos_cli::todos_dir(home, canvas).join("log.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
    }
    let mut op = serde_json::to_value(op).unwrap_or_default();
    // An import's text is in the file it came from.
    if let Some(text) = op.get("text").and_then(|t| t.as_str()).filter(|t| t.len() > 2_000) {
        op["text"] = format!("<{} bytes>", text.len()).into();
    }
    let line = match result {
        Ok(applied) => json!({ "ms": now, "actor": actor, "op": op, "ok": true, "message": applied.message, "warnings": applied.warnings }),
        Err(why) => json!({ "ms": now, "actor": actor, "op": op, "ok": false, "message": why }),
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(home));
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// The line every mirror starts with (after `# TODOs`).
const MIRROR_MARK: &str = "> Generated by Atelier";

/// Rewrites the Markdown mirror when it is set and its text changed. A file
/// there that is not a mirror (the owner's own TODOs.md) is copied to
/// `<file>.bak` (`<file>.<ms>.bak` when that exists) before it is first
/// replaced; without that copy it is left alone.
fn write_mirror(book: &TodoBook, now: u64) -> Result<(), String> {
    let Some(path) = book.settings.mirror.as_deref().map(Path::new) else { return Ok(()) };
    let text = todos::mirror_markdown(book);
    match std::fs::read(path) {
        Ok(old) if old == text.as_bytes() => return Ok(()),
        Ok(old) if !String::from_utf8_lossy(&old).lines().take(5).any(|l| l.starts_with(MIRROR_MARK)) => {
            let named = |suffix: String| {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                std::path::PathBuf::from(name)
            };
            let backup = [named(".bak".into()), named(format!(".{now}.bak"))].into_iter().find(|p| !p.exists()).ok_or_else(|| format!("the mirror {} is not one the canvas wrote, and its backups exist: move them first", path.display()))?;
            std::fs::copy(path, &backup).map_err(|e| format!("the mirror {} is not one the canvas wrote and could not be copied to {} ({e}): it was left alone", path.display(), backup.display()))?;
        }
        _ => {}
    }
    crate::paths::write_atomic(path, text.as_bytes()).map_err(|e| format!("cannot write the mirror {}: {e}", path.display()))
}

/// When an operation runs: now, the hop of the request that asked for it
/// (its events carry it, so chains stop), and the sessions that run (their
/// claims are live; `None`: every claim is).
pub(super) struct Run {
    pub now: u64,
    pub hop: u32,
    pub live: Option<BTreeSet<String>>,
}

/// Applies an operation to the routed canvas's book as `actor`, then keeps
/// what follows: the stored book, the log line, the mirror, the bus events.
pub(super) fn apply(hub: &mut Hub, home: &Path, routing: &Routing, op: TodoOp, actor: &Actor, run: Run) -> Outcome {
    let Run { now, hop, live } = run;
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return Outcome { result: Err("no canvas with an id is open in Atelier (open the canvas with an up-to-date client)".into()), events: Vec::new() };
    }
    hub.open(home, &routing.canvas);
    let op = resolve_groups(routing, op);
    let refused = if let Some(why) = hub.unreadable.as_ref().or(hub.newer.as_ref()) {
        Some(format!("{why}; nothing was changed"))
    } else if let TodoOp::SetMirror { path: Some(path) } = &op
        && !path.trim().is_empty()
        && !["off", "none"].contains(&path.trim().to_ascii_lowercase().as_str())
        && !Path::new(path.trim()).is_absolute()
    {
        Some(format!("the mirror needs a full path on the service's machine, not {:?}", path.trim()))
    } else {
        None
    };
    if let Some(why) = refused {
        let result = Err(why);
        append_log(home, &routing.canvas, actor, &op, &result, now);
        return Outcome { result, events: Vec::new() };
    }
    // A canvas-wide agent's group is its frame: it has none for boards (it names them).
    let group = actor.session.as_deref().filter(|s| !routing.passive_by_session(s).is_some_and(|p| p.canvas_wide)).and_then(|s| routing.group_of(s));
    let ctx = Ctx { now, group, groups: group_names(routing), live, sessions: session_names(routing) };
    let logged = op.clone();
    // The book's text, checked against the size budget before the change is kept (a change that shrinks it always is).
    let (old, budget, mut text) = (hub.saved.len(), hub.budget.unwrap_or(MAX_BOOK_BYTES), None);
    let mut result = hub.book.apply_checked(op, actor, &ctx, |next| {
        let new = serde_json::to_string_pretty(next).map_err(|e| e.to_string())?;
        if new.len() > budget && new.len() > old {
            return Err(format!("the canvas's todo book would grow beyond {} MB (every window gets all of it): delete old boards or todos first", budget >> 20));
        }
        text = Some(new);
        Ok(())
    });
    let mut events = Vec::new();
    if let Ok(applied) = &mut result {
        let stored = match text {
            Some(text) => hub.store(home, text),
            None => hub.save(home),
        };
        if let Err(error) = stored {
            applied.warnings.push(format!("the change is not saved yet: {error}"));
        }
        if let Err(error) = write_mirror(&hub.book, now) {
            applied.warnings.push(error);
        }
        events = applied.events.iter().map(|e| bus_event(&hub.book, routing, actor, e, hop)).collect();
    }
    append_log(home, &routing.canvas, actor, &logged, &result, now);
    Outcome { result, events }
}

/// The answer a waiting command gets (`data`: its `--format json` output).
fn answer_of(book: &TodoBook, result: &Result<Applied, String>) -> Answer {
    match result {
        Ok(applied) => {
            let id = applied.created.clone().or_else(|| applied.changed.first().cloned());
            let mut data = json!({
                "schemaVersion": SCHEMA_VERSION, "kind": "todo-change", "ok": true, "message": applied.message,
                "warnings": applied.warnings, "id": id, "created": applied.created, "changed": applied.changed,
            });
            if let Some(id) = id.as_deref() {
                if let Some(todo) = book.todo(id) {
                    data["todo"] = serde_json::to_value(todo).unwrap_or_default();
                } else if let Some(board) = book.board(id) {
                    data["board"] = serde_json::to_value(board).unwrap_or_default();
                }
            }
            let message = std::iter::once(applied.message.clone()).chain(applied.warnings.iter().map(|w| format!("Warning: {w}."))).collect::<Vec<_>>().join("\n");
            Answer { ok: true, message, state: "applied".into(), data: Some(data) }
        }
        Err(why) => Answer { ok: false, message: why.clone(), state: "refused".into(), data: Some(json!({ "schemaVersion": SCHEMA_VERSION, "kind": "todo-change", "ok": false, "message": why })) },
    }
}

/// Serves one `canvas.todo` request: applies it as its sender and writes
/// the answer the command waits for. Returns the answer and the events to
/// route. A request served before (its inbox file read again because it
/// could not be removed) is never applied twice.
pub(super) fn serve(hub: &mut Hub, home: &Path, routing: &Routing, event: &Event, live: Option<BTreeSet<String>>, now: u64) -> (Answer, Vec<Event>) {
    let source = event.source.as_str();
    let request = event.data["request"].as_str().unwrap_or_default();
    if hub.seen(request) {
        return (Answer { ok: true, message: format!("request {request} was served already: not applied again"), state: "duplicate".into(), data: None }, Vec::new());
    }
    let (answer, events) = if !source.is_empty() && !routing.names.contains_key(source) && routing.passive_by_session(source).is_none() {
        let why = "this session is not on the canvas Atelier has open (open its canvas first)".to_owned();
        (Answer { ok: false, message: why.clone(), state: "refused".into(), data: None }, Vec::new())
    } else {
        match serde_json::from_value::<TodoOp>(event.data.clone()) {
            Ok(op) => {
                let actor = actor_of(routing, source, event.data["bot"].as_str());
                let outcome = apply(hub, home, routing, op, &actor, Run { now, hop: event.hop, live });
                (answer_of(&hub.book, &outcome.result), outcome.events)
            }
            Err(error) => (Answer { ok: false, message: format!("not a todo operation this service knows ({error}): is atelier newer than the service?"), state: "refused".into(), data: None }, Vec::new()),
        }
    };
    write_answer(home, request, &answer);
    (answer, events)
}

/// Sessions of the canvas that run (here, or elsewhere): their claims are live.
fn live_sessions(shared: &Shared, routing: &Routing) -> BTreeSet<String> {
    routing.names.keys().chain(routing.passive.iter().map(|p| &p.session_id)).filter(|id| super::prs::running(shared, id) || super::prs::elsewhere(shared, id)).cloned().collect()
}

pub(super) fn is_request(event: &Event) -> bool {
    event.kind == REQUEST
}

/// A `atelier todo …` request from the bus inbox. Returns events to route.
pub(super) fn request(shared: &Shared, routing: &Routing, event: &Event, now: u64) -> Vec<Event> {
    let live = live_sessions(shared, routing);
    let (answer, events) = serve(&mut lock(&shared.todos), &shared.instance.home, routing, event, Some(live), now);
    let op = event.data["op"].as_str().unwrap_or_default();
    let who = super::prs::who(routing, &event.source);
    log(&shared.instance, &format!("todo {op} from {who}: {}{}", if answer.ok { "" } else { "refused: " }, answer.message.lines().next().unwrap_or_default()));
    events
}
/// A window's operation (`{"action": "todos", "op": …}`), applied as you:
/// the notice for that window when it is refused, and the events to route.
pub(super) fn window_op(hub: &mut Hub, home: &Path, routing: &Routing, value: &serde_json::Value, live: Option<BTreeSet<String>>, now: u64) -> (Option<String>, Vec<Event>) {
    let op = match serde_json::from_value::<TodoOp>(value["op"].clone()) {
        Ok(op) => op,
        Err(error) => return (Some(format!("Todo change not understood by the service ({error}): restart the service after updating Atelier")), Vec::new()),
    };
    let outcome = apply(hub, home, routing, op, &Actor::human(), Run { now, hop: 0, live });
    (outcome.result.err().map(|why| format!("Todo change refused: {why}")), outcome.events)
}

/// A window's todo action (false: `value` is none). Applied now, not on
/// the bus loop's next pass; every window gets the new book at once, and a
/// refusal goes back to the window that asked (with the book as it is, so
/// its optimistic change is undone).
pub(super) fn action(shared: &Shared, client: &Arc<Client>, value: &serde_json::Value) -> bool {
    if value["action"].as_str() != Some("todos") {
        return false;
    }
    let routing = lock(&shared.routing).clone();
    let live = live_sessions(shared, &routing);
    let (refused, events) = window_op(&mut lock(&shared.todos), &shared.instance.home, &routing, value, Some(live), now_ms());
    {
        // Made and sent in turn with the bus loop's: a snapshot made before a newer one is never sent after it.
        let _turn = lock(&shared.snapshots);
        let snapshot = ServiceMsg::Bus { json: super::passive::snapshot(shared) };
        match refused {
            Some(notice) => {
                log(&shared.instance, &format!("todo {} from a window: {notice}", value["op"]["op"].as_str().unwrap_or("?")));
                client.send(ServiceMsg::Error { id: None, message: notice });
                client.send(snapshot);
            }
            None => shared.broadcast(snapshot),
        }
    }
    for event in events {
        let _ = shared.bus_tx.send(event);
    }
    true
}

/// A merged pull request finishes the todos linked to it that are in
/// progress or in review (as the bot "pull request merged"). Returns their events.
pub(super) fn finish_merged(hub: &mut Hub, home: &Path, routing: &Routing, event: &Event, now: u64) -> Vec<Event> {
    if event.kind != "pr.merged" || !crate::doc::valid_canvas_id(&routing.canvas) {
        return Vec::new();
    }
    hub.open(home, &routing.canvas);
    let links: Vec<String> = ["url", "pr"].iter().filter_map(|k| event.data[*k].as_str()).map(str::to_owned).filter(|l| !l.trim().is_empty()).collect();
    let mut ids: Vec<String> = Vec::new();
    for link in &links {
        for id in hub.book.todos_for_pr(link) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let shown = links.first().cloned().unwrap_or_default();
    let actor = Actor::bot("pull request merged");
    let mut events = Vec::new();
    for id in ids {
        let op = TodoOp::Done { id, note: Some(format!("Its pull request was merged: {shown}")) };
        events.extend(apply(hub, home, routing, op, &actor, Run { now, hop: event.hop, live: None }).events);
    }
    events
}

/// `pr.merged` from the PR monitor or a session's history (the bus loop
/// never passes events from inbox files here). Returns events to route.
pub(super) fn merged(shared: &Shared, routing: &Routing, event: &Event, now: u64) -> Vec<Event> {
    if event.kind != "pr.merged" {
        return Vec::new();
    }
    let events = finish_merged(&mut lock(&shared.todos), &shared.instance.home, routing, event, now);
    for done in &events {
        log(&shared.instance, &format!("todo {} is done: its pull request was merged", done.data["id"].as_str().unwrap_or_default()));
    }
    events
}

/// The routed canvas's book for clients (`None`: the canvas has no id, or
/// its stored book cannot be read).
pub(super) fn view(shared: &Shared, routing: &Routing) -> Option<TodoBook> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return None;
    }
    let mut hub = lock(&shared.todos);
    hub.open(&shared.instance.home, &routing.canvas);
    hub.unreadable.is_none().then(|| hub.book.clone())
}

/// The `# Todos` section of a group's `group-context.md` (empty: no board is linked to it).
pub(super) fn group_section(book: &TodoBook, group: &str) -> String {
    match todos::group_summary(book, group) {
        Some(summary) => format!(
            "\n# Todos\n\nThe todo boards linked to this group. `atelier todo prime` says how to use them; change todos only with `atelier todo …` (never edit a TODOs.md mirror).\n\n{summary}"
        ),
        None => String::new(),
    }
}

/// The todo part of `canvas-context.md` (empty while the canvas has no boards).
pub(super) fn canvas_section(book: &TodoBook, routing: &Routing) -> String {
    if book.boards.is_empty() {
        return String::new();
    }
    format!("\n# Todos\n\nThe canvas's todo boards (`atelier todo boards`; `atelier todo list --board NAME` lists one).\n\n{}", todos::canvas_summary(book, &group_names(routing)))
}

pub(super) fn group_digest(shared: &Shared, routing: &Routing, group: &str) -> String {
    view(shared, routing).map(|book| group_section(&book, group)).unwrap_or_default()
}

pub(super) fn canvas_digest(shared: &Shared, routing: &Routing) -> String {
    view(shared, routing).map(|book| canvas_section(&book, routing)).unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bus::{PassiveSpec, RouteGroup};

    const NOW: u64 = 1_793_000_000_000;

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("cc-todos-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// Canvas c1: Pay (g1) with the session s1 and the passive session p1
    /// (on `todo.*`), Docs (g2), and a canvas-wide agent w (on `todo.*`).
    fn routing() -> Routing {
        Routing {
            canvas: "c1".into(),
            groups: vec![RouteGroup { id: "g1".into(), name: "Pay".into(), folder: None }, RouteGroup { id: "g2".into(), name: "Docs".into(), folder: None }],
            members: [("s1".to_owned(), "g1".to_owned())].into(),
            names: [("s1".to_owned(), "Fix flaky test".to_owned()), ("p1".to_owned(), "Todo watcher".to_owned()), ("w".to_owned(), "Coordinator".to_owned())].into(),
            passive: vec![
                PassiveSpec { key: "kp".into(), session_id: "p1".into(), name: "Todo watcher".into(), group: "g1".into(), subscribe: vec!["todo.*".into()], ..Default::default() },
                PassiveSpec { key: "kw".into(), session_id: "w".into(), name: "Coordinator".into(), subscribe: vec!["todo.*".into()], canvas_wide: true, ..Default::default() },
            ],
            ..Default::default()
        }
    }

    fn request(source: &str, mut op: serde_json::Value, id: &str) -> Event {
        op["request"] = id.into();
        Event { kind: REQUEST.into(), source: source.into(), data: op, hop: 1, ..Default::default() }
    }

    fn log_lines(home: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(crate::todos_cli::todos_dir(home, "c1").join("log.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn requests_are_applied_as_their_sender_answered_and_stored() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        let (answer, events) = serve(&mut hub, &home, &r, &request("", json!({ "op": "add_board", "name": "Decomps", "groups": ["pay"] }), "r-1"), None, NOW);
        assert!(answer.ok && events.is_empty(), "{answer:?}");
        assert_eq!(answer.message, "Board \"Decomps\" (DEC) created, linked to Pay.");
        let stored: Answer = serde_json::from_slice(&std::fs::read(crate::prs_cli::answers_dir(&home).join("r-1.json")).unwrap()).unwrap();
        assert_eq!(stored, answer, "the waiting command reads this answer");
        assert_eq!(answer.data.as_ref().unwrap()["board"]["canvas_groups"][0], "g1", "group names become ids");
        // A session adds to its group's board; the event goes to that group, from the session.
        let (answer, events) = serve(&mut hub, &home, &r, &request("s1", json!({ "op": "add", "title": "Fix the loader", "type": "bug" }), "r-2"), None, NOW);
        let data = answer.data.unwrap();
        assert_eq!((data["schemaVersion"].as_u64(), data["kind"].as_str(), data["todo"]["id"].as_str(), data["todo"]["created_by"]["session"].as_str()), (Some(1), Some("todo-change"), Some("DEC-001"), Some("s1")));
        assert_eq!(events.len(), 1);
        let created = &events[0];
        assert_eq!((created.kind.as_str(), created.group.as_str(), created.source.as_str(), created.canvas.as_str(), created.hop), ("todo.created", "g1", "s1", "c1", 1));
        assert_eq!(r.subscribers(created), ["kp", "kw"], "the group's passive sessions and the canvas-wide agents");
        // A passive session claims it: its own event never comes back to it.
        let (answer, events) = serve(&mut hub, &home, &r, &request("p1", json!({ "op": "claim", "id": "dec-1" }), "r-3"), None, NOW);
        assert!(answer.ok && answer.message.starts_with("Claimed DEC-001"), "{answer:?}");
        assert_eq!((events[0].kind.as_str(), events[0].data["by"].as_str()), ("todo.claimed", Some("Todo watcher")));
        assert_eq!(r.subscribers(&events[0]), ["kw"]);
        // Refusals: the rules, strangers, garbage, no canvas.
        let (answer, _) = serve(&mut hub, &home, &r, &request("s1", json!({ "op": "delete", "id": "DEC-001" }), "r-4"), None, NOW);
        assert!(!answer.ok && answer.message.contains("agents may not delete todos"), "{answer:?}");
        assert_eq!(answer.data.unwrap()["ok"], false);
        let (answer, _) = serve(&mut hub, &home, &r, &request("stranger", json!({ "op": "note", "id": "DEC-1", "text": "hi" }), "r-5"), None, NOW);
        assert!(answer.message.contains("not on the canvas"));
        let (answer, _) = serve(&mut hub, &home, &r, &request("", json!({ "op": "explode" }), "r-6"), None, NOW);
        assert!(answer.message.contains("not a todo operation"));
        let (answer, _) = serve(&mut hub, &home, &Routing { canvas: String::new(), ..r.clone() }, &request("", json!({ "op": "note", "id": "DEC-1", "text": "x" }), "r-7"), None, NOW);
        assert!(answer.message.contains("no canvas with an id"));
        // The automation outside a session is the bot.
        let (answer, _) = serve(&mut hub, &home, &r, &request("", json!({ "op": "note", "id": "DEC-1", "text": "checked", "bot": "TODO bot" }), "r-8"), None, NOW);
        assert!(answer.ok);
        assert_eq!(hub.book.todo("DEC-001").unwrap().log.last().unwrap().by, Actor::bot("TODO bot"));
        // A canvas-wide agent's board belongs to no group (its group is its frame).
        let (answer, _) = serve(&mut hub, &home, &r, &request("w", json!({ "op": "add_board", "name": "Agents board" }), "r-9"), None, NOW);
        assert!(answer.ok && hub.book.find_board("Agents board").unwrap().canvas_groups.is_empty(), "{answer:?}");
        // Stored and logged (refusals too); the book survives reopening.
        let stored = crate::todos_cli::load(&home, "c1");
        assert_eq!(stored, hub.book);
        let lines = log_lines(&home);
        assert_eq!(lines.iter().map(|l| (l["op"]["op"].as_str().unwrap_or_default().to_owned(), l["ok"].as_bool().unwrap())).collect::<Vec<_>>(), [("add_board".to_owned(), true), ("add".to_owned(), true), ("claim".to_owned(), true), ("delete".to_owned(), false), ("note".to_owned(), true), ("add_board".to_owned(), true)]);
        assert_eq!(lines[3]["actor"]["session"], "s1");
        let mut again = Hub::default();
        again.open(&home, "c1");
        assert_eq!(again.book, hub.book);
        again.open(&home, "c2");
        assert!(again.book.boards.is_empty(), "another canvas, another book");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn window_actions_apply_as_you_and_refusals_become_notices() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        let window = |hub: &mut Hub, op: TodoOp, live: Option<BTreeSet<String>>| window_op(hub, &home, &r, &todos::client_action(&op), live, NOW);
        let (refused, _) = window(&mut hub, TodoOp::AddBoard { name: "Decomps".into(), prefix: None, parent: None, groups: vec!["g1".into()], x: Some(3), y: Some(4) }, None);
        assert_eq!(refused, None);
        let (refused, events) = window(&mut hub, TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: "A".into(), status: Some(todos::Status::Todo), ..Default::default() })), None);
        assert_eq!((refused, events[0].source.as_str(), events[0].source_name.as_str()), (None, "", "you"));
        serve(&mut hub, &home, &r, &request("s1", json!({ "op": "claim", "id": "DEC-001" }), "c-1"), None, NOW);
        let live: BTreeSet<String> = ["s1".to_owned()].into();
        let (refused, _) = window(&mut hub, TodoOp::Claim { id: "DEC-001".into(), force: false, session: None }, Some(live.clone()));
        assert!(refused.as_deref().is_some_and(|n| n.starts_with("Todo change refused: DEC-001 is claimed by Fix flaky test")), "{refused:?}");
        let (refused, _) = window(&mut hub, TodoOp::Claim { id: "DEC-001".into(), force: false, session: None }, Some(BTreeSet::new()));
        assert_eq!(refused, None, "s1 stopped: its claim is stale");
        assert_eq!(hub.book.todo("DEC-001").unwrap().claimed_by.as_ref().unwrap().by, Actor::human());
        let (refused, _) = window_op(&mut hub, &home, &r, &json!({ "action": "todos", "op": { "op": "explode" } }), None, NOW);
        assert!(refused.unwrap().contains("not understood"));
        let (refused, _) = window(&mut hub, TodoOp::SetMirror { path: Some("TODOs.md".into()) }, None);
        assert!(refused.unwrap().contains("needs a full path"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn you_claim_todos_for_a_session_by_id_or_name() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        let window = |hub: &mut Hub, op: TodoOp| window_op(hub, &home, &r, &todos::client_action(&op), None, NOW);
        window(&mut hub, TodoOp::AddBoard { name: "Decomps".into(), prefix: None, parent: None, groups: vec!["g1".into()], x: None, y: None });
        for title in ["A", "B", "C"] {
            window(&mut hub, TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: title.into(), status: Some(todos::Status::Todo), ..Default::default() })));
        }
        // A window names the session by id: its claim, with the canvas's name for it.
        let (refused, events) = window(&mut hub, TodoOp::Claim { id: "DEC-001".into(), force: false, session: Some("s1".into()) });
        assert_eq!(refused, None);
        assert_eq!(hub.book.todo("DEC-001").unwrap().claimed_by.as_ref().map(|c| c.by.clone()), Some(Actor::agent("s1", "Fix flaky test")));
        assert_eq!((events[0].kind.as_str(), events[0].data["for_session"].as_str(), events[0].source.as_str()), ("todo.claimed", Some("s1"), ""));
        // The command (you) may name it: sessions and passive sessions resolve to their ids.
        let (answer, _) = serve(&mut hub, &home, &r, &request("", json!({ "op": "claim", "id": "DEC-2", "session": "fix FLAKY test" }), "r-1"), None, NOW);
        assert!(answer.ok, "{answer:?}");
        assert_eq!(hub.book.todo("DEC-002").unwrap().claim_session(), Some("s1"));
        let (answer, _) = serve(&mut hub, &home, &r, &request("", json!({ "op": "claim", "id": "DEC-3", "session": "Todo watcher" }), "r-2"), None, NOW);
        assert!(answer.ok, "{answer:?}");
        assert_eq!(hub.book.todo("DEC-003").unwrap().claimed_by.as_ref().map(|c| c.by.clone()), Some(Actor::agent("p1", "Todo watcher")));
        // Strangers and agents assigning others are refused.
        let (refused, _) = window(&mut hub, TodoOp::Claim { id: "DEC-001".into(), force: true, session: Some("nobody".into()) });
        assert!(refused.is_some_and(|r| r.contains("no session \"nobody\"")));
        hub.book.apply(TodoOp::Release { id: "DEC-003".into() }, &Actor::human(), &Ctx::at(NOW)).unwrap();
        let (answer, _) = serve(&mut hub, &home, &r, &request("s1", json!({ "op": "claim", "id": "DEC-3", "session": "p1" }), "r-3"), None, NOW);
        assert!(!answer.ok && answer.message.contains("only for themselves"), "{answer:?}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn merged_pull_requests_finish_their_todos_and_the_mirror_follows() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        let mirror = home.join("repo").join("TODOs.md");
        let you = Actor::human();
        let run = || Run { now: NOW, hop: 0, live: None };
        for op in [
            TodoOp::AddBoard { name: "Decomps".into(), prefix: None, parent: None, groups: vec!["g1".into()], x: None, y: None },
            TodoOp::SetMirror { path: Some(mirror.display().to_string()) },
            TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: "Load the file".into(), status: Some(todos::Status::Todo), ..Default::default() })),
            TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: "Not started".into(), status: Some(todos::Status::Todo), ..Default::default() })),
            TodoOp::Link { id: "DEC-002".into(), kind: todos::LinkKind::Pr, target: "https://github.com/o/r/pull/4".into(), title: None },
        ] {
            assert!(apply(&mut hub, &home, &r, op, &you, run()).result.is_ok());
        }
        let s1 = Actor::agent("s1", "Fix flaky test");
        apply(&mut hub, &home, &r, TodoOp::Claim { id: "DEC-001".into(), force: false, session: None }, &s1, run());
        apply(&mut hub, &home, &r, TodoOp::Submit { id: "DEC-001".into(), pr: Some("https://github.com/O/R/pull/4".into()), summary: None }, &s1, run());
        assert!(std::fs::read_to_string(&mirror).unwrap().contains("- [ ] **DEC-001: [[DEC-001 - Load the file]]** - (canvas: in review; claimed by Fix flaky test)"));
        let closed = Event { kind: "pr.closed".into(), data: json!({ "url": "https://github.com/o/r/pull/4" }), ..Default::default() };
        assert!(finish_merged(&mut hub, &home, &r, &closed, NOW).is_empty());
        let merged = Event { kind: "pr.merged".into(), source: "s1".into(), data: json!({ "pr": "github.com/o/r#4", "url": "https://github.com/o/r/pull/4", "monitor": "atelier" }), hop: 1, ..Default::default() };
        let events = finish_merged(&mut hub, &home, &r, &merged, NOW);
        assert_eq!(events.len(), 1, "only the todo in review; one in todo stays");
        assert_eq!((events[0].kind.as_str(), events[0].data["id"].as_str(), events[0].data["by"].as_str(), events[0].group.as_str()), ("todo.done", Some("DEC-001"), Some("pull request merged"), "g1"));
        let done = hub.book.todo("DEC-001").unwrap();
        assert_eq!((done.status, done.claimed_by.is_none()), (todos::Status::Done, true));
        assert!(done.log.iter().any(|l| l.text == "Its pull request was merged: https://github.com/o/r/pull/4"));
        assert_eq!(hub.book.todo("DEC-002").unwrap().status, todos::Status::Todo);
        assert!(std::fs::read_to_string(&mirror).unwrap().contains("- [x] **DEC-001: [[DEC-001 - Load the file]]**"), "the mirror is rewritten");
        assert!(finish_merged(&mut hub, &home, &r, &merged, NOW).is_empty(), "once");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn digests_get_a_todos_section() {
        let r = routing();
        let mut book = todos::TodoBook::default();
        let ctx = Ctx { now: NOW, group: None, groups: group_names(&r), ..Default::default() };
        book.apply(TodoOp::AddBoard { name: "Decomps".into(), prefix: None, parent: None, groups: vec!["g1".into()], x: None, y: None }, &Actor::human(), &ctx).unwrap();
        book.apply(TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: "Crash".into(), kind: todos::TodoType::Bug, status: Some(todos::Status::Todo), ..Default::default() })), &Actor::human(), &ctx).unwrap();
        let group = group_section(&book, "g1");
        assert!(group.starts_with("\n# Todos\n\nThe todo boards linked to this group. `atelier todo prime`"), "{group}");
        assert!(group.contains("## Decomps (DEC)") && group.contains("- DEC-001 [todo, ready] (bug) Crash"), "{group}");
        assert_eq!(group_section(&book, "g2"), "");
        let canvas = canvas_section(&book, &r);
        assert!(canvas.contains("# Todos") && canvas.contains("- Decomps (DEC) · groups: Pay · 1 open"), "{canvas}");
        assert_eq!(canvas_section(&todos::TodoBook::default(), &r), "");
    }

    #[test]
    fn the_command_waits_for_the_services_answer() {
        let home = temp_home();
        let r = routing();
        std::fs::create_dir_all(home.join("bus")).unwrap();
        std::fs::write(home.join("bus").join("routing.json"), serde_json::to_string(&r).unwrap()).unwrap();
        // This test plays the service: it announces itself and serves the inbox.
        crate::reports::announce_service(&home).unwrap();
        let server = std::thread::spawn({
            let home = home.clone();
            move || {
                let (mut hub, mut served) = (Hub::default(), 0);
                let deadline = Instant::now() + Duration::from_secs(30);
                while served < 3 && Instant::now() < deadline {
                    let mut files: Vec<PathBuf> = std::fs::read_dir(crate::bus::inbox_dir(&home)).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect()).unwrap_or_default();
                    files.sort();
                    for file in files {
                        let event: Event = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                        std::fs::remove_file(&file).unwrap();
                        serve(&mut hub, &home, &r, &event, None, now_ms());
                        served += 1;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        });
        let you = crate::todos_cli::Caller::default();
        let opts = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let out = crate::todos_cli::execute(&home, &["board", "add", "Decomps"], &[], &[], &you, &mut std::io::empty()).unwrap();
        assert_eq!(out, "Board \"Decomps\" (DEC) created.");
        let out = crate::todos_cli::execute(&home, &["add", "Crash", "on", "load"], &opts(&[("type", "bug"), ("board", "dec"), ("format", "json")]), &[], &you, &mut std::io::empty()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!((value["ok"].as_bool(), value["todo"]["id"].as_str(), value["todo"]["title"].as_str()), (Some(true), Some("DEC-001"), Some("Crash on load")));
        let agent = crate::todos_cli::Caller { session: "s1".into(), bot: String::new() };
        let error = crate::todos_cli::execute(&home, &["done", "DEC-1"], &[], &[], &agent, &mut std::io::empty()).unwrap_err().to_string();
        assert!(error.starts_with("Atelier refused: marking todos done is the user's"), "{error}");
        server.join().unwrap();
        assert!(crate::todos_cli::execute(&home, &["list"], &[], &[], &you, &mut std::io::empty()).unwrap().contains("DEC-001  backlog · bug  Crash on load"));
        let _ = std::fs::remove_dir_all(home);
    }

    fn add_board(hub: &mut Hub, home: &Path, name: &str) -> Outcome {
        apply(hub, home, &routing(), TodoOp::AddBoard { name: name.into(), prefix: None, parent: None, groups: vec![], x: None, y: None }, &Actor::human(), Run { now: NOW, hop: 0, live: None })
    }

    #[test]
    fn a_book_that_cannot_be_read_is_never_overwritten() {
        let (home, mut hub) = (temp_home(), Hub::default());
        let path = crate::todos_cli::book_path(&home, "c1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A book from a newer version (a status this one does not know), or a damaged one.
        for stored in [r#"{"boards":[],"todos":[{"id":"A-1","status":"someday"}]}"#, "{\"boards\": [", ""] {
            std::fs::write(&path, stored).unwrap();
            let mut hub = Hub::default();
            let refused = add_board(&mut hub, &home, "Decomps").result.unwrap_err();
            assert!(refused.contains("cannot be read") && refused.ends_with("nothing was changed"), "{refused}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), stored, "left as it was");
            assert!(hub.unreadable.is_some() && crate::todos_cli::try_load(&home, "c1").is_err());
        }
        // Read again once it is fixed.
        std::fs::write(&path, "{\"boards\": [").unwrap();
        assert!(add_board(&mut hub, &home, "Decomps").result.is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(add_board(&mut hub, &home, "Decomps").result.is_ok());
        assert_eq!(crate::todos_cli::load(&home, "c1").boards.len(), 1);
        let dir = path.parent().unwrap();
        assert_eq!(std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().to_string_lossy().ends_with(".tmp")).count(), 0, "no temporary file is left");
        // A newer version's book (it may hold what this one would drop): shown, never written.
        let newer = format!(r#"{{"version":{},"boards":[{{"id":"b","name":"B","prefix":"BB","later":true}}]}}"#, todos::VERSION + 1);
        std::fs::write(&path, &newer).unwrap();
        let mut hub = Hub::default();
        let refused = add_board(&mut hub, &home, "Decomps").result.unwrap_err();
        assert!(refused.contains("newer atelier") && hub.book.boards.len() == 1 && hub.unreadable.is_none(), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_request_read_twice_is_applied_once() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        add_board(&mut hub, &home, "Decomps");
        let add = request("", json!({ "op": "add", "title": "Once", "type": "bug", "board": "Decomps" }), "r-1");
        let (first, events) = serve(&mut hub, &home, &r, &add, None, NOW);
        assert!(first.ok && events.len() == 1, "{first:?}");
        let (again, events) = serve(&mut hub, &home, &r, &add, None, NOW);
        assert!(again.state == "duplicate" && events.is_empty(), "{again:?}");
        assert_eq!(hub.book.todos.len(), 1);
        // Requests without an id (none is ever written so) are not remembered.
        let note = request("", json!({ "op": "note", "id": "DEC-1", "text": "x" }), "");
        serve(&mut hub, &home, &r, &note, None, NOW);
        serve(&mut hub, &home, &r, &note, None, NOW);
        assert_eq!(hub.book.todos[0].log.iter().filter(|l| l.text == "x").count(), 2);
        // The memory is bounded.
        for n in 0..SERVED + 1 {
            assert!(!hub.seen(&format!("q-{n}")));
        }
        assert_eq!(hub.served.len(), SERVED);
        assert!(!hub.seen("r-1"), "the oldest are forgotten");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_book_stays_within_its_size_budget() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        add_board(&mut hub, &home, "Decomps");
        let saved = std::fs::read_to_string(crate::todos_cli::book_path(&home, "c1")).unwrap();
        hub.budget = Some(saved.len() + 1_500);
        let you = Actor::human();
        let run = || Run { now: NOW, hop: 0, live: None };
        let add = |title: &str| TodoOp::Add(Box::new(todos::NewTodo { board: "Decomps".into(), title: title.into(), ..Default::default() }));
        assert!(apply(&mut hub, &home, &r, add("Fits"), &you, run()).result.is_ok());
        let book = hub.book.clone();
        let refused = apply(&mut hub, &home, &r, TodoOp::Update(Box::new(todos::TodoUpdate { id: "DEC-1".into(), body: Some("x".repeat(2_000)), ..Default::default() })), &you, run()).result.unwrap_err();
        assert!(refused.contains("would grow beyond"), "{refused}");
        assert_eq!(hub.book, book, "refused: nothing changed");
        assert_eq!(crate::todos_cli::load(&home, "c1"), book, "nor stored");
        // Over the budget (a smaller one now), changes that do not grow it still go through.
        hub.budget = Some(10);
        assert!(apply(&mut hub, &home, &r, add("More"), &you, run()).result.is_err());
        assert!(apply(&mut hub, &home, &r, TodoOp::Delete { id: "DEC-1".into() }, &you, run()).result.is_ok());
        assert!(hub.book.todos.is_empty() && crate::todos_cli::load(&home, "c1").todos.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_mirror_keeps_a_copy_of_a_file_it_did_not_write() {
        let (home, r, mut hub) = (temp_home(), routing(), Hub::default());
        let mirror = home.join("repo").join("TODOs.md");
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        std::fs::write(&mirror, "# My own list\n- [ ] keep me\n").unwrap();
        let run = || Run { now: NOW, hop: 0, live: None };
        add_board(&mut hub, &home, "Decomps");
        assert!(apply(&mut hub, &home, &r, TodoOp::SetMirror { path: Some(mirror.display().to_string()) }, &Actor::human(), run()).result.is_ok());
        let backup = home.join("repo").join("TODOs.md.bak");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "# My own list\n- [ ] keep me\n", "the owner's file is kept");
        assert!(std::fs::read_to_string(&mirror).unwrap().contains(MIRROR_MARK));
        // Its own mirror is simply rewritten; another foreign file gets the next free name.
        add_board(&mut hub, &home, "Second");
        assert!(std::fs::read_to_string(&mirror).unwrap().contains("## Second"));
        std::fs::write(&mirror, "hand edited\n").unwrap();
        add_board(&mut hub, &home, "Third");
        assert_eq!(std::fs::read_to_string(home.join("repo").join(format!("TODOs.md.{NOW}.bak"))).unwrap(), "hand edited\n");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "# My own list\n- [ ] keep me\n", "never over a backup");
        let names: Vec<String> = std::fs::read_dir(home.join("repo")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 3, "the mirror and two copies, nothing else: {names:?}");
        let _ = std::fs::remove_dir_all(home);
    }
}
