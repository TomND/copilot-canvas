//! The book for others: JSON Canvas ([`json_canvas`]), the agents' digests
//! ([`group_summary`], [`canvas_summary`]), the demo ([`demo_book`]) and
//! the message a window sends ([`client_action`]).

use serde_json::json;

use super::*;

/// The `ClientMsg::Bus` message a window sends for an operation:
/// `{"action": "todos", "op": {…}}`. The service applies it as you.
pub fn client_action(op: &TodoOp) -> serde_json::Value {
    json!({ "action": "todos", "op": op })
}

/// A card's text in a JSON Canvas export (Markdown).
fn card_text(book: &TodoBook, todo: &Todo, lane: Option<&str>) -> String {
    let mut head = format!("**{}** · {}", todo.id, todo.kind.word());
    if !todo.priority.marks().is_empty() {
        head.push_str(&format!(" · {}", todo.priority.marks()));
    }
    if todo.blocker {
        head.push_str(" · ‼ blocker");
    }
    let mut lines = vec![head, String::new(), format!("**{}**", todo.title)];
    let mut badges = Vec::new();
    if let Some(lane) = lane {
        badges.push(format!("lane: {lane}"));
    }
    if book.is_blocked(todo) {
        let mut why: Vec<String> = todo.blocked.iter().cloned().collect();
        why.extend(book.blockers(todo).iter().map(|b| format!("waits for {}", b.id)));
        badges.push(format!("⚑ blocked: {}", why.join("; ")));
    }
    let (checked, all) = todo.checked();
    if all > 0 {
        badges.push(format!("☑ {checked}/{all}"));
    }
    if let Some(claim) = &todo.claimed_by {
        badges.push(format!("claimed by {}", claim.by.label()));
    }
    for pr in todo.prs() {
        badges.push(format!("PR [{}]({})", if pr.title.is_empty() { &pr.target } else { &pr.title }, pr.target));
    }
    if !badges.is_empty() {
        lines.push(String::new());
        lines.extend(badges);
    }
    lines.join("\n")
}

