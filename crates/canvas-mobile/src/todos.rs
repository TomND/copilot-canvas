//! The todo boards on the phone (the desktop's Tasks space, for lists and
//! cards): the book the service sends in its bus snapshot as records, what
//! waits for you, and your changes. A change is applied to this phone's copy
//! of the book at once (as the desktop's windows do) and sent as the
//! window's bus action (`todos::client_action`); the next book the service
//! sends confirms it, or undoes it after a refusal notice.

use std::collections::{BTreeSet, HashSet};

use canvas_model::doc::{Doc, NodeKind};
use canvas_model::todos::{self, Actor, AddPolicy, BoardView, Ctx, LogKind, Need, NewTodo, Priority, Status, Todo, TodoBook, TodoOp, TodoType, TodoUpdate};
use canvas_model::{notebook, report_html};

use crate::model::{
    TodoActions, TodoBoardItem, TodoCell, TodoCheck, TodoColumn, TodoDetail, TodoDraft, TodoEdit, TodoGroupCounts, TodoItem, TodoKind, TodoLane, TodoLink, TodoLogLine, TodoNeed, TodoNeedItem, TodoPriority,
    TodoRelation, TodoStatus, TodosOverview,
};

impl From<Status> for TodoStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Proposed => TodoStatus::Proposed,
            Status::Backlog => TodoStatus::Backlog,
            Status::Todo => TodoStatus::Todo,
            Status::InProgress => TodoStatus::InProgress,
            Status::InReview => TodoStatus::InReview,
            Status::Done => TodoStatus::Done,
            Status::Canceled => TodoStatus::Canceled,
        }
    }
}

impl From<TodoStatus> for Status {
    fn from(status: TodoStatus) -> Self {
        match status {
            TodoStatus::Proposed => Status::Proposed,
            TodoStatus::Backlog => Status::Backlog,
            TodoStatus::Todo => Status::Todo,
            TodoStatus::InProgress => Status::InProgress,
            TodoStatus::InReview => Status::InReview,
            TodoStatus::Done => Status::Done,
            TodoStatus::Canceled => Status::Canceled,
        }
    }
}

impl From<TodoType> for TodoKind {
    fn from(kind: TodoType) -> Self {
        match kind {
            TodoType::Bug => TodoKind::Bug,
            TodoType::Feature => TodoKind::Feature,
            TodoType::Improvement => TodoKind::Improvement,
            TodoType::Optimization => TodoKind::Optimization,
            TodoType::Chore => TodoKind::Chore,
            TodoType::Docs => TodoKind::Docs,
            TodoType::Research => TodoKind::Research,
        }
    }
}

impl From<TodoKind> for TodoType {
    fn from(kind: TodoKind) -> Self {
        match kind {
            TodoKind::Bug => TodoType::Bug,
            TodoKind::Feature => TodoType::Feature,
            TodoKind::Improvement => TodoType::Improvement,
            TodoKind::Optimization => TodoType::Optimization,
            TodoKind::Chore => TodoType::Chore,
            TodoKind::Docs => TodoType::Docs,
            TodoKind::Research => TodoType::Research,
        }
    }
}

impl From<Priority> for TodoPriority {
    fn from(priority: Priority) -> Self {
        match priority {
            Priority::Urgent => TodoPriority::Urgent,
            Priority::High => TodoPriority::High,
            Priority::Medium => TodoPriority::Medium,
            Priority::Low => TodoPriority::Low,
            Priority::None => TodoPriority::NoPriority,
        }
    }
}

impl From<TodoPriority> for Priority {
    fn from(priority: TodoPriority) -> Self {
        match priority {
            TodoPriority::Urgent => Priority::Urgent,
            TodoPriority::High => Priority::High,
            TodoPriority::Medium => Priority::Medium,
            TodoPriority::Low => Priority::Low,
            TodoPriority::NoPriority => Priority::None,
        }
    }
}

impl From<Need> for TodoNeed {
    fn from(need: Need) -> Self {
        match need {
            Need::Proposed => TodoNeed::Proposed,
            Need::InReview => TodoNeed::InReview,
            Need::StaleClaim => TodoNeed::StaleClaim,
        }
    }
}

/// A type's glyph on the desktop's cards (one cell wide).
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

