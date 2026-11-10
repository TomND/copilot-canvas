//! Changes to a todo book ([`TodoOp`]) and the rules for who may make them
//! ([`TodoBook::apply`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::*;

/// Where and for whom an operation runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ctx {
    /// Now (ms since the epoch).
    pub now: u64,
    /// The actor's canvas group: an agent's new board is linked to it, and
    /// `add` without a board uses the first board linked to it.
    pub group: Option<String>,
    /// The canvas's groups (id → name); group ids are checked against them
    /// when there are any.
    pub groups: BTreeMap<String, String>,
    /// Sessions that are live (running here or elsewhere). A claim by a
    /// session outside this set is stale: anyone may take it over. `None`:
    /// every claim counts as live.
    pub live: Option<BTreeSet<String>>,
    /// The canvas's sessions (id → name): a claim for a session (people and
    /// bots assign todos) must name one of them when there are any (else one
    /// of `live` when that is known).
    pub sessions: BTreeMap<String, String>,
}

impl Ctx {
    pub fn at(now: u64) -> Ctx {
        Ctx { now, ..Default::default() }
    }
}

/// A bus event an applied operation emits (the service sends it to the
/// first canvas group linked to the board, never back to the actor).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TodoEvent {
    /// `todo.created`, `todo.proposed`, `todo.accepted`, `todo.status`
    /// (`from`, `to`), `todo.claimed`, `todo.submitted`, `todo.done`.
    pub kind: String,
    pub board: String,
    pub data: serde_json::Value,
}

/// What an applied operation did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    /// Todos and boards it changed (ids).
    pub changed: Vec<String>,
    /// The todo or board it created.
    pub created: Option<String>,
    /// What happened, for people.
    pub message: String,
    /// Soft warnings: over a WIP limit, a blocked todo claimed, a claim taken over.
    pub warnings: Vec<String>,
    pub events: Vec<TodoEvent>,
}

/// A new todo (`TodoOp::Add`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct NewTodo {
    /// A board id, name or prefix; empty: the first board linked to the actor's group.
    pub board: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: TodoType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Backlog by default; agents may ask for backlog or todo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Checklist items.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checklist: Vec<String>,
    /// A proposal (an agent's own idea): it waits for the user.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub propose: bool,
    /// Why it is worth doing (proposals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Found while working on this todo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blocker: bool,
}

/// Field changes (`TodoOp::Update`); `None` keeps a field.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TodoUpdate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<TodoType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Why it is blocked; empty or `none`: not blocked any more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Replaces the labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove_labels: Vec<String>,
    /// A parent todo; empty: none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The spec note's path; empty: none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Every change to a todo book (JSON: `{"op": "claim", "id": "DEC-031", …}`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoOp {
    /// A board (with `parent`: a sub-board, a swimlane of that board).
    AddBoard {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// Canvas groups (ids) to link it to (agents: always their own group).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        groups: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
    },
    UpdateBoard {
        board: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// A top-level board's prefix (todos keep their ids).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// The WIP limit; 0: none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wip: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<u8>,
        /// The notes folder; empty: none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<AgentPolicy>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        statuses: Option<StatusNames>,
    },
    /// Its place and size on the todo canvas.
    MoveBoard {
        board: String,
        x: i32,
        y: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<i32>,
    },
    /// Links a board to a canvas group (id), or unlinks it.
    LinkBoard {
        board: String,
        group: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unlink: bool,
    },
    /// Deletes a board and its sub-boards; one holding todos only `with_todos`.
    DeleteBoard {
        board: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        with_todos: bool,
    },
    Add(Box<NewTodo>),
    Update(Box<TodoUpdate>),
    /// Another status, board, or place in its column (`after` / `before` a
    /// neighbour, or an explicit `rank`; else the end of the column).
    Move {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        board: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rank: Option<String>,
    },
    /// Accepts a proposal (into backlog, or `status`).
    Accept {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
    },
    Decline {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The actor works on it (backlog and todo move to in progress); people
    /// and bots may claim it for a session of the canvas (`session`: its id),
    /// agents only for themselves.
    Claim {
        id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    Release {
        id: String,
    },
    /// Appends a note to its log.
    Note {
        id: String,
        text: String,
    },
    /// Ticks (or unticks) checklist item `index` (0-based).
    Check {
        id: String,
        index: usize,
        #[serde(default = "yes")]
        done: bool,
    },
    AddCheck {
        id: String,
        text: String,
    },
    Link {
        id: String,
        kind: LinkKind,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Unlink {
        id: String,
        target: String,
    },
    Relate {
        id: String,
        kind: RelKind,
        other: String,
    },
    Unrelate {
        id: String,
        other: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<RelKind>,
    },
    /// Finished work for review (in review), with its pull request and a summary.
    Submit {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    Done {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Cancel {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution: Option<Resolution>,
    },
    /// A done or canceled todo goes back to todo (a declined proposal: proposed).
    Reopen {
        id: String,
    },
    Delete {
        id: String,
    },
    /// The Markdown mirror file (`None`: no mirror).
    SetMirror {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Todos from the owner's TODOs.md format into a board (created when missing).
    Import {
        board: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        text: String,
    },
    /// Rewords checklist item `index` (0-based; its tick stays). An older
    /// service answers "not understood" (the change is undone).
    EditCheck {
        id: String,
        index: usize,
        text: String,
    },
    /// Removes checklist item `index` (0-based; people only).
    RemoveCheck {
        id: String,
        index: usize,
    },
}

fn yes() -> bool {
    true
}

impl TodoOp {
    /// The todo it is about, if any.
    pub fn todo_id(&self) -> Option<&str> {
        match self {
            TodoOp::Update(u) => Some(&u.id),
            TodoOp::Move { id, .. }
            | TodoOp::Accept { id, .. }
            | TodoOp::Decline { id, .. }
            | TodoOp::Claim { id, .. }
            | TodoOp::Release { id }
            | TodoOp::Note { id, .. }
            | TodoOp::Check { id, .. }
            | TodoOp::AddCheck { id, .. }
            | TodoOp::EditCheck { id, .. }
            | TodoOp::RemoveCheck { id, .. }
            | TodoOp::Link { id, .. }
            | TodoOp::Unlink { id, .. }
            | TodoOp::Relate { id, .. }
            | TodoOp::Unrelate { id, .. }
            | TodoOp::Submit { id, .. }
            | TodoOp::Done { id, .. }
            | TodoOp::Cancel { id, .. }
            | TodoOp::Reopen { id }
            | TodoOp::Delete { id } => Some(id),
            _ => None,
        }
    }

    /// Its JSON `op` word.
    pub fn word(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v["op"].as_str().map(str::to_owned)).unwrap_or_default()
    }
}
fn only_people(actor: &Actor, what: &str) -> Result<(), String> {
    if actor.is_agent() { Err(format!("agents may not {what}: that is the user's (ask them)")) } else { Ok(()) }
}

/// Who a claim is for: the actor, or the session people and bots name
/// (one of the canvas's when `ctx` knows them); agents claim only for themselves.
fn claimant(actor: &Actor, session: Option<&str>, ctx: &Ctx) -> Result<Actor, String> {
    let Some(session) = session.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(actor.clone()) };
    if actor.is_agent() {
        if actor.session.as_deref() == Some(session) {
            return Ok(actor.clone());
        }
        return Err("agents claim todos only for themselves (run todo claim ID without --session): ask the user to assign it to another session".into());
    }
    let known = if ctx.sessions.is_empty() { ctx.live.as_ref().map(|live| live.contains(session)) } else { Some(ctx.sessions.contains_key(session)) };
    if known == Some(false) {
        return Err(format!("there is no session {session:?} on this canvas"));
    }
    Ok(Actor::agent(session, ctx.sessions.get(session).map(String::as_str).unwrap_or_default()))
}

/// A title: one line, spaces collapsed, at most [`MAX_TITLE`] characters.
fn clean_title(text: &str) -> Result<String, String> {
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("a todo needs a title".into());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("the title is longer than {MAX_TITLE} characters: put the details in the body"));
    }
    Ok(title)
}

/// One line of text (names, reasons, checklist items, paths).
fn clean_line(text: &str, what: &str, max: usize) -> Result<String, String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return Err(format!("the {what} is empty"));
    }
    if line.chars().count() > max {
        return Err(format!("the {what} is longer than {max} characters"));
    }
    Ok(line)
}