/// A board (or every top-level board) as a JSON Canvas 1.0 document: a
/// group node per board and per column, a text node per todo (sub-boards'
/// todos say their lane), a `file` node under each todo with a note, and
/// edges for the relations between exported todos. Canceled todos are left out.
pub fn json_canvas(book: &TodoBook, board: Option<&str>) -> Result<serde_json::Value, String> {
    const CARD_W: i64 = 280;
    const CARD_H: i64 = 140;
    const NOTE_H: i64 = 60;
    const GAP: i64 = 20;
    const HEAD: i64 = 60;
    let roots: Vec<&Board> = match board.map(str::trim).filter(|b| !b.is_empty()) {
        Some(text) => vec![book.find_board(text).and_then(|b| book.root(&b.id)).ok_or_else(|| format!("no board {text:?}"))?],
        None => book.boards.iter().filter(|b| b.parent.is_none()).collect(),
    };
    let (mut groups, mut cards, mut edges) = (Vec::new(), Vec::new(), Vec::new());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut top = 0i64;
    for root in roots {
        let Some(view) = book.board_view(&root.id) else { continue };
        let column_w = CARD_W + 2 * GAP;
        let mut height = CARD_H + 2 * GAP;
        let mut columns = Vec::new();
        for (c, column) in view.columns.iter().enumerate() {
            let x = GAP + c as i64 * (column_w + GAP);
            let mut y = top + HEAD + 2 * GAP;
            for (lane, cell) in column.cells.iter().enumerate() {
                for todo in cell {
                    let lane_name = (lane > 0).then(|| view.lanes[lane].name.as_str());
                    let mut card = json!({ "id": format!("todo-{}", todo.id), "type": "text", "text": card_text(book, todo, lane_name), "x": x + GAP, "y": y, "width": CARD_W, "height": CARD_H });
                    if todo.kind == TodoType::Bug {
                        card["color"] = "1".into();
                    }
                    cards.push(card);
                    y += CARD_H + GAP;
                    if let Some(note) = &todo.note {
                        cards.push(json!({ "id": format!("note-{}", todo.id), "type": "file", "file": note, "x": x + GAP, "y": y - GAP / 2, "width": CARD_W, "height": NOTE_H }));
                        edges.push(json!({ "id": format!("edge-{}-note", todo.id), "fromNode": format!("todo-{}", todo.id), "fromSide": "bottom", "toNode": format!("note-{}", todo.id), "toSide": "top" }));
                        y += NOTE_H + GAP;
                    }
                    placed.insert(todo.id.clone());
                }
            }
            let column_h = (y - top - HEAD).max(CARD_H + 2 * GAP);
            height = height.max(column_h);
            columns.push((format!("column-{}-{}", root.id, column.status.word()), format!("{} ({})", column.name, column.count), x));
        }
        let width = GAP + view.columns.len() as i64 * (column_w + GAP);
        let mut frame = json!({ "id": format!("board-{}", root.id), "type": "group", "label": format!("{} ({})", root.name, root.prefix), "x": 0, "y": top, "width": width, "height": HEAD + height + GAP });
        if (1..=6).contains(&root.color) {
            frame["color"] = root.color.to_string().into();
        }
        groups.push(frame);
        for (id, label, x) in columns {
            groups.push(json!({ "id": id, "type": "group", "label": label, "x": x, "y": top + HEAD, "width": column_w, "height": height }));
        }
        top += HEAD + height + 4 * GAP;
    }
    for todo in book.todos.iter().filter(|t| placed.contains(&t.id)) {
        for relation in todo.relations.iter().filter(|r| placed.contains(&r.id)) {
            edges.push(json!({ "id": format!("rel-{}-{}-{}", todo.id, relation.kind.word(), relation.id), "fromNode": format!("todo-{}", todo.id), "toNode": format!("todo-{}", relation.id), "label": relation.kind.label() }));
        }
    }
    groups.extend(cards);
    Ok(json!({ "nodes": groups, "edges": edges }))
}

/// One todo as a digest line: `- DEC-012 [in progress, claimed by X] (bug, high) Title ‼ ⚑ blocked: …`.
fn digest_line(book: &TodoBook, todo: &Todo) -> String {
    let mut state = vec![todo.status.label().to_lowercase()];
    if let Some(claim) = &todo.claimed_by {
        state.push(format!("claimed by {}", claim.by.label()));
    }
    if todo.status == Status::Todo && book.is_ready(todo) {
        state.push("ready".into());
    }
    let mut kind = vec![todo.kind.word().to_owned()];
    if todo.priority != Priority::None {
        kind.push(todo.priority.word().to_owned());
    }
    let mut line = format!("- {} [{}] ({}) {}", todo.id, state.join(", "), kind.join(", "), todo.title);
    if todo.blocker {
        line.push_str(" ‼");
    }
    if book.is_blocked(todo) {
        let mut why: Vec<String> = todo.blocked.iter().cloned().collect();
        why.extend(book.blockers(todo).iter().map(|b| format!("waits for {}", b.id)));
        line.push_str(&format!(" ⚑ blocked: {}", why.join("; ")));
    }
    line
}

fn counts_line(todos: &[&Todo]) -> String {
    let count = |s: Status| todos.iter().filter(|t| t.status == s).count();
    let open = todos.iter().filter(|t| t.status.open()).count();
    format!(
        "{open} open ({} backlog, {} todo, {} in progress, {} in review) · {} proposed · {} done",
        count(Status::Backlog),
        count(Status::Todo),
        count(Status::InProgress),
        count(Status::InReview),
        count(Status::Proposed),
        count(Status::Done)
    )
}