/// The canvas's sessions as the boards see them: claims by sessions that
/// are gone (not on the canvas) or stopped are stale; working ones show `⟳`.
pub struct Canvas<'a> {
    pub doc: &'a Doc,
    pub stopped: HashSet<String>,
    pub working: HashSet<String>,
}

impl Canvas<'_> {
    pub fn gone(&self, session: &str) -> bool {
        self.doc.node(session).is_none() || self.stopped.contains(session)
    }

    /// Where your operations run: now, the canvas's groups, the live sessions.
    pub fn ctx(&self, now: u64) -> Ctx {
        let live: BTreeSet<String> = self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && !self.gone(&n.id)).map(|n| n.id.clone()).collect();
        let sessions = self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).map(|n| (n.id.clone(), n.name.clone())).collect();
        Ctx { now, group: None, groups: self.doc.groups.iter().map(|g| (g.id.clone(), g.name.clone())).collect(), live: Some(live), sessions }
    }

    /// An actor's name: a session's box name on this canvas, else its label.
    fn name(&self, actor: &Actor) -> String {
        match actor.session.as_deref().and_then(|s| self.doc.node(s)) {
            Some(node) => node.name.clone(),
            None => actor.label(),
        }
    }
}

/// A todo as lists and cards show it.
pub fn item(book: &TodoBook, todo: &Todo, canvas: &Canvas) -> TodoItem {
    let board = book.board(&todo.board);
    let root = book.root(&todo.board);
    let mut why: Vec<String> = todo.blocked.iter().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()).collect();
    why.extend(book.blockers(todo).iter().map(|b| format!("waits for {}", b.id)));
    let session = todo.claim_session().filter(|s| canvas.doc.node(s).is_some());
    let (checked, all) = todo.checked();
    TodoItem {
        id: todo.id.clone(),
        title: todo.title.clone(),
        kind: todo.kind.into(),
        kind_glyph: type_glyph(todo.kind).to_owned(),
        status: todo.status.into(),
        status_label: root.map(|b| b.status_name(todo.status)).unwrap_or_else(|| todo.status.label().to_owned()),
        priority: todo.priority.into(),
        priority_marks: todo.priority.marks().to_owned(),
        blocked: book.is_blocked(todo),
        blocked_reason: why.join("; "),
        blocker: todo.blocker,
        blocks: book.blocking(&todo.id).len() as u32,
        board: todo.board.clone(),
        board_name: board.map(|b| b.name.clone()).unwrap_or_default(),
        root_board: root.map(|b| b.id.clone()).unwrap_or_else(|| todo.board.clone()),
        lane: board.filter(|b| b.parent.is_some()).map(|b| b.name.clone()).unwrap_or_default(),
        claimed_by: todo.claimed_by.as_ref().map(|c| canvas.name(&c.by)).unwrap_or_default(),
        claimed_session: session.unwrap_or_default().to_owned(),
        claim_working: session.is_some_and(|s| canvas.working.contains(s)),
        ai_created: todo.ai_created(),
        proposal_why: todo.proposal.as_ref().map(|p| p.why.clone()).unwrap_or_default(),
        checklist_done: checked as u32,
        checklist_total: all as u32,
        labels: todo.labels.clone(),
        prs: todo.prs().map(|l| l.target.clone()).collect(),
        ready: book.is_ready(todo),
        created_ms: todo.created_ms,
        updated_ms: todo.updated_ms,
    }
}

/// A top-level board with its swimlanes and columns.
fn board(view: &BoardView) -> TodoBoardItem {
    let board = view.board;
    let (done, total) = view.progress();
    TodoBoardItem {
        id: board.id.clone(),
        name: board.name.clone(),
        prefix: board.prefix.clone(),
        color: board.color,
        groups: board.canvas_groups.clone(),
        lanes: view.lanes.iter().map(|l| TodoLane { id: l.id.clone(), name: l.name.clone() }).collect(),
        columns: view
            .columns
            .iter()
            .map(|c| TodoColumn {
                status: c.status.into(),
                name: c.name.clone(),
                count: c.count as u32,
                wip: c.wip.unwrap_or(0),
                over: c.over,
                collapsed: c.collapsed,
                cells: c.cells.iter().zip(&view.lanes).map(|(cell, lane)| TodoCell { lane: lane.id.clone(), ids: cell.iter().map(|t| t.id.clone()).collect() }).collect(),
            })
            .collect(),
        canceled: view.canceled as u32,
        blocked: view.blocked as u32,
        done: done as u32,
        total: total as u32,
        agents_propose: board.policy.agent_adds == AddPolicy::Proposed,
    }
}