/// Text of several lines (notes, summaries): `\r\n` as `\n`, at most [`MAX_TEXT`] characters.
fn clean_text(text: &str, what: &str) -> Result<String, String> {
    let text = text.replace("\r\n", "\n").trim().to_owned();
    if text.is_empty() {
        return Err(format!("the {what} is empty"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("the {what} is longer than {MAX_TEXT} characters: shorten it (or put it in a note)"));
    }
    Ok(text)
}

fn clean_body(text: &str) -> Result<String, String> {
    let body = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").trim_end().to_owned();
    if body.len() > MAX_BODY {
        return Err(format!("the body is larger than {} KB: put the details in a note and link it", MAX_BODY / 1024));
    }
    Ok(body)
}

/// Labels: lowercase words joined by `-`, at most 40 characters, no duplicates.
fn clean_labels(labels: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        let label = label.split_whitespace().collect::<Vec<_>>().join("-").to_lowercase();
        if label.is_empty() {
            continue;
        }
        if label.chars().count() > 40 || label.contains(',') {
            return Err(format!("the label {label:?} is longer than 40 characters or has a comma"));
        }
        if !out.contains(&label) {
            out.push(label);
        }
    }
    if out.len() > MAX_LABELS {
        return Err(format!("a todo has at most {MAX_LABELS} labels"));
    }
    Ok(out)
}

/// A link's target as stored, and its default title (pull requests: normalized, `owner/repo#12`).
fn clean_link(kind: LinkKind, target: &str) -> Result<(String, String), String> {
    let target = target.trim();
    if target.is_empty() || target.contains(['\n', '\r']) {
        return Err(format!("a {} link needs one target", kind.word()));
    }
    match kind {
        LinkKind::Pr => {
            let pr = crate::prs::PrRef::parse(target)?;
            Ok((pr.url(), pr.short()))
        }
        LinkKind::Url => {
            let lower = target.to_ascii_lowercase();
            if !(lower.starts_with("https://") || lower.starts_with("http://")) || target.contains(char::is_whitespace) || target.len() > 2_000 {
                return Err(format!("{target:?} is not a web link (http:// or https://)"));
            }
            Ok((target.to_owned(), String::new()))
        }
        _ if target.chars().count() > 1_000 => Err(format!("the {} link is longer than 1000 characters", kind.word())),
        _ => Ok((target.to_owned(), String::new())),
    }
}

/// A pull request's key (`github.com/o/r#12`) from its link, for matching.
pub(super) fn pr_key(text: &str) -> Option<String> {
    crate::prs::PrRef::parse(text).ok().map(|p| p.key())
}

fn record(todo: &mut Todo, by: &Actor, now: u64, kind: LogKind, text: String) {
    todo.log.push(LogEntry { ms: now, by: by.clone(), kind, text });
    trim_log(&mut todo.log);
}

/// Keeps the newest [`MAX_LOG`] entries and at most [`MAX_LOG_BYTES`] of
/// their text (always the newest entry): the whole book goes to every client.
pub(super) fn trim_log(log: &mut Vec<LogEntry>) {
    let (mut kept, mut bytes) = (0, 0);
    for entry in log.iter().rev() {
        if kept == MAX_LOG || (kept > 0 && bytes + entry.text.len() > MAX_LOG_BYTES) {
            break;
        }
        kept += 1;
        bytes += entry.text.len();
    }
    let extra = log.len() - kept;
    log.drain(..extra);
}

/// `2026-10-31` of a time.
pub(super) fn date(ms: u64) -> String {
    crate::paths::format_rfc3339_ms(ms).chars().take(10).collect()
}

impl TodoBook {
    /// Applies an operation as `actor` (all or nothing): refusals say why,
    /// in words for the actor. Bumps `rev` (and the changed todos' `rev`
    /// and `updated_ms`).
    pub fn apply(&mut self, op: TodoOp, actor: &Actor, ctx: &Ctx) -> Result<Applied, String> {
        self.apply_checked(op, actor, ctx, |_| Ok(()))
    }

    /// [`TodoBook::apply`], and `check` must accept the changed book before
    /// it is kept (the service's size budget); its refusal changes nothing.
    pub fn apply_checked(&mut self, op: TodoOp, actor: &Actor, ctx: &Ctx, check: impl FnOnce(&TodoBook) -> Result<(), String>) -> Result<Applied, String> {
        let mut next = self.clone();
        let mut applied = next.run(op, actor, ctx)?;
        // Columns whose ranks grew long are spread again.
        let changed: BTreeSet<&str> = applied.changed.iter().map(String::as_str).collect();
        let columns: BTreeSet<(String, Status)> = next.todos.iter().filter(|t| changed.contains(t.id.as_str())).map(|t| (t.board.clone(), t.status)).collect();
        for (board, status) in columns {
            next.tidy_ranks(&board, status, &mut applied);
        }
        next.version = VERSION;
        next.rev += 1;
        let mut seen = BTreeSet::new();
        applied.changed.retain(|id| seen.insert(id.clone()));
        let rev = next.rev;
        for todo in next.todos.iter_mut().filter(|t| seen.contains(&t.id)) {
            todo.rev = rev;
            todo.updated_ms = ctx.now;
        }
        check(&next)?;
        *self = next;
        Ok(applied)
    }

    /// The todos a merged pull request finishes: in progress or in review with a link to it.
    pub fn todos_for_pr(&self, pr: &str) -> Vec<String> {
        let key = pr_key(pr).unwrap_or_else(|| pr.trim().to_lowercase());
        self.todos
            .iter()
            .filter(|t| matches!(t.status, Status::InProgress | Status::InReview))
            .filter(|t| t.prs().any(|l| pr_key(&l.target).unwrap_or_else(|| l.target.to_lowercase()) == key))
            .map(|t| t.id.clone())
            .collect()
    }

    fn index(&self, id: &str) -> Result<usize, String> {
        let found = self.todo(id).map(|t| t.id.clone()).ok_or_else(|| format!("no todo {} on this canvas", id.trim()))?;
        Ok(self.todos.iter().position(|t| t.id == found).unwrap_or_default())
    }

    fn todo_id(&self, id: &str) -> Result<String, String> {
        self.index(id).map(|i| self.todos[i].id.clone())
    }

    fn board_id(&self, text: &str) -> Result<String, String> {
        self.find_board(text).map(|b| b.id.clone()).ok_or_else(|| {
            let names: Vec<&str> = self.boards.iter().map(|b| b.name.as_str()).collect();
            if names.is_empty() {
                format!("no board {:?}: this canvas has no todo boards yet (atelier todo board add NAME)", text.trim())
            } else {
                format!("no board {:?} (boards: {})", text.trim(), names.join(", "))
            }
        })
    }

    /// The rules for agents on a board (its top-level board's).
    fn policy(&self, board: &str) -> AgentPolicy {
        self.root(board).map(|b| b.policy).unwrap_or_default()
    }

    /// The rank after the last todo of a column (`except`: the one moving).
    fn end_rank(&self, board: &str, status: Status, except: &str) -> String {
        let last = self.todos.iter().filter(|t| t.board == board && t.status == status && t.id != except).map(|t| t.rank.as_str()).filter(|r| valid_rank(r)).max();
        rank_between(last, None)
    }

    /// Spreads a column's ranks evenly again once one grew long.
    fn tidy_ranks(&mut self, board: &str, status: Status, out: &mut Applied) {
        let mut column: Vec<&Todo> = self.todos.iter().filter(|t| t.board == board && t.status == status).collect();
        if !column.iter().any(|t| t.rank.len() > 24 || !valid_rank(&t.rank)) {
            return;
        }
        column.sort_by(super::by_rank);
        let ids: Vec<String> = column.iter().map(|t| t.id.clone()).collect();
        let ranks: BTreeMap<&str, String> = ids.iter().map(String::as_str).zip(rank_sequence(ids.len())).collect();
        for todo in &mut self.todos {
            if let Some(rank) = ranks.get(todo.id.as_str()) {
                todo.rank = rank.clone();
            }
        }
        out.changed.extend(ids.iter().cloned());
    }