/// What a group's sessions should know of its boards (the `# Todos`
/// section of `group-context.md`): each linked board's counts and rules,
/// and up to 20 open todos (in progress first). `None`: no board is linked.
pub fn group_summary(book: &TodoBook, group: &str) -> Option<String> {
    let linked = book.boards_of_group(group);
    if linked.is_empty() {
        return None;
    }
    let ids: BTreeSet<&str> = linked.iter().map(|b| b.id.as_str()).collect();
    let mut out = String::new();
    let mut left = 20usize;
    for board in linked.iter().filter(|b| b.parent.as_deref().is_none_or(|p| !ids.contains(p))) {
        let lanes = book.lane_ids(&board.id);
        let todos: Vec<&Todo> = book.todos.iter().filter(|t| lanes.contains(&t.board)).collect();
        let subs: Vec<String> = book.sub_boards(&board.id).iter().map(|b| b.name.clone()).collect();
        out.push_str(&format!("\n## {} ({}){}\n\n", board.name, board.prefix, if subs.is_empty() { String::new() } else { format!(" · swimlanes: {}", subs.join(", ")) }));
        out.push_str(&format!("{}{}\n", counts_line(&todos), board.wip.map(|w| format!(" · WIP limit {w}")).unwrap_or_default()));
        let policy = book.root(&board.id).map(|r| r.policy).unwrap_or_default();
        out.push_str(&format!(
            "Agents: {}; {}.\n",
            if policy.agent_adds == AddPolicy::Proposed { "todos you add become proposals" } else { "add todos the user asked for, propose your own ideas" },
            if policy.agents_may_close { "you may close todos" } else { "done and canceled are the user's (submit for review instead)" }
        ));
        let order = |s: Status| match s {
            Status::InProgress => 0,
            Status::InReview => 1,
            Status::Todo => 2,
            _ => 3,
        };
        let mut open: Vec<&Todo> = todos.iter().copied().filter(|t| t.status.open()).collect();
        open.sort_by(|a, b| order(a.status).cmp(&order(b.status)).then(a.priority.cmp(&b.priority)).then_with(|| by_rank(a, b)));
        let shown = open.len().min(left);
        if shown > 0 {
            out.push('\n');
        }
        for todo in &open[..shown] {
            out.push_str(&digest_line(book, todo));
            out.push('\n');
        }
        if open.len() > shown {
            out.push_str(&format!("- … and {} more (copilot-canvas todo list --board \"{}\")\n", open.len() - shown, board.name));
        }
        left -= shown;
    }
    Some(out.trim_start().to_owned())
}