/// The boards, the todos and what waits for you (`book`: none until the service sends it).
pub fn overview(book: Option<&TodoBook>, canvas: &Canvas) -> TodosOverview {
    let Some(book) = book else {
        return TodosOverview { loaded: false, boards: Vec::new(), todos: Vec::new(), needs_you: Vec::new(), ready: Vec::new(), claimed: Vec::new(), groups: Vec::new() };
    };
    let mut claimed: Vec<&Todo> = book.todos.iter().filter(|t| !t.status.closed() && t.claim_session().is_some()).collect();
    claimed.sort_by_key(|t| std::cmp::Reverse(t.claimed_by.as_ref().map_or(0, |c| c.ms)));
    TodosOverview {
        loaded: true,
        boards: book.boards.iter().filter(|b| b.parent.is_none()).filter_map(|b| book.board_view(&b.id)).map(|v| board(&v)).collect(),
        todos: book.todos.iter().map(|t| item(book, t, canvas)).collect(),
        needs_you: book.needs_you(|s| canvas.gone(s)).into_iter().map(|n| TodoNeedItem { need: n.need.into(), todo: item(book, n.todo, canvas) }).collect(),
        ready: book.ready(None).into_iter().map(|t| t.id.clone()).collect(),
        claimed: claimed.into_iter().map(|t| t.id.clone()).collect(),
        groups: canvas
            .doc
            .groups
            .iter()
            .filter_map(|g| book.group_counts(&g.id).map(|c| TodoGroupCounts { group: g.id.clone(), open: c.open as u32, in_progress: c.in_progress as u32, proposed: c.proposed as u32 }))
            .collect(),
    }
}

/// Everything about one todo (`None`: not in the book).
pub fn detail(book: &TodoBook, id: &str, canvas: &Canvas) -> Option<TodoDetail> {
    let todo = book.todo(id)?;
    let body_html = if todo.body.trim().is_empty() { String::new() } else { report_html::document(&notebook::markdown_html(&todo.body), &crate::reports::PHONE) };
    Some(TodoDetail {
        item: item(book, todo, canvas),
        body: todo.body.clone(),
        body_html,
        checklist: todo.checklist.iter().map(|c| TodoCheck { text: c.text.clone(), done: c.done }).collect(),
        relations: todo
            .relations
            .iter()
            .map(|r| {
                let other = book.todo(&r.id);
                TodoRelation { kind: r.kind.label().to_owned(), id: r.id.clone(), title: other.map(|o| o.title.clone()).unwrap_or_default(), closed: other.is_some_and(|o| o.status.closed()) }
            })
            .collect(),
        links: todo.links.iter().map(|l| TodoLink { kind: l.kind.word().to_owned(), target: l.target.clone(), title: l.title.clone() }).collect(),
        log: todo.log.iter().rev().map(|e| TodoLogLine { ms: e.ms, by: canvas.name(&e.by), note: e.kind == LogKind::Note, text: e.text.clone() }).collect(),
        resolution: todo.resolution.map(|r| r.word().to_owned()).unwrap_or_default(),
        created_by: canvas.name(&todo.created_by),
        note: todo.note.clone().unwrap_or_default(),
        parent: todo.parent.clone().unwrap_or_default(),
        blocked_text: todo.blocked.clone().unwrap_or_default(),
        actions: actions(todo),
    })
}

/// What you may do with a todo now: the desktop's buttons for it (the book
/// still checks each change).
pub fn actions(todo: &Todo) -> TodoActions {
    let (proposal, closed) = (todo.status == Status::Proposed, todo.status.closed());
    let moves = if proposal || closed { Vec::new() } else { [Status::Backlog, Status::Todo, Status::InProgress, Status::InReview].into_iter().filter(|s| *s != todo.status).map(Into::into).collect() };
    TodoActions { accept: proposal, decline: proposal, done: !proposal && !closed, cancel: !closed, reopen: closed, release: todo.claimed_by.is_some(), moves }
}

/// The operation that adds a drafted todo.
pub fn add_op(draft: TodoDraft) -> TodoOp {
    TodoOp::Add(Box::new(NewTodo {
        board: draft.board.trim().to_owned(),
        title: draft.title,
        kind: draft.kind.into(),
        priority: Some(draft.priority.into()),
        status: draft.status.map(Into::into),
        body: draft.body,
        blocker: draft.blocker,
        ..Default::default()
    }))
}