    /// Moves todo `i` to `to` at the end of its column; the done time,
    /// resolution and claim follow. Answers the old status.
    fn set_status(&mut self, i: usize, to: Status, resolution: Option<Resolution>, by: &Actor, now: u64) -> Status {
        let from = self.todos[i].status;
        if from == to {
            return from;
        }
        let rank = self.end_rank(&self.todos[i].board, to, &self.todos[i].id);
        let todo = &mut self.todos[i];
        todo.status = to;
        todo.rank = rank;
        if to.closed() {
            todo.resolution = Some(resolution.unwrap_or(if to == Status::Done { Resolution::Done } else { Resolution::WontDo }));
            todo.done_ms = Some(now);
            todo.claimed_by = None;
        } else {
            todo.resolution = None;
            todo.done_ms = None;
        }
        record(todo, by, now, LogKind::History, format!("{} → {}", from.label(), to.label()));
        from
    }

    fn event(&self, kind: &str, i: usize, by: &Actor, extra: serde_json::Value) -> TodoEvent {
        let todo = &self.todos[i];
        let mut data = serde_json::json!({
            "id": todo.id,
            "title": todo.title,
            "type": todo.kind.word(),
            "priority": todo.priority.word(),
            "status": todo.status.word(),
            "board": todo.board,
            "board_name": self.board(&todo.board).map(|b| b.name.as_str()).unwrap_or_default(),
            "by": by.label(),
            "by_session": by.session.clone().unwrap_or_default(),
        });
        if let (Some(data), serde_json::Value::Object(extra)) = (data.as_object_mut(), extra) {
            data.extend(extra);
        }
        TodoEvent { kind: kind.into(), board: todo.board.clone(), data }
    }

    /// `todo.done` for done, else `todo.status`, with `from` and `to`.
    fn status_event(&self, i: usize, from: Status, by: &Actor) -> TodoEvent {
        let to = self.todos[i].status;
        let kind = if to == Status::Done { "todo.done" } else { "todo.status" };
        self.event(kind, i, by, serde_json::json!({ "from": from.word(), "to": to.word() }))
    }

    /// Over the WIP limit of the board's top-level board.
    fn wip_warning(&self, board: &str) -> Option<String> {
        let root = self.root(board)?;
        let limit = root.wip?;
        let count = self.in_progress(&root.id);
        (count > limit as usize).then(|| format!("{} has {count} todos in progress, over its WIP limit of {limit}", root.name))
    }

    /// Agents move todos between backlog, todo, in progress and in review;
    /// closing and reopening only where the board lets them, never a
    /// proposal, and never the status of a todo another live session claimed.
    fn agent_may_move(&self, i: usize, to: Status, actor: &Actor, ctx: &Ctx) -> Result<(), String> {
        let todo = &self.todos[i];
        if todo.status == Status::Proposed {
            return Err(format!("{} is a proposal: the user accepts or declines it", todo.id));
        }
        if (to.closed() || todo.status.closed()) && !self.policy(&todo.board).agents_may_close {
            return Err(format!(
                "closing and reopening todos is the user's (or a merged pull request's): when {id} is finished, run atelier todo submit {id} --pr URL",
                id = todo.id
            ));
        }
        self.not_held_by_another(i, actor, ctx)
    }

    /// Refuses an agent's change of a todo another live session claimed
    /// (people and bots may; a claim of a session that is gone is stale).
    fn not_held_by_another(&self, i: usize, actor: &Actor, ctx: &Ctx) -> Result<(), String> {
        let todo = &self.todos[i];
        match todo.claimed_by.as_ref().filter(|c| actor.is_agent() && !c.by.same(actor)) {
            Some(claim) if claim.by.session.as_deref().is_none_or(|s| ctx.live.as_ref().is_none_or(|live| live.contains(s))) => {
                Err(format!("{} is claimed by {}: only that session (or the user) moves or closes it", todo.id, claim.by.label()))
            }
            _ => Ok(()),
        }
    }

    /// `id` waits for `target`, directly or through other todos.
    fn waits_for(&self, id: &str, target: &str) -> bool {
        let (mut stack, mut seen) = (vec![id.to_owned()], BTreeSet::new());
        while let Some(current) = stack.pop() {
            if same_id(&current, target) {
                return true;
            }
            if seen.insert(current.clone())
                && let Some(todo) = self.todo(&current)
            {
                stack.extend(todo.blocked_by().map(str::to_owned));
            }
        }
        false
    }

    /// `id` is `ancestor` or one of its descendants (parents).
    fn descends_from(&self, id: &str, ancestor: &str) -> bool {
        let (mut current, mut seen) = (Some(id.to_owned()), BTreeSet::new());
        while let Some(at) = current {
            if same_id(&at, ancestor) {
                return true;
            }
            if !seen.insert(at.clone()) {
                return false;
            }
            current = self.todo(&at).and_then(|t| t.parent.clone());
        }
        false
    }