/// Every board in a few lines (the todo part of `canvas-context.md`);
/// `groups`: the canvas's group names by id.
pub fn canvas_summary(book: &TodoBook, groups: &BTreeMap<String, String>) -> String {
    if book.boards.is_empty() {
        return "No todo boards yet.\n".into();
    }
    let mut out = String::new();
    for board in book.boards.iter().filter(|b| b.parent.is_none()) {
        let todos: Vec<&Todo> = book.todos_in(&board.id).collect();
        let subs: Vec<String> = book.sub_boards(&board.id).iter().map(|b| b.name.clone()).collect();
        let mut linked: Vec<String> = book.lane_ids(&board.id).iter().filter_map(|id| book.board(id)).flat_map(|b| b.canvas_groups.iter()).map(|g| groups.get(g).cloned().unwrap_or_else(|| g.clone())).collect();
        linked.dedup();
        let blocked = todos.iter().filter(|t| !t.status.closed() && book.is_blocked(t)).count();
        out.push_str(&format!(
            "- {} ({}){} · {} · {} · {} blocked, {} ready\n",
            board.name,
            board.prefix,
            if subs.is_empty() { String::new() } else { format!(", swimlanes: {}", subs.join(", ")) },
            if linked.is_empty() { "no canvas group".to_owned() } else { format!("groups: {}", linked.join(", ")) },
            counts_line(&todos),
            blocked,
            book.ready(Some(&board.id)).len()
        ));
    }
    let (proposed, review) = (book.todos.iter().filter(|t| t.status == Status::Proposed).count(), book.todos.iter().filter(|t| t.status == Status::InReview).count());
    if proposed + review > 0 {
        out.push_str(&format!("Waiting for the user: {proposed} proposal(s), {review} todo(s) in review.\n"));
    }
    out
}
/// A small book for demo mode and view snapshots: the boards "Decomps"
/// (DEC, a "Parser" swimlane, agents' todos become proposals) and "Canvas
/// App" (WI, linked to the demo group `g-demo`, WIP 2), with todos in every
/// status, proposals, a blocker, blocked todos, checklists, links and
/// claims by the demo sessions (`demo-main`, `demo-review`).
pub fn demo_book() -> TodoBook {
    const DAY: u64 = 86_400_000;
    // 2026-10-26 09:00 UTC.
    let start: u64 = 1_792_998_000_000;
    let mut book = TodoBook::default();
    let you = Actor::human();
    let main = Actor::agent("demo-main", "Build session canvas");
    let review = Actor::agent("demo-review", "Review implementation");
    let mut step = 0u64;
    let mut run = |book: &mut TodoBook, op: TodoOp, actor: &Actor| {
        step += 1;
        let ctx = Ctx { now: start + step * DAY / 8, group: Some("g-demo".into()), groups: [("g-demo".to_owned(), "Canvas project".to_owned())].into(), ..Default::default() };
        let result = book.apply(op, actor, &ctx);
        debug_assert!(result.is_ok(), "demo book: {result:?}");
    };
    let board = |name: &str, prefix: Option<&str>, parent: Option<&str>, groups: &[&str], x: i32| TodoOp::AddBoard {
        name: name.into(),
        prefix: prefix.map(str::to_owned),
        parent: parent.map(str::to_owned),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        x: Some(x),
        y: Some(0),
    };
    run(&mut book, board("Canvas App", Some("WI"), None, &["g-demo"], 0), &you);
    run(&mut book, board("Decomps", Some("DEC"), None, &[], BOARD_W + 6), &you);
    run(&mut book, board("Parser", None, Some("Decomps"), &[], 0), &you);
    let proposals = AgentPolicy { agent_adds: AddPolicy::Proposed, agents_may_close: false };
    run(&mut book, TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: None, color: Some(4), notes: None, policy: Some(proposals), statuses: None }, &you);
    run(&mut book, TodoOp::UpdateBoard { board: "Canvas App".into(), name: None, prefix: None, wip: Some(2), color: Some(5), notes: None, policy: None, statuses: None }, &you);
    let add = |board: &str, title: &str, kind: TodoType, priority: Priority, status: Status| {
        TodoOp::Add(Box::new(NewTodo { board: board.into(), title: title.into(), kind, priority: Some(priority), status: Some(status), ..Default::default() }))
    };
    // Canvas App.
    run(&mut book, add("Canvas App", "Easier group selection", TodoType::Improvement, Priority::Medium, Status::Todo), &you);
    run(&mut book, TodoOp::Done { id: "WI-001".into(), note: Some("Shipped in da03e6f.".into()) }, &you);
    run(&mut book, TodoOp::Add(Box::new(NewTodo { board: "Canvas App".into(), title: "Gentle momentum when panning".into(), kind: TodoType::Feature, priority: Some(Priority::High), status: Some(Status::Todo), body: "A flick keeps the canvas gliding and slows down gently.".into(), checklist: vec!["Velocity from the last 60 ms".into(), "Friction curve".into(), "Stop on click".into(), "Reduced motion: no glide".into()], ..Default::default() })), &you);
    run(&mut book, TodoOp::Claim { id: "WI-002".into(), force: false, session: None }, &main);
    run(&mut book, TodoOp::Check { id: "WI-002".into(), index: 0, done: true }, &main);
    run(&mut book, TodoOp::Check { id: "WI-002".into(), index: 1, done: true }, &main);
    run(&mut book, TodoOp::Note { id: "WI-002".into(), text: "Glide works; tuning the friction next.".into() }, &main);
    run(&mut book, add("Canvas App", "Todo boards in the Tasks space", TodoType::Feature, Priority::Urgent, Status::Todo), &you);
    run(&mut book, TodoOp::Claim { id: "WI-003".into(), force: false, session: None }, &review);
    run(&mut book, TodoOp::Submit { id: "WI-003".into(), pr: Some("https://github.com/TomND/copilot-canvas/pull/42".into()), summary: Some("Boards, columns, swimlanes and details.".into()) }, &review);
    run(&mut book, add("Canvas App", "Docs for the Tasks space", TodoType::Docs, Priority::Medium, Status::Todo), &you);
    run(&mut book, add("Canvas App", "Flaky reconnect test", TodoType::Bug, Priority::High, Status::Todo), &you);
    run(&mut book, TodoOp::Update(Box::new(TodoUpdate { id: "WI-005".into(), blocked: Some("waiting for the new CI runner image".into()), ..Default::default() })), &you);
    run(&mut book, TodoOp::Add(Box::new(NewTodo { board: "Canvas App".into(), title: "Show todo counts on group chips".into(), kind: TodoType::Improvement, propose: true, why: Some("Sessions would see their group's open work at a glance.".into()), ..Default::default() })), &main);
    run(&mut book, add("Canvas App", "Keyboard shortcuts cheat sheet", TodoType::Docs, Priority::Low, Status::Backlog), &you);
    // Decomps.
    run(&mut book, add("Decomps", "Decompile the save-game loader", TodoType::Feature, Priority::High, Status::Todo), &you);
    run(&mut book, TodoOp::Done { id: "DEC-001".into(), note: None }, &you);
    run(&mut book, add("Decomps", "Map the inventory structs", TodoType::Research, Priority::Medium, Status::Todo), &you);
    run(&mut book, TodoOp::Claim { id: "DEC-002".into(), force: false, session: None }, &you);
    run(&mut book, TodoOp::Add(Box::new(NewTodo { board: "Decomps".into(), title: "Crash when a chunk header is truncated".into(), kind: TodoType::Bug, priority: Some(Priority::Urgent), status: Some(Status::Todo), blocker: true, ..Default::default() })), &you);
    run(&mut book, add("Decomps", "Rename the recovered functions", TodoType::Chore, Priority::None, Status::Backlog), &you);
    run(&mut book, TodoOp::Relate { id: "DEC-004".into(), kind: RelKind::BlockedBy, other: "DEC-003".into() }, &you);
    run(&mut book, TodoOp::Add(Box::new(NewTodo { board: "Decomps/Parser".into(), title: "Parse the compressed string table".into(), kind: TodoType::Feature, priority: Some(Priority::High), status: Some(Status::Todo), checklist: vec!["Header".into(), "Huffman table".into(), "Tests".into()], ..Default::default() })), &you);
    run(&mut book, TodoOp::Check { id: "DEC-005".into(), index: 0, done: true }, &you);
    run(&mut book, add("Decomps/Parser", "Faster symbol lookup", TodoType::Optimization, Priority::Low, Status::Backlog), &you);
    run(&mut book, TodoOp::Add(Box::new(NewTodo { board: "Decomps".into(), title: "Cache decompiled functions between runs".into(), kind: TodoType::Optimization, propose: true, why: Some("Every run decompiles the same 4,000 functions again.".into()), discovered_from: Some("DEC-002".into()), ..Default::default() })), &main);
    run(&mut book, add("Decomps", "Port the old Python scripts", TodoType::Chore, Priority::None, Status::Backlog), &you);
    run(&mut book, TodoOp::Cancel { id: "DEC-008".into(), reason: Some("The new pipeline replaces them.".into()), resolution: Some(Resolution::Obsolete) }, &you);
    book
}