/// The operation that changes a todo's fields.
pub fn update_op(id: String, edit: TodoEdit) -> TodoOp {
    TodoOp::Update(Box::new(TodoUpdate {
        id,
        title: edit.title,
        kind: edit.kind.map(Into::into),
        priority: edit.priority.map(Into::into),
        // Empty: not blocked any more.
        blocked: edit.blocked.map(|b| b.trim().to_owned()),
        blocker: edit.blocker,
        body: edit.body,
        labels: edit.labels,
        ..Default::default()
    }))
}

/// The bus message a change goes to the service as.
pub fn action_json(op: &TodoOp) -> String {
    todos::client_action(op).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::doc::Node;

    /// The demo book's canvas: its group and the main session (working); the
    /// review session is not on it (its claim is stale).
    fn doc() -> Doc {
        let mut doc = Doc::default();
        doc.groups.push(serde_json::from_value(serde_json::json!({ "id": "g-demo", "name": "Canvas project", "x": 0, "y": 0, "w": 60, "h": 20, "color": 1 })).unwrap());
        doc.nodes.push(serde_json::from_value::<Node>(serde_json::json!({ "id": "demo-main", "kind": "session", "name": "Momentum work", "x": 2, "y": 2 })).unwrap());
        doc
    }

    fn canvas(doc: &Doc) -> Canvas<'_> {
        Canvas { doc, stopped: HashSet::new(), working: ["demo-main".to_owned()].into() }
    }

    #[test]
    fn the_book_becomes_boards_with_lanes_and_cards_with_their_badges() {
        let (book, doc) = (todos::demo_book(), doc());
        let overview = overview(Some(&book), &canvas(&doc));
        assert!(overview.loaded);
        assert_eq!(overview.boards.iter().map(|b| (b.name.as_str(), b.prefix.as_str())).collect::<Vec<_>>(), [("Canvas App", "WI"), ("Decomps", "DEC")], "sub-boards are swimlanes");
        let decomps = &overview.boards[1];
        assert_eq!(decomps.lanes.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["Decomps", "Parser"]);
        assert!(decomps.agents_propose && decomps.columns[0].status == TodoStatus::Proposed, "agents' todos are proposals there");
        assert!(decomps.columns.iter().all(|c| c.cells.len() == 2), "a cell per lane");
        let todo_column = decomps.columns.iter().find(|c| c.status == TodoStatus::Todo).unwrap();
        assert_eq!((todo_column.cells[1].lane.as_str(), todo_column.cells[1].ids.as_slice()), (decomps.lanes[1].id.as_str(), ["DEC-005".to_owned()].as_slice()));
        assert_eq!((decomps.canceled, decomps.blocked), (1, 1));
        let app = &overview.boards[0];
        let progress = app.columns.iter().find(|c| c.status == TodoStatus::InProgress).unwrap();
        assert_eq!((progress.wip, progress.over, app.groups.as_slice()), (2, false, ["g-demo".to_owned()].as_slice()));
        assert!(app.columns.iter().find(|c| c.status == TodoStatus::Done).unwrap().collapsed);

        let card = |id: &str| overview.todos.iter().find(|t| t.id == id).unwrap().clone();
        let momentum = card("WI-002");
        assert_eq!((momentum.kind, momentum.kind_glyph.as_str(), momentum.status, momentum.status_label.as_str()), (TodoKind::Feature, "✚", TodoStatus::InProgress, "In progress"));
        assert_eq!((momentum.priority, momentum.priority_marks.as_str(), momentum.checklist_done, momentum.checklist_total), (TodoPriority::High, "!!", 2, 4));
        assert_eq!((momentum.claimed_by.as_str(), momentum.claimed_session.as_str(), momentum.claim_working), ("Momentum work", "demo-main", true), "named by its box");
        let review = card("WI-003");
        assert_eq!((review.claimed_by.as_str(), review.claimed_session.as_str()), ("Review implementation", ""), "not on this canvas: the name it had");
        assert_eq!(review.prs, ["https://github.com/TomND/copilot-canvas/pull/42"]);
        let flaky = card("WI-005");
        assert!(flaky.blocked && flaky.blocked_reason == "waiting for the new CI runner image" && !flaky.ready);
        let (crash, rename) = (card("DEC-003"), card("DEC-004"));
        assert_eq!((crash.blocker, crash.blocks, crash.priority_marks.as_str()), (true, 1, "!!!"));
        assert_eq!((rename.blocked, rename.blocked_reason.as_str()), (true, "waits for DEC-003"));
        let parse = card("DEC-005");
        assert_eq!((parse.lane.as_str(), parse.board_name.as_str(), parse.root_board.as_str()), ("Parser", "Parser", decomps.id.as_str()));
        let chips = card("WI-006");
        assert!(chips.ai_created && chips.status == TodoStatus::Proposed && chips.proposal_why.starts_with("Sessions would see"));

        // What waits for you: proposals first, then reviews, then the stale claim.
        let needs: Vec<(TodoNeed, &str)> = overview.needs_you.iter().map(|n| (n.need, n.todo.id.as_str())).collect();
        assert_eq!(needs.iter().filter(|(n, _)| *n == TodoNeed::Proposed).count(), 2);
        assert!(needs.contains(&(TodoNeed::InReview, "WI-003")) && needs.contains(&(TodoNeed::StaleClaim, "WI-003")), "{needs:?}");
        assert!(!needs.iter().any(|(_, id)| *id == "WI-002"), "a working session's claim is live");
        assert!(overview.ready.contains(&"WI-004".to_owned()) && !overview.ready.contains(&"WI-002".to_owned()));
        assert_eq!(overview.ready[0], "DEC-003", "urgent first");
        assert!(overview.claimed.contains(&"WI-002".to_owned()) && !overview.claimed.contains(&"DEC-002".to_owned()), "claimed by sessions (you are not one)");
        assert_eq!(overview.groups, [TodoGroupCounts { group: "g-demo".into(), open: 5, in_progress: 1, proposed: 1 }]);
        assert!(!super::overview(None, &canvas(&doc)).loaded, "an older service sends no book");
    }

    #[test]
    fn details_have_the_body_page_checklist_relations_links_and_the_newest_log_first() {
        let (book, doc) = (todos::demo_book(), doc());
        let momentum = detail(&book, "wi-2", &canvas(&doc)).expect("found by a loose id");
        assert_eq!(momentum.item.id, "WI-002");
        assert!(momentum.body_html.contains("<p>A flick keeps the canvas gliding") && momentum.body_html.contains("font:-apple-system-body"), "{}", momentum.body_html);
        assert_eq!(momentum.checklist.iter().map(|c| c.done).collect::<Vec<_>>(), [true, true, false, false]);
        assert!(momentum.log.first().is_some_and(|l| l.note && l.by == "Momentum work" && l.text.starts_with("Glide works")), "{:?}", momentum.log);
        let rename = detail(&book, "DEC-004", &canvas(&doc)).unwrap();
        assert_eq!(rename.relations, [TodoRelation { kind: "blocked by".into(), id: "DEC-003".into(), title: "Crash when a chunk header is truncated".into(), closed: false }]);
        assert!(rename.body_html.is_empty());
        let review = detail(&book, "WI-003", &canvas(&doc)).unwrap();
        assert!(review.links.iter().any(|l| l.kind == "pr" && l.target.ends_with("/pull/42")));
        assert_eq!(detail(&book, "DEC-008", &canvas(&doc)).unwrap().resolution, "obsolete");
        assert!(detail(&book, "NOPE-1", &canvas(&doc)).is_none());
    }

    #[test]
    fn drafts_and_changes_are_the_windows_bus_actions() {
        let draft = TodoDraft { board: " Decomps/Parser ".into(), title: "Fuzz the parser".into(), kind: TodoKind::Research, priority: TodoPriority::NoPriority, status: Some(TodoStatus::Todo), body: String::new(), blocker: true };
        let op = add_op(draft);
        let json: serde_json::Value = serde_json::from_str(&action_json(&op)).unwrap();
        assert_eq!(json["action"], "todos");
        assert_eq!((json["op"]["op"].as_str(), json["op"]["board"].as_str(), json["op"]["type"].as_str(), json["op"]["status"].as_str(), json["op"]["priority"].as_str()), (Some("add"), Some("Decomps/Parser"), Some("research"), Some("todo"), Some("none")));
        let mut book = todos::demo_book();
        let doc = doc();
        assert_eq!(json["op"]["blocker"].as_bool(), Some(true));
        let applied = book.apply(op, &Actor::human(), &canvas(&doc).ctx(1)).unwrap();
        assert_eq!(applied.created.as_deref(), Some("DEC-009"));
        assert!(book.todo("DEC-009").is_some_and(|t| t.blocker && t.board != book.find_board("Decomps").unwrap().id), "in the swimlane");
        let accept: serde_json::Value = serde_json::from_str(&action_json(&TodoOp::Accept { id: "WI-006".into(), status: Some(TodoStatus::Todo.into()) })).unwrap();
        assert_eq!(accept, serde_json::json!({ "action": "todos", "op": { "op": "accept", "id": "WI-006", "status": "todo" } }));
        // Live sessions: on the canvas and not stopped.
        let mut stopped = canvas(&doc);
        assert_eq!(stopped.ctx(1).live, Some(["demo-main".to_owned()].into()));
        stopped.stopped.insert("demo-main".into());
        assert!(stopped.ctx(1).live.is_some_and(|l| l.is_empty()) && stopped.gone("demo-main") && stopped.gone("demo-review"));
        for status in Status::ALL {
            assert_eq!(Status::from(TodoStatus::from(status)), status);
        }
        for kind in TodoType::ALL {
            assert_eq!(TodoType::from(TodoKind::from(kind)), kind);
        }
        for priority in Priority::ALL {
            assert_eq!(Priority::from(TodoPriority::from(priority)), priority);
        }
    }

    #[test]
    fn edits_change_only_the_fields_given_and_actions_follow_the_desktops_buttons() {
        let (mut book, doc) = (todos::demo_book(), doc());
        let ctx = canvas(&doc).ctx(5);
        let edit = TodoEdit { title: Some("Momentum on every drag".into()), priority: Some(TodoPriority::Urgent), labels: Some(vec!["UI".into(), "canvas".into()]), blocked: Some("waiting for design".into()), ..Default::default() };
        let json: serde_json::Value = serde_json::from_str(&action_json(&update_op("WI-002".into(), edit))).unwrap();
        assert_eq!(json["op"], serde_json::json!({ "op": "update", "id": "WI-002", "title": "Momentum on every drag", "priority": "urgent", "blocked": "waiting for design", "labels": ["UI", "canvas"] }), "unset fields are not sent");
        let before = book.todo("WI-002").unwrap().clone();
        book.apply(serde_json::from_value(json["op"].clone()).unwrap(), &Actor::human(), &ctx).unwrap();
        let after = book.todo("WI-002").unwrap();
        assert_eq!((after.title.as_str(), after.priority, after.labels.as_slice(), after.blocked.as_deref()), ("Momentum on every drag", Priority::Urgent, ["ui".to_owned(), "canvas".into()].as_slice(), Some("waiting for design")));
        assert_eq!((after.body.as_str(), after.kind, after.checklist.len()), (before.body.as_str(), before.kind, before.checklist.len()));
        book.apply(update_op("WI-002".into(), TodoEdit { blocked: Some("  ".into()), kind: Some(TodoKind::Bug), ..Default::default() }), &Actor::human(), &ctx).unwrap();
        assert!(book.todo("WI-002").is_some_and(|t| t.blocked.is_none() && t.kind == TodoType::Bug), "an empty reason unblocks");

        // The buttons: a proposal is accepted or declined; open todos move, finish or cancel; closed ones reopen.
        let detail = |book: &TodoBook, id: &str| super::detail(book, id, &canvas(&doc)).unwrap();
        let proposal = detail(&book, "WI-006");
        assert_eq!(proposal.actions, TodoActions { accept: true, decline: true, cancel: true, ..Default::default() });
        let working = detail(&book, "WI-002");
        assert_eq!(working.actions, TodoActions { done: true, cancel: true, release: true, moves: vec![TodoStatus::Backlog, TodoStatus::Todo, TodoStatus::InReview], ..Default::default() });
        let canceled = detail(&book, "DEC-008");
        assert_eq!(canceled.actions, TodoActions { reopen: true, ..Default::default() });
        assert_eq!(detail(&book, "WI-005").blocked_text, "waiting for the new CI runner image");
        assert_eq!(detail(&book, "DEC-004").blocked_text, "", "waiting for another todo is not a reason written");
    }
}