    fn check_groups(&self, ctx: &Ctx, groups: &[String]) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        for group in groups.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
            if !ctx.groups.is_empty() && !ctx.groups.contains_key(group) {
                return Err(format!("no canvas group {group:?} on this canvas"));
            }
            if !out.iter().any(|g| g == group) {
                out.push(group.to_owned());
            }
        }
        Ok(out)
    }

    fn new_board_id(&self, name: &str, parent: Option<&str>) -> String {
        let base = match parent {
            Some(parent) => format!("{parent}-{}", slug(name)),
            None => slug(name),
        };
        let (mut id, mut n) = (base.clone(), 2);
        while self.board(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}
impl TodoBook {
    fn run(&mut self, op: TodoOp, actor: &Actor, ctx: &Ctx) -> Result<Applied, String> {
        let now = ctx.now;
        let mut out = Applied::default();
        match op {
            TodoOp::AddBoard { name, prefix, parent, groups, x, y } => {
                let name = clean_line(&name, "board name", 80)?;
                if self.boards.len() >= MAX_BOARDS {
                    return Err(format!("a canvas has at most {MAX_BOARDS} boards"));
                }
                let parent = match parent.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                    Some(text) => {
                        let id = self.board_id(text)?;
                        if self.board(&id).is_some_and(|b| b.parent.is_some()) {
                            return Err(format!("{text:?} is a sub-board: sub-boards are swimlanes of a top-level board"));
                        }
                        // Agents add swimlanes only to boards of their own group.
                        if actor.is_agent() && !ctx.group.as_ref().is_some_and(|g| self.board(&id).is_some_and(|b| b.canvas_groups.contains(g))) {
                            return Err(format!("{text:?} is not linked to your canvas group: ask the user to add the swimlane"));
                        }
                        Some(id)
                    }
                    None => None,
                };
                if self.boards.iter().any(|b| b.parent == parent && b.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("there is a board called {name:?} already"));
                }
                let asked = prefix.as_deref().map(|p| p.trim().to_ascii_uppercase()).filter(|p| !p.is_empty());
                let prefix = match (&parent, asked) {
                    (Some(parent), asked) => {
                        let shared = self.board(parent).map(|b| b.prefix.clone()).unwrap_or_default();
                        if asked.is_some_and(|a| a != shared) {
                            return Err(format!("a sub-board shares its parent's prefix ({shared})"));
                        }
                        shared
                    }
                    (None, Some(asked)) => {
                        if !valid_prefix(&asked) {
                            return Err(format!("{asked:?} is not a prefix: 2 to 5 letters A-Z, like DEC"));
                        }
                        if let Some(other) = self.boards.iter().find(|b| b.parent.is_none() && b.prefix == asked) {
                            return Err(format!("the prefix {asked} belongs to the board {:?}", other.name));
                        }
                        asked
                    }
                    (None, None) => self.suggest_prefix(&name),
                };
                // An agent's board belongs to its own group.
                let groups = if actor.is_agent() { ctx.group.iter().cloned().collect() } else { self.check_groups(ctx, &groups)? };
                let sub = parent.is_some();
                let right = self.boards.iter().filter(|b| b.parent.is_none()).map(|b| b.x.saturating_add(b.w).saturating_add(4)).max().unwrap_or(0);
                let id = self.new_board_id(&name, parent.as_deref());
                let linked: Vec<&str> = groups.iter().map(|g| ctx.groups.get(g).map_or(g.as_str(), String::as_str)).collect();
                out.message = format!(
                    "Board {name:?} ({prefix}) created{}{}.",
                    parent.as_deref().and_then(|p| self.board(p)).map(|p| format!(" as a swimlane of {:?}", p.name)).unwrap_or_default(),
                    if linked.is_empty() { String::new() } else { format!(", linked to {}", linked.join(", ")) }
                );
                self.boards.push(Board {
                    id: id.clone(),
                    name,
                    prefix,
                    parent,
                    x: if sub { 0 } else { x.unwrap_or(right).clamp(-MAX_COORD, MAX_COORD) },
                    y: if sub { 0 } else { y.unwrap_or(0).clamp(-MAX_COORD, MAX_COORD) },
                    w: if sub { 0 } else { BOARD_W },
                    h: if sub { 0 } else { BOARD_H },
                    canvas_groups: groups,
                    created_by: actor.clone(),
                    created_ms: now,
                    ..Default::default()
                });
                out.changed.push(id.clone());
                out.created = Some(id);
            }
            TodoOp::UpdateBoard { board, name, prefix, wip, color, notes, policy, statuses } => {
                only_people(actor, "change boards")?;
                let id = self.board_id(&board)?;
                let parent = self.board(&id).and_then(|b| b.parent.clone());
                if let Some(name) = name {
                    let name = clean_line(&name, "board name", 80)?;
                    if self.boards.iter().any(|b| b.id != id && b.parent == parent && b.name.eq_ignore_ascii_case(&name)) {
                        return Err(format!("there is a board called {name:?} already"));
                    }
                    self.boards.iter_mut().filter(|b| b.id == id).for_each(|b| b.name = name.clone());
                }
                if let Some(prefix) = prefix.map(|p| p.trim().to_ascii_uppercase()) {
                    if parent.is_some() {
                        return Err("a sub-board uses its parent's prefix".into());
                    }
                    if !valid_prefix(&prefix) {
                        return Err(format!("{prefix:?} is not a prefix: 2 to 5 letters A-Z, like DEC"));
                    }
                    if self.prefix_taken(&prefix, Some(&id)) {
                        return Err(format!("the prefix {prefix} belongs to another board"));
                    }
                    // Its sub-boards follow; existing todos keep their ids.
                    self.boards.iter_mut().filter(|b| b.id == id || b.parent.as_deref() == Some(id.as_str())).for_each(|b| b.prefix = prefix.clone());
                }
                let board = self.boards.iter_mut().find(|b| b.id == id).ok_or("the board went missing")?;
                if let Some(wip) = wip {
                    board.wip = (wip > 0).then_some(wip);
                }
                if let Some(color) = color {
                    board.color = color;
                }
                if let Some(notes) = notes {
                    board.notes = Some(notes.trim().to_owned()).filter(|n| !n.is_empty());
                }
                if let Some(policy) = policy {
                    board.policy = policy;
                }
                if let Some(statuses) = statuses {
                    board.statuses = statuses;
                }
                out.message = format!("Board {:?} updated.", board.name);
                out.changed.push(id);
            }
            TodoOp::MoveBoard { board, x, y, w, h } => {
                only_people(actor, "move boards")?;
                let id = self.board_id(&board)?;
                let board = self.boards.iter_mut().find(|b| b.id == id).ok_or("the board went missing")?;
                (board.x, board.y) = (x.clamp(-MAX_COORD, MAX_COORD), y.clamp(-MAX_COORD, MAX_COORD));
                if let Some(w) = w {
                    board.w = w.clamp(30, 2_000);
                }
                if let Some(h) = h {
                    board.h = h.clamp(8, 1_000);
                }
                out.message = format!("Board {:?} moved.", board.name);
                out.changed.push(id);
            }
            TodoOp::LinkBoard { board, group, unlink } => {
                let id = self.board_id(&board)?;
                let group = group.trim().to_owned();
                // A group that left the canvas can still be unlinked.
                let stale = unlink && self.board(&id).is_some_and(|b| b.canvas_groups.contains(&group));
                if group.is_empty() || (!stale && !ctx.groups.is_empty() && !ctx.groups.contains_key(&group)) {
                    return Err(format!("no canvas group {group:?} on this canvas"));
                }
                if actor.is_agent() && (unlink || ctx.group.as_deref() != Some(group.as_str())) {
                    return Err("agents link boards to their own canvas group only (and never unlink them): ask the user".into());
                }
                let name = ctx.groups.get(&group).cloned().unwrap_or_else(|| group.clone());
                let board = self.boards.iter_mut().find(|b| b.id == id).ok_or("the board went missing")?;
                let linked = board.canvas_groups.contains(&group);
                out.message = match (unlink, linked) {
                    (false, true) => format!("Board {:?} is linked to {name:?} already.", board.name),
                    (false, false) => {
                        board.canvas_groups.push(group);
                        format!("Board {:?} linked to {name:?}.", board.name)
                    }
                    (true, true) => {
                        board.canvas_groups.retain(|g| *g != group);
                        format!("Board {:?} is no longer linked to {name:?}.", board.name)
                    }
                    (true, false) => format!("Board {:?} was not linked to {name:?}.", board.name),
                };
                out.changed.push(id);
            }
            TodoOp::DeleteBoard { board, with_todos } => {
                only_people(actor, "delete boards")?;
                let id = self.board_id(&board)?;
                let lanes = self.lane_ids(&id);
                let name = self.board(&id).map(|b| b.name.clone()).unwrap_or_default();
                let gone: Vec<String> = self.todos.iter().filter(|t| lanes.contains(&t.board)).map(|t| t.id.clone()).collect();
                if !gone.is_empty() && !with_todos {
                    return Err(format!("{name:?} has {} todo(s): move or delete them first (or delete the board with its todos)", gone.len()));
                }
                self.remove_todos(&gone, &mut out);
                self.boards.retain(|b| !lanes.contains(&b.id));
                out.message = format!("Board {name:?} deleted{}.", if gone.is_empty() { String::new() } else { format!(" with its {} todo(s)", gone.len()) });
                out.changed.extend(lanes);
            }
            TodoOp::Add(new) => self.add(*new, actor, ctx, &mut out)?,
            TodoOp::Update(update) => self.update(*update, actor, now, &mut out)?,
            TodoOp::Move { id, status, board, after, before, rank } => self.move_todo(MoveTo { id, status, board, after, before, rank }, actor, ctx, &mut out)?,
            other => self.run_todo(other, actor, ctx, &mut out)?,
        }
        Ok(out)
    }

    /// Removes todos; relations to them go, their children lose their parent.
    fn remove_todos(&mut self, ids: &[String], out: &mut Applied) {
        self.todos.retain(|t| !ids.contains(&t.id));
        for todo in &mut self.todos {
            let before = (todo.relations.len(), todo.parent.clone());
            todo.relations.retain(|r| !ids.iter().any(|id| same_id(id, &r.id)));
            if todo.parent.as_deref().is_some_and(|p| ids.iter().any(|id| same_id(id, p))) {
                todo.parent = None;
            }
            if (todo.relations.len(), todo.parent.clone()) != before {
                out.changed.push(todo.id.clone());
            }
        }
        out.changed.extend(ids.iter().cloned());
    }

    fn add(&mut self, new: NewTodo, actor: &Actor, ctx: &Ctx, out: &mut Applied) -> Result<(), String> {
        let now = ctx.now;
        if self.todos.len() >= MAX_TODOS {
            return Err(format!("a canvas has at most {MAX_TODOS} todos"));
        }
        let title = clean_title(&new.title)?;
        let board = if new.board.trim().is_empty() {
            self.default_board(ctx.group.as_deref()).map(|b| b.id.clone()).ok_or("no todo board is linked to your canvas group: name one with --board (atelier todo boards lists them)")?
        } else {
            self.board_id(&new.board)?
        };
        let policy = self.policy(&board);
        let why = new.why.as_deref().map(|w| clean_text(w, "reason")).transpose()?;
        let (status, proposal) = if new.propose || new.status == Some(Status::Proposed) {
            if actor.is_agent() && why.is_none() {
                return Err("a proposal needs a reason: say why it is worth doing (--why)".into());
            }
            (Status::Proposed, Some(Proposal { why: why.unwrap_or_default(), ms: now }))
        } else if actor.is_agent() && policy.agent_adds == AddPolicy::Proposed {
            let why = why.unwrap_or_else(|| "added by an agent (this board makes agents' todos proposals)".into());
            (Status::Proposed, Some(Proposal { why, ms: now }))
        } else {
            let status = new.status.unwrap_or(Status::Backlog);
            if actor.is_agent() && !matches!(status, Status::Backlog | Status::Todo) {
                return Err(format!("agents add todos to backlog or todo, not {} (the default is backlog)", status.word()));
            }
            if status.closed() {
                return Err("a new todo cannot start done or canceled".into());
            }
            (status, None)
        };
        let parent = new.parent.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(|p| self.todo_id(p)).transpose()?;
        let mut relations = Vec::new();
        if let Some(from) = new.discovered_from.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            relations.push(Relation { kind: RelKind::DiscoveredFrom, id: self.todo_id(from)? });
        }
        let checklist: Vec<CheckItem> = new.checklist.iter().map(|c| clean_line(c, "checklist item", MAX_TEXT).map(|text| CheckItem { text, done: false })).collect::<Result<_, _>>()?;
        if checklist.len() > MAX_CHECKLIST {
            return Err(format!("a checklist has at most {MAX_CHECKLIST} items"));
        }
        let blocked = new.blocked.as_deref().map(str::trim).filter(|b| !b.is_empty() && !b.eq_ignore_ascii_case("none")).map(|b| clean_line(b, "blocked reason", 500)).transpose()?;
        let prefix = self.root(&board).map(|b| b.prefix.clone()).filter(|p| valid_prefix(p)).ok_or("the board has no prefix")?;
        let id = self.next_id(&prefix)?;
        let mut todo = Todo {
            id: id.clone(),
            title,
            kind: new.kind,
            status,
            priority: new.priority.unwrap_or_default(),
            blocked,
            blocker: new.blocker,
            rank: self.end_rank(&board, status, ""),
            board: board.clone(),
            parent,
            body: clean_body(&new.body)?,
            labels: clean_labels(&new.labels)?,
            checklist,
            relations,
            proposal,
            created_by: actor.clone(),
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        };
        record(&mut todo, actor, now, LogKind::History, if status == Status::Proposed { "proposed".into() } else { format!("created in {}", status.label()) });
        let board_name = self.board(&board).map(|b| b.name.clone()).unwrap_or_default();
        self.todos.push(todo);
        let i = self.todos.len() - 1;
        if status == Status::Proposed {
            let why = self.todos[i].proposal.as_ref().map(|p| p.why.clone()).unwrap_or_default();
            out.events.push(self.event("todo.proposed", i, actor, serde_json::json!({ "why": why })));
            out.message = format!("Proposed {id} “{}” on {board_name:?}: the user accepts or declines it.", self.todos[i].title);
        } else {
            out.events.push(self.event("todo.created", i, actor, serde_json::json!({})));
            out.message = format!("Created {id} “{}” on {board_name:?} ({}).", self.todos[i].title, status.label());
        }
        if status == Status::InProgress {
            out.warnings.extend(self.wip_warning(&board));
        }
        out.changed.push(id.clone());
        out.created = Some(id);
        Ok(())
    }
}
/// Where `TodoOp::Move` takes a todo.
struct MoveTo {
    id: String,
    status: Option<Status>,
    board: Option<String>,
    after: Option<String>,
    before: Option<String>,
    rank: Option<String>,
}

impl TodoBook {
    fn update(&mut self, u: TodoUpdate, actor: &Actor, now: u64, out: &mut Applied) -> Result<(), String> {
        let i = self.index(&u.id)?;
        let id = self.todos[i].id.clone();
        // Everything is checked before anything changes.
        let title = u.title.as_deref().map(clean_title).transpose()?;
        let none = |text: &str| text.is_empty() || text.eq_ignore_ascii_case("none");
        let blocked = u.blocked.as_deref().map(str::trim).map(|b| if none(b) { Ok(None) } else { clean_line(b, "blocked reason", 500).map(Some) }).transpose()?;
        let body = u.body.as_deref().map(clean_body).transpose()?;
        let parent = match u.parent.as_deref().map(str::trim) {
            None => None,
            Some(p) if none(p) => Some(None),
            Some(p) => {
                let parent = self.todo_id(p)?;
                if self.descends_from(&parent, &id) {
                    return Err(format!("{parent} cannot be the parent of {id}: it is {id} or one of its children"));
                }
                Some(Some(parent))
            }
        };
        let note = u.note.as_deref().map(str::trim).map(|n| if none(n) { Ok(None) } else { clean_line(n, "note path", 1_000).map(Some) }).transpose()?;
        let mut labels = match &u.labels {
            Some(list) => clean_labels(list)?,
            None => self.todos[i].labels.clone(),
        };
        labels.extend(clean_labels(&u.add_labels)?);
        let remove = clean_labels(&u.remove_labels)?;
        labels.retain(|l| !remove.contains(l));
        let labels = clean_labels(&labels)?;
        let todo = &mut self.todos[i];
        let mut what: Vec<&str> = Vec::new();
        if let Some(title) = title.filter(|t| *t != todo.title) {
            todo.title = title;
            what.push("title");
        }
        if let Some(kind) = u.kind.filter(|k| *k != todo.kind) {
            todo.kind = kind;
            what.push("type");
        }
        if let Some(priority) = u.priority.filter(|p| *p != todo.priority) {
            todo.priority = priority;
            what.push("priority");
        }
        if let Some(blocked) = blocked.filter(|b| *b != todo.blocked) {
            todo.blocked = blocked;
            what.push("blocked");
        }
        if let Some(blocker) = u.blocker.filter(|b| *b != todo.blocker) {
            todo.blocker = blocker;
            what.push("blocker");
        }
        if let Some(body) = body.filter(|b| *b != todo.body) {
            todo.body = body;
            what.push("body");
        }
        if labels != todo.labels {
            todo.labels = labels;
            what.push("labels");
        }
        if let Some(parent) = parent.filter(|p| *p != todo.parent) {
            todo.parent = parent;
            what.push("parent");
        }
        if let Some(note) = note.filter(|n| *n != todo.note) {
            todo.note = note;
            what.push("note");
        }
        if what.is_empty() {
            out.message = format!("Nothing changed on {id}.");
            return Ok(());
        }
        record(todo, actor, now, LogKind::History, format!("changed {}", what.join(", ")));
        out.message = format!("Updated {id}: {}.", what.join(", "));
        out.changed.push(id);
        Ok(())
    }

    fn move_todo(&mut self, to: MoveTo, actor: &Actor, ctx: &Ctx, out: &mut Applied) -> Result<(), String> {
        let now = ctx.now;
        let i = self.index(&to.id)?;
        let id = self.todos[i].id.clone();
        let from = self.todos[i].status;
        let status = to.status.unwrap_or(from);
        if status == Status::Proposed && from != Status::Proposed {
            return Err(format!("only agents' new ideas are proposals: move {id} to backlog instead"));
        }
        if actor.is_agent() && status != from {
            self.agent_may_move(i, status, actor, ctx)?;
        }
        let board = match to.board.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            Some(text) => self.board_id(text)?,
            None => self.todos[i].board.clone(),
        };
        let after = to.after.as_deref().map(|a| self.todo_id(a)).transpose()?;
        let before = to.before.as_deref().map(|b| self.todo_id(b)).transpose()?;
        if after.as_deref() == Some(id.as_str()) || before.as_deref() == Some(id.as_str()) {
            return Err(format!("{id} cannot move next to itself"));
        }
        let board_name = self.board(&board).map(|b| b.name.clone()).unwrap_or_default();
        let moved_board = board != self.todos[i].board;
        if moved_board {
            let todo = &mut self.todos[i];
            todo.board = board.clone();
            record(todo, actor, now, LogKind::History, format!("moved to the board {board_name:?}"));
        }
        if status != from {
            // A proposal dropped on Canceled is declined.
            let resolution = (from == Status::Proposed && status == Status::Canceled).then_some(Resolution::Declined);
            self.set_status(i, status, resolution, actor, now);
        } else if moved_board {
            self.todos[i].rank = self.end_rank(&board, status, &id);
        }
        if to.rank.is_some() || after.is_some() || before.is_some() {
            let rank_of = |other: &Option<String>| other.as_deref().and_then(|o| self.todo(o)).map(|t| t.rank.clone()).filter(|r| valid_rank(r));
            let mut column: Vec<&Todo> = self.todos.iter().filter(|t| t.board == board && t.status == status && t.id != id).collect();
            column.sort_by(super::by_rank);
            let ranks: Vec<&str> = column.iter().map(|t| t.rank.as_str()).collect();
            let rank = match (to.rank.filter(|r| valid_rank(r)), rank_of(&after), rank_of(&before)) {
                (Some(rank), _, _) => rank,
                (None, Some(a), Some(b)) => rank_between(Some(a.as_str()), Some(b.as_str())),
                (None, Some(a), None) => rank_between(Some(a.as_str()), ranks.iter().copied().find(|r| *r > a.as_str())),
                (None, None, Some(b)) => rank_between(ranks.iter().copied().rev().find(|r| *r < b.as_str()), Some(b.as_str())),
                (None, None, None) => self.end_rank(&board, status, &id),
            };
            self.todos[i].rank = rank;
        }
        self.tidy_ranks(&board, status, out);
        if status != from {
            if from == Status::Proposed && status.open() {
                out.events.push(self.event("todo.accepted", i, actor, serde_json::json!({ "from": from.word(), "to": status.word() })));
            } else {
                out.events.push(self.status_event(i, from, actor));
            }
            if status == Status::InProgress {
                out.warnings.extend(self.wip_warning(&board));
            }
        }
        let column = self.root(&board).map(|b| b.status_name(status)).unwrap_or_else(|| status.label().to_owned());
        out.message = match (status != from, moved_board) {
            (true, true) => format!("Moved {id} to {board_name:?} › {column}."),
            (true, false) => format!("Moved {id} to {column}."),
            (false, true) => format!("Moved {id} to {board_name:?}."),
            (false, false) => format!("Moved {id} within {column}."),
        };
        out.changed.push(id);
        Ok(())
    }
}
impl TodoBook {
    /// The operations on one todo (and the book's settings).
    fn run_todo(&mut self, op: TodoOp, actor: &Actor, ctx: &Ctx, out: &mut Applied) -> Result<(), String> {
        let now = ctx.now;
        let id = match &op {
            TodoOp::SetMirror { path } => return self.set_mirror(path.as_deref(), actor, out),
            TodoOp::Import { board, prefix, text } => return self.import(board, prefix.as_deref(), text, actor, ctx, out),
            other => other.todo_id().map(str::to_owned).ok_or_else(|| format!("{} is not a todo operation", other.word()))?,
        };
        let i = self.index(&id)?;
        let id = self.todos[i].id.clone();
        let title = self.todos[i].title.clone();
        let board = self.todos[i].board.clone();
        let policy = self.policy(&board);
        let status = self.todos[i].status;
        let optional = |text: Option<&str>, what: &str| text.map(str::trim).filter(|t| !t.is_empty()).map(|t| clean_text(t, what)).transpose();
        match op {
            TodoOp::Accept { status: to, .. } => {
                only_people(actor, "accept proposals")?;
                if status != Status::Proposed {
                    return Err(format!("{id} is not a proposal (it is {})", status.label().to_lowercase()));
                }
                let to = to.unwrap_or(Status::Backlog);
                if !to.open() {
                    return Err("an accepted todo goes to backlog, todo, in progress or in review".into());
                }
                self.set_status(i, to, None, actor, now);
                if to == Status::InProgress {
                    out.warnings.extend(self.wip_warning(&board));
                }
                out.events.push(self.event("todo.accepted", i, actor, json!({ "from": status.word(), "to": to.word() })));
                out.message = format!("Accepted {id} “{title}” ({}).", to.label());
            }
            TodoOp::Decline { reason, .. } => {
                only_people(actor, "decline proposals")?;
                if status != Status::Proposed {
                    return Err(format!("{id} is not a proposal (cancel it instead)"));
                }
                let reason = optional(reason.as_deref(), "reason")?;
                self.set_status(i, Status::Canceled, Some(Resolution::Declined), actor, now);
                if let Some(reason) = reason {
                    record(&mut self.todos[i], actor, now, LogKind::Note, format!("declined: {reason}"));
                }
                let mut event = self.status_event(i, status, actor);
                event.data["resolution"] = "declined".into();
                out.events.push(event);
                out.message = format!("Declined {id} “{title}”.");
            }
            TodoOp::Claim { force, session, .. } => {
                if status == Status::Proposed {
                    return Err(format!("{id} is a proposal: the user accepts it first"));
                }
                if status.closed() {
                    return Err(format!("{id} is {}: it needs reopening first", status.label().to_lowercase()));
                }
                let claimant = claimant(actor, session.as_deref(), ctx)?;
                let mine = claimant.same(actor);
                if let Some(claim) = self.todos[i].claimed_by.clone() {
                    if claim.by.same(&claimant) {
                        out.message = if mine { format!("{id} is yours already (claimed {}).", date(claim.ms)) } else { format!("{id} is claimed by {} already (since {}).", claimant.label(), date(claim.ms)) };
                        return Ok(());
                    }
                    let live = claim.by.session.as_deref().is_none_or(|s| ctx.live.as_ref().is_none_or(|live| live.contains(s)));
                    let holder = format!("{} (since {})", claim.by.label(), date(claim.ms));
                    if live && actor.is_agent() {
                        let forced = if force { " (only the user can force a claim)" } else { "" };
                        return Err(format!("{id} is claimed by {holder}{forced}: pick another todo (atelier todo ready) or ask the user to reassign it"));
                    }
                    if live && !force {
                        return Err(format!("{id} is claimed by {holder}: force the claim to take it over"));
                    }
                    out.warnings.push(if live { format!("took {id} over from {}", claim.by.label()) } else { format!("took over the claim of {}, which is gone or stopped", claim.by.label()) });
                }
                let mut why: Vec<String> = self.todos[i].blocked.iter().cloned().collect();
                why.extend(self.blockers(&self.todos[i]).iter().map(|b| format!("waits for {} ({})", b.id, b.status.label().to_lowercase())));
                if !why.is_empty() {
                    out.warnings.push(format!("{id} is blocked: {}", why.join("; ")));
                }
                self.todos[i].claimed_by = Some(Claim { by: claimant.clone(), ms: now });
                record(&mut self.todos[i], actor, now, LogKind::History, format!("claimed {} {}", if mine { "by" } else { "for" }, claimant.label()));
                if matches!(status, Status::Backlog | Status::Todo) {
                    self.set_status(i, Status::InProgress, None, actor, now);
                    out.warnings.extend(self.wip_warning(&board));
                }
                let to = self.todos[i].status;
                let mut extra = json!({ "from": status.word(), "to": to.word() });
                if !mine {
                    extra["for"] = claimant.label().into();
                    extra["for_session"] = claimant.session.clone().unwrap_or_default().into();
                }
                out.events.push(self.event("todo.claimed", i, actor, extra));
                out.message = if mine {
                    format!("Claimed {id} “{title}”: it is yours and {}. Note progress with todo note / todo check; when it is finished: todo submit {id} --pr URL.", to.label().to_lowercase())
                } else {
                    format!("Claimed {id} “{title}” for {}: it is {} (that session works on it; tell it so if it does not know yet).", claimant.label(), to.label().to_lowercase())
                };
            }
            TodoOp::Release { .. } => {
                let claim = self.todos[i].claimed_by.clone().ok_or_else(|| format!("{id} is not claimed"))?;
                if actor.is_agent() && !claim.by.same(actor) {
                    return Err(format!("{id} is claimed by {}, not you: only that session (or the user) releases it", claim.by.label()));
                }
                self.todos[i].claimed_by = None;
                record(&mut self.todos[i], actor, now, LogKind::History, format!("released (was claimed by {})", claim.by.label()));
                if status == Status::InProgress {
                    self.set_status(i, Status::Todo, None, actor, now);
                    out.events.push(self.status_event(i, status, actor));
                }
                out.message = format!("Released {id}{}.", if status == Status::InProgress { " (back to todo)" } else { "" });
            }
            TodoOp::Note { text, .. } => {
                let text = clean_text(&text, "note")?;
                record(&mut self.todos[i], actor, now, LogKind::Note, text);
                out.message = format!("Noted on {id}.");
            }
            TodoOp::Check { index, done, .. } => {
                let count = self.todos[i].checklist.len();
                if index >= count {
                    return Err(if count == 0 { format!("{id} has no checklist (add items with todo check {id} --add TEXT)") } else { format!("{id} has {count} checklist item(s): pick 1 to {count}") });
                }
                let item = &mut self.todos[i].checklist[index];
                let (text, changed) = (item.text.clone(), item.done != done);
                item.done = done;
                if changed {
                    record(&mut self.todos[i], actor, now, LogKind::History, format!("{} “{text}”", if done { "checked" } else { "unchecked" }));
                }
                let (checked, all) = self.todos[i].checked();
                out.message = format!("{id}: {checked}/{all} checklist items done.");
            }
            TodoOp::AddCheck { text, .. } => {
                let text = clean_line(&text, "checklist item", MAX_TEXT)?;
                if self.todos[i].checklist.len() >= MAX_CHECKLIST {
                    return Err(format!("a checklist has at most {MAX_CHECKLIST} items"));
                }
                self.todos[i].checklist.push(CheckItem { text: text.clone(), done: false });
                record(&mut self.todos[i], actor, now, LogKind::History, format!("added checklist item “{text}”"));
                out.message = format!("Added checklist item {} to {id}.", self.todos[i].checklist.len());
            }
            TodoOp::EditCheck { index, text, .. } => {
                let text = clean_line(&text, "checklist item", MAX_TEXT)?;
                let count = self.todos[i].checklist.len();
                let item = self.todos[i].checklist.get_mut(index).ok_or_else(|| format!("{id} has {count} checklist item(s): pick 1 to {count}"))?;
                if item.text == text {
                    out.message = format!("Nothing changed on {id}.");
                    return Ok(());
                }
                let old = std::mem::replace(&mut item.text, text.clone());
                record(&mut self.todos[i], actor, now, LogKind::History, format!("reworded checklist item “{old}” as “{text}”"));
                out.message = format!("Changed checklist item {} of {id}.", index + 1);
            }
            TodoOp::RemoveCheck { index, .. } => {
                only_people(actor, "remove checklist items")?;
                let count = self.todos[i].checklist.len();
                if index >= count {
                    return Err(format!("{id} has {count} checklist item(s): pick 1 to {count}"));
                }
                let item = self.todos[i].checklist.remove(index);
                record(&mut self.todos[i], actor, now, LogKind::History, format!("removed checklist item “{}”", item.text));
                out.message = format!("Removed checklist item {} from {id}.", index + 1);
            }
            TodoOp::Link { kind, target, title: link_title, .. } => {
                let (target, default_title) = clean_link(kind, &target)?;
                let link_title = link_title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(|t| clean_line(t, "link title", 200)).transpose()?.unwrap_or(default_title);
                let todo = &mut self.todos[i];
                if todo.links.iter().any(|l| l.kind == kind && l.target == target) {
                    out.message = format!("{id} links to {target} already.");
                    return Ok(());
                }
                if todo.links.len() >= MAX_LINKS {
                    return Err(format!("a todo has at most {MAX_LINKS} links"));
                }
                todo.links.push(Link { kind, target: target.clone(), title: link_title });
                if kind == LinkKind::Note && todo.note.is_none() {
                    todo.note = Some(target.clone());
                }
                record(todo, actor, now, LogKind::History, format!("linked {} {target}", kind.word()));
                out.message = format!("Linked {} {target} to {id}.", kind.word());
            }
            TodoOp::Unlink { target, .. } => {
                let target = target.trim().to_owned();
                let key = pr_key(&target);
                let todo = &mut self.todos[i];
                let before = todo.links.len();
                todo.links.retain(|l| l.target != target && (key.is_none() || pr_key(&l.target) != key));
                if todo.links.len() == before {
                    return Err(format!("{id} has no link to {target:?}"));
                }
                if todo.note.as_deref() == Some(target.as_str()) {
                    todo.note = None;
                }
                record(todo, actor, now, LogKind::History, format!("unlinked {target}"));
                out.message = format!("Unlinked {target} from {id}.");
            }
            TodoOp::Relate { kind, other, .. } => {
                let other = self.todo_id(&other)?;
                if other == id {
                    return Err(format!("{id} cannot relate to itself"));
                }
                if kind == RelKind::BlockedBy && self.waits_for(&other, &id) {
                    return Err(format!("{other} already waits for {id}: blocking each other would never end"));
                }
                let todo = &mut self.todos[i];
                if todo.relations.iter().any(|r| r.kind == kind && r.id == other) {
                    out.message = format!("{id} is {} {other} already.", kind.label());
                    return Ok(());
                }
                if todo.relations.len() >= MAX_RELATIONS {
                    return Err(format!("a todo has at most {MAX_RELATIONS} relations"));
                }
                todo.relations.push(Relation { kind, id: other.clone() });
                record(todo, actor, now, LogKind::History, format!("{} {other}", kind.label()));
                out.message = format!("{id} is {} {other} now.", kind.label());
                out.changed.push(other);
            }
            TodoOp::Unrelate { other, kind, .. } => {
                let other = self.todo(&other).map(|t| t.id.clone()).unwrap_or_else(|| other.trim().to_owned());
                let todo = &mut self.todos[i];
                let before = todo.relations.len();
                todo.relations.retain(|r| !(same_id(&r.id, &other) && kind.is_none_or(|k| k == r.kind)));
                if todo.relations.len() == before {
                    return Err(format!("{id} has no such relation to {other}"));
                }
                record(todo, actor, now, LogKind::History, format!("no longer related to {other}"));
                out.message = format!("{id} is no longer related to {other}.");
            }
            TodoOp::Submit { pr, summary, .. } => {
                if status == Status::Proposed {
                    return Err(format!("{id} is a proposal: the user accepts it first"));
                }
                if status.closed() {
                    return Err(format!("{id} is {}: it needs reopening first", status.label().to_lowercase()));
                }
                if actor.is_agent()
                    && let Some(claim) = self.todos[i].claimed_by.as_ref().filter(|c| !c.by.same(actor))
                    && claim.by.session.as_deref().is_none_or(|s| ctx.live.as_ref().is_none_or(|live| live.contains(s)))
                {
                    return Err(format!("{id} is claimed by {}: only that session (or the user) submits it", claim.by.label()));
                }
                let pr = pr.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(|p| clean_link(LinkKind::Pr, p)).transpose()?;
                let summary = optional(summary.as_deref(), "summary")?;
                let todo = &mut self.todos[i];
                if let Some((url, short)) = &pr
                    && !todo.links.iter().any(|l| l.kind == LinkKind::Pr && l.target == *url)
                {
                    if todo.links.len() >= MAX_LINKS {
                        return Err(format!("a todo has at most {MAX_LINKS} links"));
                    }
                    todo.links.push(Link { kind: LinkKind::Pr, target: url.clone(), title: short.clone() });
                }
                if let Some(summary) = summary {
                    record(todo, actor, now, LogKind::Note, format!("submitted: {summary}"));
                }
                self.set_status(i, Status::InReview, None, actor, now);
                let url = pr.as_ref().map(|(url, _)| url.clone()).unwrap_or_default();
                out.events.push(self.event("todo.submitted", i, actor, json!({ "from": status.word(), "to": "in_review", "pr": url })));
                out.message = format!("{id} is in review{}: the user marks it done (or its merged pull request does).", pr.map(|(_, short)| format!(" with {short}")).unwrap_or_default());
            }
            TodoOp::Done { note, .. } => {
                if actor.is_agent() && !policy.agents_may_close {
                    return Err(format!("marking todos done is the user's (or a merged pull request's): when {id} is finished, run atelier todo submit {id} --pr URL"));
                }
                if actor.is_agent() && status == Status::Proposed {
                    return Err(format!("{id} is a proposal: the user accepts or declines it"));
                }
                if status == Status::Done {
                    out.message = format!("{id} is done already.");
                    return Ok(());
                }
                self.not_held_by_another(i, actor, ctx)?;
                let note = optional(note.as_deref(), "note")?;
                if let Some(note) = note {
                    record(&mut self.todos[i], actor, now, LogKind::Note, note);
                }
                self.set_status(i, Status::Done, Some(Resolution::Done), actor, now);
                out.events.push(self.status_event(i, status, actor));
                out.message = format!("Done: {id} “{title}”.");
            }
            TodoOp::Cancel { reason, resolution, .. } => {
                if actor.is_agent() && (status == Status::Proposed || !policy.agents_may_close) {
                    return Err(format!("canceling todos is the user's: say why {id} should go with atelier todo note {id} \"…\""));
                }
                if status == Status::Canceled {
                    out.message = format!("{id} is canceled already.");
                    return Ok(());
                }
                self.not_held_by_another(i, actor, ctx)?;
                let reason = optional(reason.as_deref(), "reason")?;
                let resolution = resolution.unwrap_or(if status == Status::Proposed { Resolution::Declined } else { Resolution::WontDo });
                if let Some(reason) = reason {
                    record(&mut self.todos[i], actor, now, LogKind::Note, format!("canceled: {reason}"));
                }
                self.set_status(i, Status::Canceled, Some(resolution), actor, now);
                let mut event = self.status_event(i, status, actor);
                event.data["resolution"] = resolution.word().into();
                out.events.push(event);
                out.message = format!("Canceled {id} “{title}” ({}).", resolution.word().replace('_', " "));
            }
            TodoOp::Reopen { .. } => {
                if actor.is_agent() && !policy.agents_may_close {
                    return Err("reopening todos is the user's: ask them".into());
                }
                if !status.closed() {
                    return Err(format!("{id} is not done or canceled"));
                }
                let declined = self.todos[i].resolution == Some(Resolution::Declined);
                if actor.is_agent() && declined {
                    return Err(format!("{id} is a declined proposal: only the user reopens it (propose it again with a new reason if it matters now)"));
                }
                let to = if declined { Status::Proposed } else { Status::Todo };
                self.set_status(i, to, None, actor, now);
                out.events.push(self.status_event(i, status, actor));
                out.message = format!("Reopened {id} “{title}” ({}).", to.label());
            }
            TodoOp::Delete { .. } => {
                only_people(actor, "delete todos")?;
                self.remove_todos(std::slice::from_ref(&id), out);
                out.message = format!("Deleted {id} “{title}” (its id is never used again).");
                return Ok(());
            }
            other => return Err(format!("{} is not a todo operation", other.word())),
        }
        out.changed.push(id);
        Ok(())
    }

    fn set_mirror(&mut self, path: Option<&str>, actor: &Actor, out: &mut Applied) -> Result<(), String> {
        only_people(actor, "set the mirror file")?;
        let path = path.map(str::trim).filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case("off") && !p.eq_ignore_ascii_case("none"));
        if let Some(path) = path {
            let lower = path.to_lowercase();
            if !(lower.ends_with(".md") || lower.ends_with(".markdown")) || path.contains(['\n', '\r']) {
                return Err(format!("the mirror is a Markdown file (.md), not {path:?}"));
            }
        }
        self.settings.mirror = path.map(str::to_owned);
        out.message = match path {
            Some(path) => format!("The todos are mirrored to {path} after every change (a generated file: never edit it)."),
            None => "The todos are no longer mirrored to a Markdown file.".into(),
        };
        Ok(())
    }

    fn import(&mut self, board: &str, prefix: Option<&str>, text: &str, actor: &Actor, ctx: &Ctx, out: &mut Applied) -> Result<(), String> {
        only_people(actor, "import todos")?;
        let now = ctx.now;
        let name = board.trim();
        if name.is_empty() {
            return Err("name the board to import into (it is created when missing)".into());
        }
        let prefix = prefix.map(|p| p.trim().to_ascii_uppercase()).filter(|p| !p.is_empty());
        if let Some(p) = prefix.as_deref().filter(|p| !valid_prefix(p)) {
            return Err(format!("{p:?} is not a prefix: 2 to 5 letters A-Z, like WI"));
        }
        let imported = import_markdown(text, "", prefix.as_deref().unwrap_or(""));
        if imported.todos.is_empty() {
            return Err("found no todo items (`- [ ] …` lines) to import".into());
        }
        let board = match self.find_board(name) {
            Some(found) => found.id.clone(),
            None => {
                let wanted = prefix.or_else(|| Some(imported.prefix.clone()).filter(|p| valid_prefix(p) && !self.prefix_taken(p, None)));
                let made = self.run(TodoOp::AddBoard { name: name.to_owned(), prefix: wanted, parent: None, groups: Vec::new(), x: None, y: None }, actor, ctx)?;
                out.changed.extend(made.changed);
                made.created.ok_or("the board was not created")?
            }
        };
        let board_prefix = self.root(&board).map(|b| b.prefix.clone()).filter(|p| valid_prefix(p)).ok_or("the board has no prefix")?;
        let (mut added, mut skipped, mut twice, mut statuses) = (BTreeSet::new(), Vec::new(), Vec::new(), BTreeSet::new());
        // Ids as `same_id` compares them, and each column's last rank: no scan of the book per item.
        let key = |id: &str| parse_id(id).map_or_else(|| id.trim().to_ascii_uppercase(), |(p, n)| format!("{p}-{n}"));
        let mut known: BTreeSet<String> = self.todos.iter().map(|t| key(&t.id)).collect();
        let mut ends: BTreeMap<Status, String> = BTreeMap::new();
        for mut todo in imported.todos {
            if !todo.id.is_empty() && known.contains(&key(&todo.id)) {
                // Already on the canvas, or a second item with the same id in the file.
                if added.iter().any(|a: &String| same_id(a, &todo.id)) { twice.push(todo.id) } else { skipped.push(todo.id) }
                continue;
            }
            if self.todos.len() >= MAX_TODOS {
                return Err(format!("a canvas has at most {MAX_TODOS} todos"));
            }
            if todo.id.is_empty() {
                todo.id = self.next_id(&board_prefix)?;
            }
            let mut ids = vec![todo.id.clone()];
            ids.extend(todo.relations.iter().map(|r| r.id.clone()));
            self.note_ids(&ids);
            known.insert(key(&todo.id));
            todo.board = board.clone();
            let rank = match ends.get(&todo.status) {
                Some(last) => rank_between(Some(last), None),
                None => self.end_rank(&board, todo.status, ""),
            };
            ends.insert(todo.status, rank.clone());
            todo.rank = rank;
            todo.created_by = actor.clone();
            if todo.created_ms == 0 {
                todo.created_ms = now;
            }
            todo.updated_ms = now;
            record(&mut todo, actor, now, LogKind::History, "imported".into());
            statuses.insert(todo.status);
            added.insert(todo.id.clone());
            self.todos.push(todo);
        }
        for status in statuses {
            self.tidy_ranks(&board, status, out);
        }
        let count = |test: &dyn Fn(Status) -> bool| self.todos.iter().filter(|t| added.contains(&t.id) && test(t.status)).count();
        let board_name = self.board(&board).map(|b| b.name.clone()).unwrap_or_default();
        out.message = format!(
            "Imported {} todo(s) into {board_name:?} ({board_prefix}): {} open, {} done, {} proposed{}.",
            added.len(),
            count(&|s| s.open()),
            count(&|s| s == Status::Done),
            count(&|s| s == Status::Proposed),
            if skipped.is_empty() { String::new() } else { format!("; {} already on the canvas, skipped ({})", skipped.len(), skipped.join(", ")) }
        );
        out.warnings.extend(imported.warnings);
        out.warnings.extend(twice.iter().map(|id| format!("{id} is in the file more than once: its first item was imported")));
        out.changed.extend(added);
        Ok(())
    }
}
