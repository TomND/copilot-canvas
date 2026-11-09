//! Tests of the todo model: ids, ranks, operations and their rules, views,
//! import, mirror, export and summaries.

use super::*;

const NOW: u64 = 1_793_000_000_000;

fn you() -> Actor {
    Actor::human()
}

fn agent(session: &str) -> Actor {
    Actor::agent(session, &format!("Session {session}"))
}

/// A context for an actor in the group `g1` ("Pay"), every session live.
fn ctx() -> Ctx {
    Ctx { now: NOW, group: Some("g1".into()), groups: [("g1".to_owned(), "Pay".to_owned()), ("g2".to_owned(), "Docs".to_owned())].into(), ..Default::default() }
}

fn add_board(book: &mut TodoBook, name: &str, prefix: Option<&str>, parent: Option<&str>, groups: &[&str]) -> Applied {
    let op = TodoOp::AddBoard { name: name.into(), prefix: prefix.map(str::to_owned), parent: parent.map(str::to_owned), groups: groups.iter().map(|g| g.to_string()).collect(), x: None, y: None };
    book.apply(op, &you(), &ctx()).unwrap()
}

fn new(board: &str, title: &str) -> NewTodo {
    NewTodo { board: board.into(), title: title.into(), ..Default::default() }
}

fn add(book: &mut TodoBook, todo: NewTodo, actor: &Actor) -> Result<Applied, String> {
    book.apply(TodoOp::Add(Box::new(todo)), actor, &ctx())
}

/// "Decomps" (DEC, linked to g1) with a "Parser" swimlane.
fn book() -> TodoBook {
    let mut book = TodoBook::default();
    add_board(&mut book, "Decomps", None, None, &["g1"]);
    add_board(&mut book, "Parser", None, Some("Decomps"), &[]);
    book
}

fn status(book: &TodoBook, id: &str) -> Status {
    book.todo(id).unwrap().status
}

#[test]
fn ids_are_prefixed_padded_and_never_reused() {
    assert_eq!(parse_id("DEC-031"), Some(("DEC".into(), 31)));
    assert_eq!(parse_id("dec-31"), Some(("DEC".into(), 31)));
    assert_eq!(parse_id("D-1"), None, "prefixes have 2 to 5 letters");
    assert_eq!(parse_id("DECOMP-1"), None);
    assert_eq!(parse_id("DEC-x1"), None);
    assert_eq!(format_id("WI", 7), "WI-007");
    assert_eq!(format_id("WI", 1234), "WI-1234");
    assert!(same_id("wi-7", "WI-007") && !same_id("WI-7", "WI-070"));
    let mut book = book();
    for title in ["One", "Two", "Three"] {
        add(&mut book, new("Decomps", title), &you()).unwrap();
    }
    assert_eq!(book.todos.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["DEC-001", "DEC-002", "DEC-003"]);
    assert_eq!(book.todo("dec-2").unwrap().title, "Two", "found case-insensitively and without padding");
    book.apply(TodoOp::Delete { id: "DEC-003".into() }, &you(), &ctx()).unwrap();
    let made = add(&mut book, new("Parser", "Four"), &you()).unwrap();
    assert_eq!(made.created.as_deref(), Some("DEC-004"), "a sub-board shares its parent's prefix; deleted ids stay used");
    assert_eq!(book.seq["DEC"], 4);
}

#[test]
fn prefixes_come_from_the_name_and_stay_unique() {
    let mut book = TodoBook::default();
    assert_eq!(book.suggest_prefix("Decomps"), "DEC");
    assert_eq!(book.suggest_prefix("Canvas App"), "CA");
    assert_eq!(book.suggest_prefix("Über ✓"), "BER");
    assert_eq!(book.suggest_prefix("x"), "XTA", "too short: padded, then lettered");
    add_board(&mut book, "Decomps", None, None, &[]);
    assert_eq!(book.boards[0].prefix, "DEC");
    assert_eq!(book.suggest_prefix("Decoder"), "DECO", "DEC is taken");
    let taken = book.apply(TodoOp::AddBoard { name: "Other".into(), prefix: Some("dec".into()), parent: None, groups: vec![], x: None, y: None }, &you(), &ctx());
    assert!(taken.unwrap_err().contains("belongs to the board"));
    let bad = book.apply(TodoOp::AddBoard { name: "Other".into(), prefix: Some("D1".into()), parent: None, groups: vec![], x: None, y: None }, &you(), &ctx());
    assert!(bad.unwrap_err().contains("2 to 5 letters"));
    let sub = book.apply(TodoOp::AddBoard { name: "Lane".into(), prefix: Some("LAN".into()), parent: Some("Decomps".into()), groups: vec![], x: None, y: None }, &you(), &ctx());
    assert!(sub.unwrap_err().contains("parent's prefix"));
    add_board(&mut book, "Lane", None, Some("DEC"), &[]);
    let nested = book.apply(TodoOp::AddBoard { name: "Deeper".into(), prefix: None, parent: Some("Decomps/Lane".into()), groups: vec![], x: None, y: None }, &you(), &ctx());
    assert!(nested.unwrap_err().contains("swimlanes of a top-level board"));
    assert_eq!(book.find_board("decomps/lane").map(|b| b.id.as_str()), Some("decomps-lane"));
    assert_eq!(book.find_board("dec").map(|b| b.id.as_str()), Some("decomps"), "by prefix");
    assert!(book.apply(TodoOp::UpdateBoard { board: "Lane".into(), name: None, prefix: Some("LN".into()), wip: None, color: None, notes: None, policy: None, statuses: None }, &you(), &ctx()).is_err());
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: Some("DC".into()), wip: None, color: None, notes: None, policy: None, statuses: None }, &you(), &ctx()).unwrap();
    assert!(book.boards.iter().all(|b| b.prefix == "DC"), "sub-boards follow their parent");
}

#[test]
fn ranks_order_any_insertion() {
    assert_eq!(rank_between(None, None), "V");
    let mut column = vec![rank_between(None, None)];
    // 300 appends, 300 inserts at the front, 300 in the middle.
    for _ in 0..300 {
        column.push(rank_between(column.last().map(String::as_str), None));
        column.insert(0, rank_between(None, column.first().map(String::as_str)));
        let mid = column.len() / 2;
        let rank = rank_between(Some(&column[mid - 1]), Some(&column[mid]));
        column.insert(mid, rank);
    }
    assert!(column.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
    assert!(column.iter().all(|r| valid_rank(r)));
    assert!(column.iter().map(String::len).max().unwrap() < 120);
    assert!(column.last().unwrap().len() < 16, "appends grow slowly: {}", column.last().unwrap());
    assert!(column.first().unwrap().len() < 16, "so do inserts at the front: {}", column.first().unwrap());
    assert_eq!((rank_between(Some("V"), None), rank_between(None, Some("V")), rank_between(Some("z"), None), rank_between(None, Some("1"))), ("W".into(), "U".into(), "zV".into(), "0V".into()));
    // Bounds that make no sense are ignored, never an invalid rank.
    assert!(rank_between(Some("b"), Some("a")).as_str() > "b");
    assert!(valid_rank(&rank_between(Some("a0"), Some("!"))));
    let spread = rank_sequence(1_000);
    assert!(spread.windows(2).all(|w| w[0] < w[1]) && spread.iter().all(|r| valid_rank(r) && r.len() <= 2));
    assert_eq!(rank_sequence(0), Vec::<String>::new());
}

#[test]
fn people_add_anywhere_agents_into_backlog_or_todo() {
    let mut book = book();
    let made = add(&mut book, NewTodo { kind: TodoType::Bug, priority: Some(Priority::High), labels: vec!["Save Games".into(), "save-games".into()], checklist: vec!["repro".into()], ..new("Decomps", "  Crash   on load ") }, &you()).unwrap();
    let todo = book.todo("DEC-001").unwrap();
    assert_eq!((todo.title.as_str(), todo.status, todo.kind, todo.priority), ("Crash on load", Status::Backlog, TodoType::Bug, Priority::High));
    assert_eq!(todo.labels, ["save-games"]);
    assert_eq!(todo.checked(), (0, 1));
    assert_eq!(made.events[0].kind, "todo.created");
    assert_eq!(made.events[0].data["board_name"], "Decomps");
    // Agents: backlog or todo; their board is the first linked to their group.
    let made = add(&mut book, NewTodo { status: Some(Status::Todo), ..new("", "Map structs") }, &agent("s1")).unwrap();
    assert_eq!((made.created.as_deref(), status(&book, "DEC-002")), (Some("DEC-002"), Status::Todo));
    assert!(book.todo("DEC-002").unwrap().ai_created());
    let refused = add(&mut book, NewTodo { status: Some(Status::InProgress), ..new("Decomps", "x") }, &agent("s1")).unwrap_err();
    assert!(refused.contains("backlog or todo"), "{refused}");
    assert!(add(&mut book, NewTodo { status: Some(Status::Done), ..new("Decomps", "x") }, &you()).unwrap_err().contains("cannot start done"));
    assert!(add(&mut book, new("Decomps", "   "), &you()).unwrap_err().contains("needs a title"));
    assert!(add(&mut book, new("Nope", "x"), &you()).unwrap_err().contains("no board \"Nope\" (boards: Decomps, Parser)"));
    let lonely = Ctx { group: Some("g2".into()), ..ctx() };
    assert!(book.apply(TodoOp::Add(Box::new(new("", "x"))), &agent("s2"), &lonely).unwrap_err().contains("no todo board is linked to your canvas group"));
    // Proposals need a reason from agents; a board's policy turns agents' todos into proposals.
    assert!(add(&mut book, NewTodo { propose: true, ..new("Decomps", "Idea") }, &agent("s1")).unwrap_err().contains("--why"));
    let made = add(&mut book, NewTodo { propose: true, why: Some("It saves a day per release.".into()), discovered_from: Some("dec-1".into()), ..new("Decomps", "Idea") }, &agent("s1")).unwrap();
    let idea = book.todo("DEC-003").unwrap();
    assert_eq!((idea.status, idea.proposal.as_ref().unwrap().why.as_str()), (Status::Proposed, "It saves a day per release."));
    assert_eq!(idea.relations, [Relation { kind: RelKind::DiscoveredFrom, id: "DEC-001".into() }]);
    assert_eq!(made.events[0].kind, "todo.proposed");
    let policy = AgentPolicy { agent_adds: AddPolicy::Proposed, agents_may_close: false };
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: None, color: None, notes: None, policy: Some(policy), statuses: None }, &you(), &ctx()).unwrap();
    add(&mut book, new("Parser", "Asked for"), &agent("s1")).unwrap();
    assert_eq!(status(&book, "DEC-004"), Status::Proposed, "the parent board's policy covers its swimlanes");
    add(&mut book, new("Parser", "By you"), &you()).unwrap();
    assert_eq!(status(&book, "DEC-005"), Status::Backlog, "people are not proposals");
}

#[test]
fn agents_never_accept_decline_delete_or_close_unless_the_board_lets_them_close() {
    let mut book = book();
    add(&mut book, new("Decomps", "Work"), &you()).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("why".into()), ..new("Decomps", "Idea") }, &agent("s1")).unwrap();
    let bot = agent("s1");
    let refusals = [
        TodoOp::Accept { id: "DEC-002".into(), status: None },
        TodoOp::Decline { id: "DEC-002".into(), reason: None },
        TodoOp::Delete { id: "DEC-001".into() },
        TodoOp::Done { id: "DEC-001".into(), note: None },
        TodoOp::Cancel { id: "DEC-001".into(), reason: None, resolution: None },
        TodoOp::Move { id: "DEC-001".into(), status: Some(Status::Done), board: None, after: None, before: None, rank: None },
        TodoOp::Move { id: "DEC-002".into(), status: Some(Status::Backlog), board: None, after: None, before: None, rank: None },
        TodoOp::Claim { id: "DEC-002".into(), force: false, session: None },
        TodoOp::UpdateBoard { board: "Decomps".into(), name: Some("Mine".into()), prefix: None, wip: None, color: None, notes: None, policy: None, statuses: None },
        TodoOp::MoveBoard { board: "Decomps".into(), x: 1, y: 1, w: None, h: None },
        TodoOp::DeleteBoard { board: "Parser".into(), with_todos: false },
        TodoOp::LinkBoard { board: "Decomps".into(), group: "g2".into(), unlink: false },
        TodoOp::LinkBoard { board: "Decomps".into(), group: "g1".into(), unlink: true },
        TodoOp::SetMirror { path: Some(r"C:\n\TODOs.md".into()) },
        TodoOp::Import { board: "Decomps".into(), prefix: None, text: "- [ ] **DEC-9: x**".into() },
    ];
    let before = book.clone();
    for op in refusals {
        let word = op.word();
        assert!(book.apply(op, &bot, &ctx()).is_err(), "agents may not {word}");
    }
    assert_eq!(book, before, "refused operations change nothing");
    // What agents may do.
    for op in [
        TodoOp::Move { id: "DEC-001".into(), status: Some(Status::Todo), board: None, after: None, before: None, rank: None },
        TodoOp::Update(Box::new(TodoUpdate { id: "DEC-001".into(), priority: Some(Priority::Low), add_labels: vec!["ui".into()], ..Default::default() })),
        TodoOp::Note { id: "DEC-001".into(), text: "Looked at it.".into() },
        TodoOp::AddCheck { id: "DEC-001".into(), text: "step".into() },
        TodoOp::Check { id: "DEC-001".into(), index: 0, done: true },
        TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Url, target: "https://example.com/spec".into(), title: None },
        TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::Related, other: "DEC-002".into() },
        TodoOp::LinkBoard { board: "Parser".into(), group: "g1".into(), unlink: false },
        TodoOp::Move { id: "DEC-001".into(), status: Some(Status::InReview), board: Some("Parser".into()), after: None, before: None, rank: None },
    ] {
        let word = op.word();
        book.apply(op, &bot, &ctx()).unwrap_or_else(|e| panic!("agents may {word}: {e}"));
    }
    // A board that lets agents close.
    let policy = AgentPolicy { agent_adds: AddPolicy::Direct, agents_may_close: true };
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: None, color: None, notes: None, policy: Some(policy), statuses: None }, &you(), &ctx()).unwrap();
    book.apply(TodoOp::Done { id: "DEC-001".into(), note: None }, &bot, &ctx()).unwrap();
    book.apply(TodoOp::Reopen { id: "DEC-001".into() }, &bot, &ctx()).unwrap();
    book.apply(TodoOp::Cancel { id: "DEC-001".into(), reason: Some("dup".into()), resolution: Some(Resolution::Duplicate) }, &bot, &ctx()).unwrap();
    assert!(book.apply(TodoOp::Accept { id: "DEC-002".into(), status: None }, &bot, &ctx()).is_err(), "accepting stays the user's");
    assert!(book.apply(TodoOp::Cancel { id: "DEC-002".into(), reason: None, resolution: None }, &bot, &ctx()).is_err(), "so does declining");
    // An agent's new board is linked to its own group, whatever it asks.
    let made = book.apply(TodoOp::AddBoard { name: "Agent board".into(), prefix: None, parent: None, groups: vec!["g2".into()], x: None, y: None }, &bot, &ctx()).unwrap();
    assert_eq!(book.board(made.created.as_deref().unwrap()).unwrap().canvas_groups, ["g1"]);
}
#[test]
fn claims_belong_to_one_live_session_unless_the_user_forces_them() {
    let mut book = book();
    for title in ["A", "B"] {
        add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", title) }, &you()).unwrap();
    }
    let claim = |id: &str, force: bool| TodoOp::Claim { id: id.into(), force, session: None };
    let made = book.apply(claim("DEC-001", false), &agent("s1"), &ctx()).unwrap();
    assert_eq!(status(&book, "DEC-001"), Status::InProgress);
    assert_eq!((made.events[0].kind.as_str(), made.events[0].data["from"].as_str(), made.events[0].data["by_session"].as_str()), ("todo.claimed", Some("todo"), Some("s1")));
    assert_eq!(book.claimed_by_session("s1").map(|t| t.id.as_str()), Some("DEC-001"));
    assert!(book.apply(claim("DEC-001", false), &agent("s1"), &ctx()).unwrap().message.contains("yours already"));
    assert!(book.apply(claim("DEC-001", false), &agent("s2"), &ctx()).unwrap_err().contains("claimed by Session s1"));
    assert!(book.apply(claim("DEC-001", true), &agent("s2"), &ctx()).unwrap_err().contains("only the user can force"));
    assert!(book.apply(claim("DEC-001", false), &you(), &ctx()).unwrap_err().contains("force the claim"));
    assert!(book.apply(TodoOp::Release { id: "DEC-001".into() }, &agent("s2"), &ctx()).unwrap_err().contains("not you"));
    // s1 is gone: its claim is stale and anyone takes it.
    let gone = Ctx { live: Some(["s2".to_owned()].into()), ..ctx() };
    let made = book.apply(claim("DEC-001", false), &agent("s2"), &gone).unwrap();
    assert!(made.warnings[0].contains("gone or stopped"), "{:?}", made.warnings);
    assert_eq!(book.todo("DEC-001").unwrap().claim_session(), Some("s2"));
    assert_eq!(book.claimed_by_session("s1"), None);
    // The user forces a live claim.
    let made = book.apply(claim("DEC-001", true), &you(), &ctx()).unwrap();
    assert!(made.warnings[0].contains("took DEC-001 over"));
    assert!(book.apply(TodoOp::Claim { id: "DEC-001".into(), force: false, session: None }, &agent("s2"), &ctx()).unwrap_err().contains("claimed by you"), "people's claims are always live");
    let made = book.apply(TodoOp::Release { id: "DEC-001".into() }, &you(), &ctx()).unwrap();
    assert_eq!((status(&book, "DEC-001"), made.events[0].data["to"].as_str()), (Status::Todo, Some("todo")), "released: ready again");
    assert!(book.apply(TodoOp::Release { id: "DEC-001".into() }, &you(), &ctx()).unwrap_err().contains("not claimed"));
    // Soft limits: a warning, never a refusal.
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: Some(1), color: None, notes: None, policy: None, statuses: None }, &you(), &ctx()).unwrap();
    book.apply(claim("DEC-001", false), &agent("s1"), &ctx()).unwrap();
    let made = book.apply(claim("DEC-002", false), &agent("s2"), &ctx()).unwrap();
    assert!(made.warnings.iter().any(|w| w.contains("over its WIP limit of 1")), "{:?}", made.warnings);
    assert!(book.board_view("Decomps").is_none(), "views take ids");
    let view = book.board_view("decomps").unwrap();
    let column = view.columns.iter().find(|c| c.status == Status::InProgress).unwrap();
    assert_eq!((column.count, column.wip, column.over), (2, Some(1), true));
    // Done and canceled todos cannot be claimed; closing releases the claim.
    book.apply(TodoOp::Done { id: "DEC-002".into(), note: None }, &you(), &ctx()).unwrap();
    assert!(book.todo("DEC-002").unwrap().claimed_by.is_none());
    assert!(book.apply(claim("DEC-002", false), &agent("s2"), &ctx()).unwrap_err().contains("reopening"));
}

#[test]
fn people_claim_todos_for_a_session_and_agents_only_for_themselves() {
    let mut book = book();
    for title in ["A", "B"] {
        add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", title) }, &you()).unwrap();
    }
    let sessions: std::collections::BTreeMap<String, String> = [("s1".to_owned(), "Fix flaky test".to_owned()), ("s2".to_owned(), "Write docs".to_owned())].into();
    let known = Ctx { sessions: sessions.clone(), ..ctx() };
    let claim_for = |id: &str, session: &str, force: bool| TodoOp::Claim { id: id.into(), force, session: Some(session.into()) };
    // You assign DEC-001 to s1: the claim is s1's (named as the canvas names it), the log and event say who did it.
    let made = book.apply(claim_for("DEC-001", "s1", false), &you(), &known).unwrap();
    let todo = book.todo("DEC-001").unwrap();
    assert_eq!((todo.status, todo.claimed_by.as_ref().map(|c| c.by.clone())), (Status::InProgress, Some(Actor::agent("s1", "Fix flaky test"))));
    assert_eq!(book.claimed_by_session("s1").map(|t| t.id.as_str()), Some("DEC-001"), "its box shows the badge");
    assert!(made.message.starts_with("Claimed DEC-001 “A” for Fix flaky test"), "{}", made.message);
    assert!(todo.log.iter().any(|l| l.by == you() && l.text == "claimed for Fix flaky test"), "{:?}", todo.log);
    let event = &made.events[0];
    assert_eq!((event.data["by"].as_str(), event.data["for"].as_str(), event.data["for_session"].as_str()), (Some("you"), Some("Fix flaky test"), Some("s1")));
    assert!(book.apply(claim_for("DEC-001", "s1", false), &you(), &known).unwrap().message.contains("claimed by Fix flaky test already"));
    // Another session holds it: forcing reassigns it.
    assert!(book.apply(claim_for("DEC-001", "s2", false), &you(), &known).unwrap_err().contains("force the claim"));
    let made = book.apply(claim_for("DEC-001", "s2", true), &you(), &known).unwrap();
    assert!(made.warnings[0].contains("took DEC-001 over from Fix flaky test"), "{:?}", made.warnings);
    assert_eq!(book.todo("DEC-001").unwrap().claim_session(), Some("s2"));
    // Only sessions of the canvas (when it knows them, else the live ones).
    assert!(book.apply(claim_for("DEC-002", "s9", false), &you(), &known).unwrap_err().contains("no session \"s9\""));
    let live = Ctx { live: Some(["s3".to_owned()].into()), ..ctx() };
    assert!(book.apply(claim_for("DEC-002", "s9", false), &you(), &live).is_err());
    book.apply(claim_for("DEC-002", "s3", false), &Actor::bot("TODO bot"), &live).unwrap();
    assert_eq!(book.todo("DEC-002").unwrap().claimed_by.as_ref().map(|c| c.by.clone()), Some(Actor::agent("s3", "")), "a bot assigns too");
    // Agents claim for themselves only.
    book.apply(TodoOp::Release { id: "DEC-002".into() }, &you(), &ctx()).unwrap();
    assert!(book.apply(claim_for("DEC-002", "s2", false), &agent("s1"), &known).unwrap_err().contains("only for themselves"));
    book.apply(claim_for("DEC-002", "s1", false), &agent("s1"), &known).unwrap();
    assert_eq!(book.todo("DEC-002").unwrap().claimed_by.as_ref().map(|c| c.by.clone()), Some(agent("s1")), "naming itself is a plain claim");
    // Older clients send no session; it is left out of the JSON when there is none.
    assert_eq!(serde_json::from_str::<TodoOp>(r#"{"op":"claim","id":"DEC-1"}"#).unwrap(), TodoOp::Claim { id: "DEC-1".into(), force: false, session: None });
    assert_eq!(serde_json::to_value(claim_for("DEC-1", "s1", true)).unwrap(), serde_json::json!({ "op": "claim", "id": "DEC-1", "force": true, "session": "s1" }));
}

#[test]
fn proposals_wait_for_the_user() {
    let mut book = book();
    add(&mut book, NewTodo { propose: true, why: Some("a".into()), ..new("Decomps", "First idea") }, &agent("s1")).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("b".into()), ..new("Decomps", "Second idea") }, &agent("s1")).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("c".into()), ..new("Decomps", "Third idea") }, &agent("s1")).unwrap();
    let view = book.board_view("decomps").unwrap();
    assert_eq!(view.columns[0].status, Status::Proposed, "a Proposed column while there are proposals");
    let made = book.apply(TodoOp::Accept { id: "DEC-001".into(), status: Some(Status::Todo) }, &you(), &ctx()).unwrap();
    assert_eq!((status(&book, "DEC-001"), made.events[0].kind.as_str()), (Status::Todo, "todo.accepted"));
    assert!(book.apply(TodoOp::Accept { id: "DEC-001".into(), status: None }, &you(), &ctx()).unwrap_err().contains("not a proposal"));
    assert!(book.apply(TodoOp::Accept { id: "DEC-002".into(), status: Some(Status::Done) }, &you(), &ctx()).is_err());
    let made = book.apply(TodoOp::Decline { id: "DEC-002".into(), reason: Some("Not now.".into()) }, &you(), &ctx()).unwrap();
    let declined = book.todo("DEC-002").unwrap();
    assert_eq!((declined.status, declined.resolution, made.events[0].data["resolution"].as_str()), (Status::Canceled, Some(Resolution::Declined), Some("declined")));
    assert!(declined.log.iter().any(|l| l.kind == LogKind::Note && l.text == "declined: Not now."));
    book.apply(TodoOp::Reopen { id: "DEC-002".into() }, &you(), &ctx()).unwrap();
    assert_eq!(status(&book, "DEC-002"), Status::Proposed, "a declined proposal reopens as a proposal");
    // Dragging a proposal out of Proposed accepts it.
    let made = book.apply(TodoOp::Move { id: "DEC-003".into(), status: Some(Status::Backlog), board: None, after: None, before: None, rank: None }, &you(), &ctx()).unwrap();
    assert_eq!(made.events[0].kind, "todo.accepted");
    assert!(book.apply(TodoOp::Move { id: "DEC-003".into(), status: Some(Status::Proposed), board: None, after: None, before: None, rank: None }, &you(), &ctx()).unwrap_err().contains("backlog instead"));
    let needs: Vec<(&str, Need)> = book.needs_you(|_| false).iter().map(|n| (n.todo.id.as_str(), n.need)).collect();
    assert_eq!(needs, [("DEC-002", Need::Proposed)]);
    // Accepted straight into a full In progress: a warning, as for any move there.
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: Some(1), color: None, notes: None, policy: None, statuses: None }, &you(), &ctx()).unwrap();
    add(&mut book, NewTodo { status: Some(Status::InProgress), ..new("Decomps", "Busy") }, &you()).unwrap();
    let made = book.apply(TodoOp::Accept { id: "DEC-002".into(), status: Some(Status::InProgress) }, &you(), &ctx()).unwrap();
    assert!(made.warnings.iter().any(|w| w.contains("over its WIP limit of 1")), "{:?}", made.warnings);
}

#[test]
fn blocked_todos_wait_and_ready_ones_sort_by_priority_then_rank() {
    let mut book = book();
    let todo = |title: &str, priority: Priority| NewTodo { status: Some(Status::Todo), priority: Some(priority), ..new("Decomps", title) };
    add(&mut book, todo("Low", Priority::Low), &you()).unwrap();
    add(&mut book, todo("Urgent", Priority::Urgent), &you()).unwrap();
    add(&mut book, todo("None", Priority::None), &you()).unwrap();
    add(&mut book, NewTodo { board: "Parser".into(), ..todo("Urgent in a lane", Priority::Urgent) }, &you()).unwrap();
    add(&mut book, NewTodo { blocked: Some("waiting for the vendor".into()), ..todo("Blocked", Priority::Urgent) }, &you()).unwrap();
    let ids = |list: Vec<&Todo>| list.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    // Urgent first (ranks of different lanes still order them: deterministic), then low, then none.
    assert_eq!(ids(book.ready(Some("decomps"))), ["DEC-004", "DEC-002", "DEC-001", "DEC-003"]);
    assert_eq!(ids(book.ready(Some("decomps-parser"))), ["DEC-004"]);
    // A todo waits for the open todos it is blocked by.
    book.apply(TodoOp::Relate { id: "DEC-002".into(), kind: RelKind::BlockedBy, other: "DEC-001".into() }, &you(), &ctx()).unwrap();
    assert!(book.is_blocked(book.todo("DEC-002").unwrap()));
    assert_eq!(ids(book.blocking("DEC-001")), ["DEC-002"]);
    assert!(book.apply(TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::BlockedBy, other: "DEC-002".into() }, &you(), &ctx()).unwrap_err().contains("never end"));
    assert!(book.apply(TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::Related, other: "DEC-001".into() }, &you(), &ctx()).is_err());
    let claimed = book.apply(TodoOp::Claim { id: "DEC-002".into(), force: false, session: None }, &agent("s1"), &ctx()).unwrap();
    assert!(claimed.warnings.iter().any(|w| w.contains("waits for DEC-001")), "claiming a blocked todo warns: {:?}", claimed.warnings);
    book.apply(TodoOp::Release { id: "DEC-002".into() }, &agent("s1"), &ctx()).unwrap();
    book.apply(TodoOp::Done { id: "DEC-001".into(), note: None }, &you(), &ctx()).unwrap();
    assert!(!book.is_blocked(book.todo("DEC-002").unwrap()), "done blockers block nothing");
    assert_eq!(ids(book.ready(None)), ["DEC-004", "DEC-002", "DEC-003"]);
    book.apply(TodoOp::Update(Box::new(TodoUpdate { id: "DEC-005".into(), blocked: Some("none".into()), ..Default::default() })), &you(), &ctx()).unwrap();
    assert!(book.is_ready(book.todo("DEC-005").unwrap()));
    assert_eq!(book.ready_for_group(Some("g1")).len(), 4);
    assert_eq!(book.ready_for_group(Some("nobody")).len(), 4, "a group without boards sees every board");
    book.apply(TodoOp::Unrelate { id: "DEC-002".into(), other: "dec-1".into(), kind: None }, &you(), &ctx()).unwrap();
    assert!(book.todo("DEC-002").unwrap().relations.is_empty());
}

#[test]
fn moves_change_status_board_and_order() {
    let mut book = book();
    for title in ["A", "B", "C"] {
        add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", title) }, &you()).unwrap();
    }
    let order = |book: &TodoBook| book.board_view("decomps").unwrap().columns.iter().find(|c| c.status == Status::Todo).unwrap().cells[0].iter().map(|t| t.title.clone()).collect::<Vec<_>>();
    assert_eq!(order(&book), ["A", "B", "C"]);
    let place = |id: &str, after: Option<&str>, before: Option<&str>| TodoOp::Move { id: id.into(), status: None, board: None, after: after.map(str::to_owned), before: before.map(str::to_owned), rank: None };
    book.apply(place("DEC-003", None, Some("DEC-001")), &you(), &ctx()).unwrap();
    assert_eq!(order(&book), ["C", "A", "B"]);
    book.apply(place("DEC-003", Some("DEC-001"), None), &you(), &ctx()).unwrap();
    assert_eq!(order(&book), ["A", "C", "B"]);
    book.apply(place("DEC-001", Some("DEC-003"), Some("DEC-002")), &you(), &ctx()).unwrap();
    assert_eq!(order(&book), ["C", "A", "B"]);
    assert!(book.apply(place("DEC-001", Some("DEC-001"), None), &you(), &ctx()).is_err());
    assert!(book.apply(place("DEC-001", Some("DEC-099"), None), &you(), &ctx()).unwrap_err().contains("no todo DEC-099"));
    // Into a swimlane and another column (at its end).
    let made = book.apply(TodoOp::Move { id: "DEC-002".into(), status: Some(Status::InProgress), board: Some("Decomps/Parser".into()), after: None, before: None, rank: None }, &you(), &ctx()).unwrap();
    assert_eq!(made.message, "Moved DEC-002 to \"Parser\" › In progress.");
    assert_eq!((made.events[0].kind.as_str(), made.events[0].data["from"].as_str(), made.events[0].data["to"].as_str()), ("todo.status", Some("todo"), Some("in_progress")));
    let view = book.board_view("decomps-parser").unwrap();
    assert_eq!(view.board.id, "decomps", "a swimlane shows its parent board");
    let column = view.columns.iter().find(|c| c.status == Status::InProgress).unwrap();
    assert_eq!((column.cells[0].len(), column.cells[1][0].id.as_str()), (0, "DEC-002"));
    let made = book.apply(TodoOp::Move { id: "DEC-001".into(), status: Some(Status::Done), board: None, after: None, before: None, rank: None }, &you(), &ctx()).unwrap();
    assert_eq!(made.events[0].kind, "todo.done");
    let todo = book.todo("DEC-001").unwrap();
    assert_eq!((todo.resolution, todo.done_ms), (Some(Resolution::Done), Some(NOW)));
    // A column whose ranks grew long is spread again.
    book.todos.iter_mut().find(|t| t.id == "DEC-003").unwrap().rank = "V".repeat(30);
    let made = add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", "E") }, &you()).unwrap();
    assert!(made.changed.contains(&"DEC-003".to_owned()), "{:?}", made.changed);
    book.apply(place("DEC-004", None, Some("DEC-003")), &you(), &ctx()).unwrap();
    assert_eq!(order(&book), ["E", "C"]);
    assert!(book.todos.iter().all(|t| t.rank.len() <= 2 && valid_rank(&t.rank)), "{:?}", book.todos.iter().map(|t| &t.rank).collect::<Vec<_>>());
}

#[test]
fn notes_checklists_links_and_submissions() {
    let mut book = book();
    add(&mut book, NewTodo { status: Some(Status::Todo), checklist: vec!["one".into()], ..new("Decomps", "Work") }, &you()).unwrap();
    let s1 = agent("s1");
    book.apply(TodoOp::Claim { id: "DEC-001".into(), force: false, session: None }, &s1, &ctx()).unwrap();
    book.apply(TodoOp::Note { id: "DEC-001".into(), text: "Found the cause.\nFixing it.".into() }, &s1, &ctx()).unwrap();
    assert!(book.apply(TodoOp::Note { id: "DEC-001".into(), text: "  ".into() }, &s1, &ctx()).is_err());
    book.apply(TodoOp::AddCheck { id: "DEC-001".into(), text: "two".into() }, &s1, &ctx()).unwrap();
    let made = book.apply(TodoOp::Check { id: "DEC-001".into(), index: 1, done: true }, &s1, &ctx()).unwrap();
    assert_eq!(made.message, "DEC-001: 1/2 checklist items done.");
    assert!(book.apply(TodoOp::Check { id: "DEC-001".into(), index: 2, done: true }, &s1, &ctx()).unwrap_err().contains("pick 1 to 2"));
    book.apply(TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Note, target: r"C:\notebook\Todos\DEC-001.md".into(), title: None }, &s1, &ctx()).unwrap();
    assert_eq!(book.todo("DEC-001").unwrap().note.as_deref(), Some(r"C:\notebook\Todos\DEC-001.md"), "the first note link is its spec note");
    assert!(book.apply(TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Url, target: "ftp://x".into(), title: None }, &s1, &ctx()).is_err());
    assert!(book.apply(TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Pr, target: "https://github.com/o/r/issues/4".into(), title: None }, &s1, &ctx()).is_err());
    // Someone else's claim: only its holder (or the user) submits.
    assert!(book.apply(TodoOp::Submit { id: "DEC-001".into(), pr: None, summary: None }, &agent("s2"), &ctx()).unwrap_err().contains("only that session"));
    let made = book.apply(TodoOp::Submit { id: "DEC-001".into(), pr: Some("https://github.com/o/r/pull/4/files".into()), summary: Some("Fixed and tested.".into()) }, &s1, &ctx()).unwrap();
    let todo = book.todo("DEC-001").unwrap();
    assert_eq!((todo.status, todo.claim_session()), (Status::InReview, Some("s1")));
    assert_eq!(todo.prs().map(|l| (l.target.as_str(), l.title.as_str())).collect::<Vec<_>>(), [("https://github.com/o/r/pull/4", "o/r#4")]);
    assert_eq!((made.events[0].kind.as_str(), made.events[0].data["pr"].as_str()), ("todo.submitted", Some("https://github.com/o/r/pull/4")));
    assert!(todo.log.iter().any(|l| l.kind == LogKind::Note && l.text == "Found the cause.\nFixing it."));
    assert_eq!(book.todos_for_pr("https://github.com/O/R/pull/4"), ["DEC-001"], "a merged pull request finds its todos");
    assert_eq!(book.todos_for_pr("o/r#5"), Vec::<String>::new());
    let needs: Vec<Need> = book.needs_you(|s| s == "s1").iter().map(|n| n.need).collect();
    assert_eq!(needs, [Need::InReview, Need::StaleClaim]);
    book.apply(TodoOp::Unlink { id: "DEC-001".into(), target: "o/r#4".into() }, &s1, &ctx()).unwrap();
    assert_eq!(book.todo("DEC-001").unwrap().prs().count(), 0);
    assert!(book.apply(TodoOp::Unlink { id: "DEC-001".into(), target: "nothing".into() }, &s1, &ctx()).is_err());
    let made = book.apply(TodoOp::Done { id: "DEC-001".into(), note: Some("Merged.".into()) }, &you(), &ctx()).unwrap();
    assert_eq!(made.events[0].kind, "todo.done");
    assert!(book.todo("DEC-001").unwrap().claimed_by.is_none());
    assert!(book.apply(TodoOp::Submit { id: "DEC-001".into(), pr: None, summary: None }, &you(), &ctx()).unwrap_err().contains("reopening"));
}

#[test]
fn checklist_items_are_reworded_by_anyone_and_removed_by_people() {
    let mut book = book();
    add(&mut book, NewTodo { checklist: vec!["one".into(), "two".into(), "three".into()], ..new("Decomps", "Work") }, &you()).unwrap();
    let s1 = agent("s1");
    book.apply(TodoOp::Check { id: "DEC-001".into(), index: 1, done: true }, &s1, &ctx()).unwrap();
    let made = book.apply(TodoOp::EditCheck { id: "dec-1".into(), index: 1, text: "  two,\n reworded ".into() }, &s1, &ctx()).unwrap();
    assert_eq!(made.message, "Changed checklist item 2 of DEC-001.");
    let items = |book: &TodoBook| book.todo("DEC-001").unwrap().checklist.iter().map(|c| (c.text.clone(), c.done)).collect::<Vec<_>>();
    assert_eq!(items(&book)[1], ("two, reworded".to_owned(), true), "one line, the tick kept");
    assert!(book.apply(TodoOp::EditCheck { id: "DEC-001".into(), index: 1, text: "two, reworded".into() }, &s1, &ctx()).unwrap().message.starts_with("Nothing changed"));
    assert!(book.apply(TodoOp::EditCheck { id: "DEC-001".into(), index: 3, text: "x".into() }, &s1, &ctx()).unwrap_err().contains("pick 1 to 3"));
    assert!(book.apply(TodoOp::EditCheck { id: "DEC-001".into(), index: 0, text: " ".into() }, &s1, &ctx()).is_err(), "an empty item");
    let before = book.clone();
    assert!(book.apply(TodoOp::RemoveCheck { id: "DEC-001".into(), index: 0 }, &s1, &ctx()).unwrap_err().contains("agents may not"));
    assert_eq!(book, before);
    let made = book.apply(TodoOp::RemoveCheck { id: "DEC-001".into(), index: 0 }, &you(), &ctx()).unwrap();
    assert_eq!((made.message.as_str(), made.changed.as_slice()), ("Removed checklist item 1 from DEC-001.", ["DEC-001".to_owned()].as_slice()));
    assert_eq!(items(&book), [("two, reworded".to_owned(), true), ("three".to_owned(), false)]);
    assert!(book.apply(TodoOp::RemoveCheck { id: "DEC-001".into(), index: 2 }, &you(), &ctx()).is_err());
    let log = &book.todo("DEC-001").unwrap().log;
    assert!(log.iter().any(|l| l.text == "reworded checklist item “two” as “two, reworded”") && log.iter().any(|l| l.text == "removed checklist item “one”"), "{log:?}");
    // The window's bus action, as older services cannot read it.
    assert_eq!(client_action(&TodoOp::RemoveCheck { id: "DEC-001".into(), index: 0 }), serde_json::json!({ "action": "todos", "op": { "op": "remove_check", "id": "DEC-001", "index": 0 } }));
    assert_eq!(TodoOp::EditCheck { id: "A-1".into(), index: 0, text: "x".into() }.todo_id(), Some("A-1"));
}

#[test]
fn updates_check_everything_before_changing_anything() {
    let mut book = book();
    add(&mut book, new("Decomps", "Parent"), &you()).unwrap();
    add(&mut book, NewTodo { parent: Some("DEC-001".into()), labels: vec!["a".into(), "b".into()], ..new("Decomps", "Child") }, &you()).unwrap();
    let update = |u: TodoUpdate| TodoOp::Update(Box::new(TodoUpdate { id: "DEC-001".into(), ..u }));
    let rev = book.rev;
    assert!(book.apply(update(TodoUpdate { title: Some("New".into()), parent: Some("DEC-002".into()), ..Default::default() }), &you(), &ctx()).unwrap_err().contains("one of its children"));
    assert_eq!((book.todo("DEC-001").unwrap().title.as_str(), book.rev), ("Parent", rev), "nothing changed");
    let made = book.apply(update(TodoUpdate { title: Some("Parent todo".into()), kind: Some(TodoType::Chore), priority: Some(Priority::High), blocker: Some(true), body: Some("Line one\r\nLine two\n\n".into()), ..Default::default() }), &you(), &ctx()).unwrap();
    assert_eq!(made.message, "Updated DEC-001: title, type, priority, blocker, body.");
    let todo = book.todo("DEC-001").unwrap();
    assert_eq!((todo.body.as_str(), todo.rev, todo.updated_ms), ("Line one\nLine two", book.rev, NOW));
    let made = book.apply(TodoOp::Update(Box::new(TodoUpdate { id: "DEC-002".into(), add_labels: vec!["C".into()], remove_labels: vec!["a".into()], parent: Some("none".into()), ..Default::default() })), &you(), &ctx()).unwrap();
    assert_eq!(made.message, "Updated DEC-002: labels, parent.");
    assert_eq!(book.todo("DEC-002").unwrap().labels, ["b", "c"]);
    assert!(book.apply(update(TodoUpdate::default()), &you(), &ctx()).unwrap().message.contains("Nothing changed"));
    assert!(book.apply(update(TodoUpdate { title: Some("x".repeat(MAX_TITLE + 1)), ..Default::default() }), &you(), &ctx()).is_err());
}

#[test]
fn deleting_keeps_ids_used_and_cleans_relations() {
    let mut book = book();
    add(&mut book, new("Decomps", "A"), &you()).unwrap();
    add(&mut book, NewTodo { parent: Some("DEC-001".into()), ..new("Parser", "B") }, &you()).unwrap();
    book.apply(TodoOp::Relate { id: "DEC-002".into(), kind: RelKind::BlockedBy, other: "DEC-001".into() }, &you(), &ctx()).unwrap();
    let made = book.apply(TodoOp::Delete { id: "DEC-001".into() }, &you(), &ctx()).unwrap();
    assert!(made.changed.contains(&"DEC-002".to_owned()));
    let b = book.todo("DEC-002").unwrap();
    assert!(b.relations.is_empty() && b.parent.is_none());
    assert!(book.apply(TodoOp::DeleteBoard { board: "Decomps".into(), with_todos: false }, &you(), &ctx()).unwrap_err().contains("1 todo(s)"));
    book.apply(TodoOp::DeleteBoard { board: "Decomps".into(), with_todos: true }, &you(), &ctx()).unwrap();
    assert!(book.boards.is_empty() && book.todos.is_empty());
    add_board(&mut book, "Decomps", None, None, &[]);
    assert_eq!(add(&mut book, new("Decomps", "C"), &you()).unwrap().created.as_deref(), Some("DEC-003"), "a new board with the old prefix continues its numbers");
}

#[test]
fn groups_see_their_boards_counts_and_events() {
    let mut book = book();
    assert_eq!(book.group_counts("g2"), None, "no board: the chip offers to link one");
    assert_eq!(book.group_counts("g1"), Some(TodoCounts::default()));
    add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", "A") }, &you()).unwrap();
    add(&mut book, NewTodo { status: Some(Status::InProgress), ..new("Parser", "B") }, &you()).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("w".into()), ..new("Decomps", "C") }, &agent("s1")).unwrap();
    add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", "D") }, &you()).unwrap();
    book.apply(TodoOp::Done { id: "DEC-004".into(), note: None }, &you(), &ctx()).unwrap();
    assert_eq!(book.group_counts("g1"), Some(TodoCounts { open: 2, in_progress: 1, proposed: 1 }));
    assert_eq!(book.event_group("decomps-parser"), Some("g1"), "a swimlane's events go to its parent's group");
    book.apply(TodoOp::LinkBoard { board: "Parser".into(), group: "g2".into(), unlink: false }, &you(), &ctx()).unwrap();
    assert_eq!(book.event_group("decomps-parser"), Some("g2"));
    assert_eq!(book.group_counts("g2"), Some(TodoCounts { open: 1, in_progress: 1, proposed: 0 }), "a linked swimlane alone");
    assert!(book.apply(TodoOp::LinkBoard { board: "Parser".into(), group: "g9".into(), unlink: false }, &you(), &ctx()).unwrap_err().contains("no canvas group"));
    book.apply(TodoOp::LinkBoard { board: "Parser".into(), group: "g2".into(), unlink: true }, &you(), &ctx()).unwrap();
    assert_eq!(book.group_counts("g2"), None);
    assert_eq!(book.default_board(Some("g1")).map(|b| b.id.as_str()), Some("decomps"));
}
/// The owner's TODOs.md, as the TODO bot keeps it.
const TODOS_MD: &str = "# TODOs

Work items for Atelier. The TODO bot picks the first open item.

## Open

- [ ] **WI-019: [[WI-019 - Gentle momentum when panning]]** - A flick keeps the canvas gliding. (bot: blocked 2026-10-30: verification failed (cargo test -p canvas-core); see todo-bot\\logs)
- [ ] **WI-020: [[WI-020 - Fix the crash when a group is empty]]** - Deleting the last box of a group crashes the TUI.
  - [x] Reproduce
  - [ ] Fix and test
- [ ] **WI-004: [[WI-004 - Notebook space]]** - A notebook next to the canvas.
  It shows the group folders.
- [ ] **WI-007: [[WI-007 - Spaces foundation]]** - Toolbar tabs for Canvas, Notebook and Tasks.
- [?] **WI-021: [[WI-021 - Todo counts on chips]]** - Proposed by the bot.

## Done

- [x] **WI-001: [[WI-001 - Easier group selection]]** - Clicking a group's title selects the whole group. Done 2026-10-29 in `da03e6f`.
- [x] **WI-013: [[WI-013 - Faster startup]]** - Open the last canvas in under a second. Done 2026-10-30.

## Detailed requirements

### WI-013 - Faster startup
- Measure the cold start with the bench command.
- Load the index lazily.

### WI-019 - Gentle momentum when panning
- Velocity from the last 60 ms of the drag.
- Respect reduced motion.

### WI-099 - Gone
- Nothing.

## Relationships

- WI-004 follows WI-007: opening the notebook needs the spaces foundation.
- WI-020 is blocked by WI-019 (shared drag code).
";

#[test]
fn the_owners_todos_md_imports_with_its_ids_bodies_and_relations() {
    let imported = import_markdown(TODOS_MD, "canvas-app", "");
    assert_eq!(imported.prefix, "WI");
    let ids: Vec<&str> = imported.todos.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["WI-019", "WI-020", "WI-004", "WI-007", "WI-021", "WI-001", "WI-013"]);
    let get = |id: &str| imported.todos.iter().find(|t| t.id == id).unwrap();
    let momentum = get("WI-019");
    assert_eq!((momentum.title.as_str(), momentum.status, momentum.kind, momentum.board.as_str()), ("Gentle momentum when panning", Status::Todo, TodoType::Feature, "canvas-app"));
    assert_eq!(momentum.blocked.as_deref(), Some("verification failed (cargo test -p canvas-core); see todo-bot\\logs (bot, 2026-10-30)"));
    assert_eq!(momentum.body, "A flick keeps the canvas gliding.\n\n- Velocity from the last 60 ms of the drag.\n- Respect reduced motion.");
    let crash = get("WI-020");
    assert_eq!(crash.kind, TodoType::Bug);
    assert_eq!(crash.checklist, [CheckItem { text: "Reproduce".into(), done: true }, CheckItem { text: "Fix and test".into(), done: false }]);
    assert_eq!(crash.relations, [Relation { kind: RelKind::BlockedBy, id: "WI-019".into() }]);
    assert_eq!(crash.body, "Deleting the last box of a group crashes the TUI.\n\nWI-020 is blocked by WI-019 (shared drag code).");
    let notebook = get("WI-004");
    assert_eq!(notebook.relations, [Relation { kind: RelKind::Related, id: "WI-007".into() }]);
    assert_eq!(notebook.body, "A notebook next to the canvas.\nIt shows the group folders.\n\nWI-004 follows WI-007: opening the notebook needs the spaces foundation.");
    assert_eq!((get("WI-021").status, get("WI-021").proposal.as_ref().map(|p| p.why.as_str())), (Status::Proposed, Some("Proposed by the bot.")));
    let selection = get("WI-001");
    assert_eq!((selection.status, selection.resolution, selection.body.as_str()), (Status::Done, Some(Resolution::Done), "Clicking a group's title selects the whole group."));
    assert_eq!(selection.done_ms.map(super::ops::date).as_deref(), Some("2026-10-29"));
    assert!(selection.log.iter().any(|l| l.text == "done in `da03e6f`"));
    let startup = get("WI-013");
    assert_eq!((startup.kind, startup.body.as_str()), (TodoType::Optimization, "Open the last canvas in under a second.\n\n- Measure the cold start with the bench command.\n- Load the index lazily."));
    assert_eq!(imported.warnings, ["details for WI-099, which is not in the list"]);
    // Into a book: a new board with the file's prefix; numbering goes on after the highest id.
    let mut book = TodoBook::default();
    let made = book.apply(TodoOp::Import { board: "Canvas App".into(), prefix: None, text: TODOS_MD.into() }, &you(), &ctx()).unwrap();
    assert_eq!(made.message, "Imported 7 todo(s) into \"Canvas App\" (WI): 4 open, 2 done, 1 proposed.");
    assert_eq!((book.boards[0].prefix.as_str(), book.boards[0].id.as_str()), ("WI", "canvas-app"));
    assert_eq!(book.ready(None).iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["WI-004", "WI-007"], "WI-019 is blocked, WI-020 waits for it");
    assert_eq!(add(&mut book, new("Canvas App", "Next"), &you()).unwrap().created.as_deref(), Some("WI-022"), "after the highest id of the file");
    let again = book.apply(TodoOp::Import { board: "Canvas App".into(), prefix: None, text: TODOS_MD.into() }, &you(), &ctx()).unwrap();
    assert!(again.message.starts_with("Imported 0 todo(s)") && again.message.contains("7 already on the canvas"), "{}", again.message);
    assert!(book.apply(TodoOp::Import { board: "X".into(), prefix: None, text: "no items here".into() }, &you(), &ctx()).unwrap_err().contains("no todo items"));
    // Items without ids get the board's.
    let made = book.apply(TodoOp::Import { board: "Canvas App".into(), prefix: None, text: "- [ ] Write the release notes - for 0.4\n".into() }, &you(), &ctx()).unwrap();
    let todo = book.todo(made.changed.last().unwrap()).unwrap();
    assert_eq!((todo.id.as_str(), todo.title.as_str(), todo.body.as_str()), ("WI-023", "Write the release notes", "for 0.4"));
}

#[test]
fn the_mirror_writes_that_format_and_reads_back() {
    let mut book = TodoBook::default();
    book.apply(TodoOp::Import { board: "Canvas App".into(), prefix: None, text: TODOS_MD.into() }, &you(), &ctx()).unwrap();
    add_board(&mut book, "Lane", None, Some("Canvas App"), &[]);
    add(&mut book, NewTodo { kind: TodoType::Chore, priority: Some(Priority::High), status: Some(Status::Todo), ..new("Canvas App/Lane", "Tidy the lane") }, &you()).unwrap();
    book.apply(TodoOp::Claim { id: "WI-007".into(), force: false, session: None }, &agent("s1"), &ctx()).unwrap();
    let mirror = mirror_markdown(&book);
    assert!(mirror.starts_with("# TODOs\n\n> Generated by Atelier"), "{mirror}");
    for line in [
        "## Canvas App (WI)",
        "- [ ] **WI-007: [[WI-007 - Spaces foundation]]** - Toolbar tabs for Canvas, Notebook and Tasks. (canvas: in progress; claimed by Session s1)",
        "- [ ] **WI-019: [[WI-019 - Gentle momentum when panning]]** - A flick keeps the canvas gliding. (canvas: blocked: verification failed (cargo test -p canvas-core); see todo-bot\\logs (bot, 2026-10-30))",
        "- [x] **WI-001: [[WI-001 - Easier group selection]]** - Clicking a group's title selects the whole group. Done 2026-10-29 in `da03e6f`.",
        "- [x] **WI-013: [[WI-013 - Faster startup]]** - Open the last canvas in under a second. Done 2026-10-30.",
        "### Lane",
        "- [ ] **WI-022: [[WI-022 - Tidy the lane]]** - (canvas: chore; high)",
        "## Proposed\n\n- [?] **WI-021: [[WI-021 - Todo counts on chips]]** - Proposed by the bot.",
        "## Detailed requirements\n\n### WI-019 - Gentle momentum when panning\n\n- Velocity from the last 60 ms of the drag.\n- Respect reduced motion.",
        "## Relationships\n\n- WI-020 blocked by WI-019\n- WI-004 related to WI-007",
    ] {
        assert!(mirror.contains(line), "the mirror has {line:?}:\n{mirror}");
    }
    let open = mirror.find("WI-007: [[").unwrap();
    assert!(open < mirror.find("WI-019: [[").unwrap() && mirror.find("WI-019: [[").unwrap() < mirror.find("WI-001: [[").unwrap(), "in progress, then open, then done");
    // Read back: the same todos.
    let back = import_markdown(&mirror, "x", "");
    assert_eq!(back.todos.len(), book.todos.len());
    for todo in &book.todos {
        let read = back.todos.iter().find(|t| t.id == todo.id).unwrap_or_else(|| panic!("{} is in the mirror", todo.id));
        assert_eq!((&read.title, read.status, read.kind, read.priority, &read.blocked), (&todo.title, todo.status, todo.kind, todo.priority, &todo.blocked), "{}", todo.id);
        assert_eq!(read.relations, todo.relations, "{}", todo.id);
        let first = |body: &str| body.split("\n\n").next().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(first(&read.body), first(&todo.body), "{}", todo.id);
    }
    assert!(back.warnings.is_empty(), "{:?}", back.warnings);
}

#[test]
fn json_canvas_has_a_group_per_board_and_column_and_a_node_per_todo() {
    let mut book = book();
    add(&mut book, NewTodo { status: Some(Status::Todo), kind: TodoType::Bug, ..new("Decomps", "A") }, &you()).unwrap();
    add(&mut book, NewTodo { status: Some(Status::InProgress), ..new("Parser", "B") }, &you()).unwrap();
    add(&mut book, new("Decomps", "Canceled"), &you()).unwrap();
    book.apply(TodoOp::Cancel { id: "DEC-003".into(), reason: None, resolution: None }, &you(), &ctx()).unwrap();
    book.apply(TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Note, target: "Todos/DEC-001 - A.md".into(), title: None }, &you(), &ctx()).unwrap();
    book.apply(TodoOp::Relate { id: "DEC-002".into(), kind: RelKind::BlockedBy, other: "DEC-001".into() }, &you(), &ctx()).unwrap();
    let canvas = json_canvas(&book, Some("Parser")).unwrap();
    let nodes = canvas["nodes"].as_array().unwrap();
    let of = |kind: &str| nodes.iter().filter(|n| n["type"] == kind).collect::<Vec<_>>();
    let groups = of("group");
    assert_eq!(groups[0]["label"], "Decomps (DEC)", "a swimlane exports its board");
    assert_eq!(groups.iter().skip(1).map(|g| g["label"].as_str().unwrap()).collect::<Vec<_>>(), ["Backlog (0)", "Todo (1)", "In progress (1)", "In review (0)", "Done (0)"]);
    let texts = of("text");
    assert_eq!(texts.len(), 2, "canceled todos are left out");
    assert!(texts[0]["text"].as_str().unwrap().starts_with("**DEC-001** · bug") && texts[0]["color"] == "1");
    assert!(texts[1]["text"].as_str().unwrap().contains("lane: Parser") && texts[1]["text"].as_str().unwrap().contains("⚑ blocked: waits for DEC-001"));
    assert_eq!(of("file")[0]["file"], "Todos/DEC-001 - A.md");
    for node in nodes {
        for key in ["id", "x", "y", "width", "height"] {
            assert!(!node[key].is_null(), "every node has {key}: {node}");
        }
    }
    let edges = canvas["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().any(|e| e["fromNode"] == "todo-DEC-002" && e["toNode"] == "todo-DEC-001" && e["label"] == "blocked by"));
    assert!(json_canvas(&book, Some("nope")).is_err());
    assert_eq!(json_canvas(&TodoBook::default(), None).unwrap(), serde_json::json!({ "nodes": [], "edges": [] }));
}

#[test]
fn digests_summarize_a_groups_boards_and_the_whole_canvas() {
    let mut book = book();
    add(&mut book, NewTodo { status: Some(Status::Todo), kind: TodoType::Bug, priority: Some(Priority::High), blocker: true, ..new("Decomps", "Crash on load") }, &you()).unwrap();
    add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", "Map structs") }, &you()).unwrap();
    book.apply(TodoOp::Claim { id: "DEC-002".into(), force: false, session: None }, &agent("s1"), &ctx()).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("w".into()), ..new("Decomps", "Idea") }, &agent("s1")).unwrap();
    for n in 0..25 {
        add(&mut book, new("Parser", &format!("Lane work {n}")), &you()).unwrap();
    }
    let summary = group_summary(&book, "g1").unwrap();
    assert!(summary.starts_with("## Decomps (DEC) · swimlanes: Parser\n\n27 open (25 backlog, 1 todo, 1 in progress, 0 in review) · 1 proposed · 0 done\n"), "{summary}");
    assert!(summary.contains("Agents: add todos the user asked for, propose your own ideas; done and canceled are the user's"));
    assert!(summary.contains("- DEC-002 [in progress, claimed by Session s1] (feature) Map structs\n- DEC-001 [todo, ready] (bug, high) Crash on load ‼\n"), "{summary}");
    assert!(summary.contains("- … and 7 more (atelier todo list --board \"Decomps\")"), "at most 20 open todos: {summary}");
    assert_eq!(group_summary(&book, "g2"), None);
    let names: BTreeMap<String, String> = [("g1".to_owned(), "Pay".to_owned())].into();
    let canvas = canvas_summary(&book, &names);
    assert!(canvas.starts_with("- Decomps (DEC), swimlanes: Parser · groups: Pay · 27 open"), "{canvas}");
    assert!(canvas.contains("0 blocked, 1 ready") && canvas.ends_with("Waiting for the user: 1 proposal(s), 0 todo(s) in review.\n"), "{canvas}");
    assert_eq!(canvas_summary(&TodoBook::default(), &names), "No todo boards yet.\n");
}

#[test]
fn the_demo_book_has_a_bit_of_everything() {
    let book = demo_book();
    assert_eq!(book.boards.iter().map(|b| (b.name.as_str(), b.prefix.as_str())).collect::<Vec<_>>(), [("Canvas App", "WI"), ("Decomps", "DEC"), ("Parser", "DEC")]);
    for status in Status::ALL {
        assert!(book.todos.iter().any(|t| t.status == status), "a todo {status:?}");
    }
    assert!(book.todos.iter().any(|t| t.blocker) && book.todos.iter().any(|t| book.is_blocked(t) && t.blocked.is_some()) && book.todos.iter().any(|t| !book.blockers(t).is_empty()));
    assert!(book.todos.iter().any(|t| t.checklist.len() > 1) && book.todos.iter().any(|t| t.prs().count() > 0));
    assert_eq!(book.claimed_by_session("demo-main").map(|t| t.id.as_str()), Some("WI-002"));
    assert_eq!(book.claimed_by_session("demo-review").map(|t| t.id.as_str()), Some("WI-003"));
    assert_eq!(book.group_counts("g-demo"), Some(TodoCounts { open: 5, in_progress: 1, proposed: 1 }));
    assert!(book.todos.iter().filter(|t| t.board == "decomps-parser").count() >= 2, "the swimlane has cards");
    let view = book.board_view("decomps").unwrap();
    assert_eq!((view.lanes.len(), view.columns[0].status, view.canceled), (2, Status::Proposed, 1));
    let text = serde_json::to_string(&book).unwrap();
    assert_eq!(serde_json::from_str::<TodoBook>(&text).unwrap(), book, "it survives JSON");
}

#[test]
fn operations_travel_as_json_and_old_books_still_read() {
    let ops = [
        TodoOp::Claim { id: "DEC-031".into(), force: false, session: None },
        TodoOp::Add(Box::new(NewTodo { title: "x".into(), kind: TodoType::Bug, propose: true, why: Some("y".into()), ..Default::default() })),
        TodoOp::Update(Box::new(TodoUpdate { id: "DEC-1".into(), blocked: Some(String::new()), ..Default::default() })),
        TodoOp::Move { id: "DEC-1".into(), status: Some(Status::InReview), board: None, after: Some("DEC-2".into()), before: None, rank: None },
        TodoOp::SetMirror { path: None },
    ];
    for op in ops {
        let text = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<TodoOp>(&text).unwrap(), op, "{text}");
    }
    let message = client_action(&TodoOp::Claim { id: "DEC-031".into(), force: true, session: None });
    assert_eq!(message, serde_json::json!({ "action": "todos", "op": { "op": "claim", "id": "DEC-031", "force": true } }));
    let add: TodoOp = serde_json::from_str(r#"{"op":"add","title":"Fix it","type":"bug","board":"DEC","request":"r-1"}"#).unwrap();
    assert!(matches!(&add, TodoOp::Add(new) if new.kind == TodoType::Bug && new.board == "DEC"), "unknown fields are ignored");
    assert_eq!(add.word(), "add");
    assert_eq!(serde_json::from_str::<TodoOp>(r#"{"op":"check","id":"A-1","index":0}"#).unwrap(), TodoOp::Check { id: "A-1".into(), index: 0, done: true });
    assert!(serde_json::from_str::<TodoOp>(r#"{"op":"explode"}"#).is_err());
    let old: TodoBook = serde_json::from_str(r#"{"boards":[{"id":"b","name":"B","prefix":"BB"}],"todos":[{"id":"BB-001","title":"t","board":"b","later":1}]}"#).unwrap();
    assert_eq!((old.version, old.todos[0].status, old.todos[0].kind, old.boards[0].w), (VERSION, Status::Backlog, TodoType::Feature, BOARD_W));
    let json = serde_json::to_value(&old.todos[0]).unwrap();
    assert!(json.get("blocked").is_none() && json.get("claimed_by").is_none() && json["type"] == "feature", "empty fields stay out: {json}");
}

#[test]
fn applying_is_all_or_nothing_and_counts_revisions() {
    let mut book = book();
    let rev = book.rev;
    add(&mut book, new("Decomps", "A"), &you()).unwrap();
    assert_eq!(book.rev, rev + 1);
    let before = book.clone();
    assert!(book.apply(TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::Related, other: "DEC-404".into() }, &you(), &ctx()).is_err());
    assert!(book.apply(TodoOp::Link { id: "DEC-001".into(), kind: LinkKind::Pr, target: "not a link".into(), title: None }, &you(), &ctx()).is_err());
    assert!(book.apply(TodoOp::Delete { id: "DEC-404".into() }, &you(), &ctx()).unwrap_err().contains("no todo DEC-404"));
    assert_eq!(book, before);
    assert!(book.apply(TodoOp::SetMirror { path: Some("notes.txt".into()) }, &you(), &ctx()).unwrap_err().contains(".md"));
    book.apply(TodoOp::SetMirror { path: Some(r"C:\repo\TODOs.md".into()) }, &you(), &ctx()).unwrap();
    assert_eq!(book.settings.mirror.as_deref(), Some(r"C:\repo\TODOs.md"));
    book.apply(TodoOp::SetMirror { path: Some("off".into()) }, &you(), &ctx()).unwrap();
    assert_eq!(book.settings.mirror, None);
    // Board view basics: Proposed only when needed, Done collapsed, Canceled a count.
    let view = book.board_view("decomps").unwrap();
    assert_eq!(view.columns.iter().map(|c| (c.status, c.collapsed)).collect::<Vec<_>>(), [(Status::Backlog, false), (Status::Todo, false), (Status::InProgress, false), (Status::InReview, false), (Status::Done, true)]);
    book.apply(TodoOp::UpdateBoard { board: "decomps".into(), name: None, prefix: None, wip: None, color: None, notes: None, policy: None, statuses: Some(StatusNames { todo: Some("Ready".into()), ..Default::default() }) }, &you(), &ctx()).unwrap();
    let view = book.board_view("decomps").unwrap();
    assert_eq!((view.columns[1].name.as_str(), view.progress()), ("Ready", (0, 1)));
    // A check of the changed book (the service's size budget) refuses it whole.
    let before = book.clone();
    let refused = book.apply_checked(TodoOp::Add(Box::new(new("Decomps", "Too big"))), &you(), &ctx(), |next| if next.todos.len() > 1 { Err("the book is full".into()) } else { Ok(()) });
    assert_eq!((refused, &book), (Err("the book is full".to_owned()), &before));
}

#[test]
fn ids_and_places_stay_in_range() {
    let mut book = book();
    // The last number of a prefix: an error, never an overflow or a 10-digit id.
    book.seq.insert("DEC".into(), MAX_SEQ - 1);
    assert_eq!(add(&mut book, new("Decomps", "Last"), &you()).unwrap().created.as_deref(), Some("DEC-999999999"));
    assert!(parse_id("DEC-999999999").is_some());
    let before = book.clone();
    assert!(add(&mut book, new("Decomps", "One more"), &you()).unwrap_err().contains("used all its numbers"));
    book.seq.insert("DEC".into(), u32::MAX);
    assert!(add(&mut book, new("Decomps", "One more"), &you()).unwrap_err().contains("used all its numbers"));
    book.seq.insert("DEC".into(), MAX_SEQ);
    assert_eq!(book, before, "refused: nothing changed");
    // Boards far away are kept in range, and a new board still finds its place.
    let mut book = self::book();
    book.apply(TodoOp::MoveBoard { board: "Decomps".into(), x: i32::MAX, y: i32::MIN, w: Some(i32::MAX), h: None }, &you(), &ctx()).unwrap();
    let decomps = book.board("decomps").unwrap();
    assert_eq!((decomps.x, decomps.y, decomps.w), (MAX_COORD, -MAX_COORD, 2_000));
    book.boards[0].x = i32::MAX;
    let made = book.apply(TodoOp::AddBoard { name: "Next".into(), prefix: None, parent: None, groups: vec![], x: None, y: Some(i32::MAX) }, &you(), &ctx()).unwrap();
    let next = book.board(made.created.as_deref().unwrap()).unwrap();
    assert_eq!((next.x, next.y), (MAX_COORD, MAX_COORD));
}

#[test]
fn agents_leave_proposals_declines_and_other_sessions_claims_to_their_owners() {
    let mut book = book();
    let close = AgentPolicy { agent_adds: AddPolicy::Direct, agents_may_close: true };
    book.apply(TodoOp::UpdateBoard { board: "Decomps".into(), name: None, prefix: None, wip: None, color: None, notes: None, policy: Some(close), statuses: None }, &you(), &ctx()).unwrap();
    add(&mut book, NewTodo { propose: true, why: Some("w".into()), ..new("Decomps", "Idea") }, &agent("s1")).unwrap();
    add(&mut book, NewTodo { status: Some(Status::Todo), ..new("Decomps", "Work") }, &you()).unwrap();
    book.apply(TodoOp::Claim { id: "DEC-002".into(), force: false, session: None }, &agent("s1"), &ctx()).unwrap();
    let before = book.clone();
    // Even where agents may close todos: a proposal is the user's to accept, and s1's claim is s1's.
    assert!(book.apply(TodoOp::Done { id: "DEC-001".into(), note: None }, &agent("s2"), &ctx()).unwrap_err().contains("proposal"));
    for op in [
        TodoOp::Done { id: "DEC-002".into(), note: None },
        TodoOp::Cancel { id: "DEC-002".into(), reason: None, resolution: None },
        TodoOp::Move { id: "DEC-002".into(), status: Some(Status::Todo), board: None, after: None, before: None, rank: None },
    ] {
        let word = op.word();
        assert!(book.apply(op, &agent("s2"), &ctx()).unwrap_err().contains("claimed by Session s1"), "{word}");
    }
    assert_eq!(book, before, "refused: nothing changed");
    // Its owner may; so may another session once s1 is gone (a stale claim), and the user always.
    let gone = Ctx { live: Some(["s2".to_owned()].into()), ..ctx() };
    book.apply(TodoOp::Move { id: "DEC-002".into(), status: Some(Status::InReview), board: None, after: None, before: None, rank: None }, &agent("s2"), &gone).unwrap();
    book.apply(TodoOp::Done { id: "DEC-002".into(), note: None }, &agent("s1"), &ctx()).unwrap();
    // A declined proposal is reopened by the user only.
    book.apply(TodoOp::Decline { id: "DEC-001".into(), reason: None }, &you(), &ctx()).unwrap();
    assert!(book.apply(TodoOp::Reopen { id: "DEC-001".into() }, &agent("s1"), &ctx()).unwrap_err().contains("declined proposal"));
    book.apply(TodoOp::Reopen { id: "DEC-002".into() }, &agent("s1"), &ctx()).unwrap();
    book.apply(TodoOp::Reopen { id: "DEC-001".into() }, &you(), &ctx()).unwrap();
    assert_eq!((status(&book, "DEC-001"), status(&book, "DEC-002")), (Status::Proposed, Status::Todo));
    // Swimlanes: agents add them to their own group's boards only.
    let mut other = TodoBook::default();
    add_board(&mut other, "Docs", None, None, &["g2"]);
    let lane = |parent: &str| TodoOp::AddBoard { name: "Lane".into(), prefix: None, parent: Some(parent.into()), groups: vec![], x: None, y: None };
    assert!(other.apply(lane("Docs"), &agent("s1"), &ctx()).unwrap_err().contains("not linked to your canvas group"));
    other.apply(lane("Docs"), &agent("s2"), &Ctx { group: Some("g2".into()), ..ctx() }).unwrap();
}

#[test]
fn groups_that_left_the_canvas_can_be_unlinked() {
    let mut book = book();
    // g1 is linked; the canvas no longer has it.
    let without = Ctx { groups: [("g2".to_owned(), "Docs".to_owned())].into(), ..ctx() };
    assert!(book.apply(TodoOp::LinkBoard { board: "Decomps".into(), group: "g1".into(), unlink: false }, &you(), &without).unwrap_err().contains("no canvas group"));
    let made = book.apply(TodoOp::LinkBoard { board: "Decomps".into(), group: "g1".into(), unlink: true }, &you(), &without).unwrap();
    assert!(made.message.contains("no longer linked") && book.board("decomps").unwrap().canvas_groups.is_empty(), "{}", made.message);
    assert!(book.apply(TodoOp::LinkBoard { board: "Decomps".into(), group: "g9".into(), unlink: true }, &you(), &without).unwrap_err().contains("no canvas group"));
}

#[test]
fn logs_and_relations_stay_bounded() {
    let mut book = book();
    add(&mut book, new("Decomps", "Chatty"), &you()).unwrap();
    for n in 0..40 {
        book.apply(TodoOp::Note { id: "DEC-001".into(), text: format!("{n:02} {}", "x".repeat(MAX_TEXT - 3)) }, &agent("s1"), &ctx()).unwrap();
    }
    let log = &book.todo("DEC-001").unwrap().log;
    assert!(log.iter().map(|l| l.text.len()).sum::<usize>() <= MAX_LOG_BYTES, "{} entries", log.len());
    assert!(log.last().unwrap().text.starts_with("39 ") && log.len() == MAX_LOG_BYTES / MAX_TEXT, "the newest are kept: {}", log.len());
    let mut long = vec![LogEntry { text: "y".repeat(MAX_LOG_BYTES * 2), ..Default::default() }];
    super::ops::trim_log(&mut long);
    assert_eq!(long.len(), 1, "the newest entry stays, however long");
    for n in 0..MAX_RELATIONS + 1 {
        add(&mut book, new("Decomps", &format!("Other {n}")), &you()).unwrap();
    }
    for n in 0..MAX_RELATIONS {
        book.apply(TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::Related, other: format_id("DEC", n as u32 + 2) }, &you(), &ctx()).unwrap();
    }
    let last = format_id("DEC", MAX_RELATIONS as u32 + 2);
    assert!(book.apply(TodoOp::Relate { id: "DEC-001".into(), kind: RelKind::Related, other: last }, &you(), &ctx()).unwrap_err().contains("at most"));
}

#[test]
fn imports_read_any_file_within_the_books_limits() {
    // A byte order mark and Windows line ends change nothing.
    let windows = format!("\u{feff}{}", TODOS_MD.replace('\n', "\r\n"));
    let (plain, read) = (import_markdown(TODOS_MD, "b", ""), import_markdown(&windows, "b", ""));
    assert_eq!(read, plain);
    let first = import_markdown("\u{feff}## Proposed\n- [ ] **WI-001: [[WI-001 - A]]** - a.\n", "b", "");
    assert_eq!(first.todos[0].status, Status::Proposed, "a heading on the first line counts");
    // Oversized parts are cut to what the book keeps, and said so.
    let checks: String = (0..MAX_CHECKLIST + 5).map(|n| format!("  - [ ] step {n}\n")).collect();
    let big = format!(
        "- [?] **WI-001: [[WI-001 - Big]]** - {why} (bot: blocked: {reason})\n  - [ ] {item}\n{checks}\n## Details\n\n### WI-001 - Big\n{body}\n",
        why = "w".repeat(MAX_TEXT * 2),
        reason = "r".repeat(2_000),
        item = "i".repeat(MAX_TEXT * 2),
        body = "é".repeat(MAX_BODY),
    );
    let imported = import_markdown(&big, "b", "");
    let todo = &imported.todos[0];
    assert!(todo.body.len() <= MAX_BODY && todo.body.ends_with('é'), "cut at a character: {}", todo.body.len());
    assert_eq!(todo.checklist.len(), MAX_CHECKLIST);
    assert_eq!(todo.checklist[0].text.chars().count(), MAX_TEXT, "a long item is cut");
    assert!(todo.blocked.as_ref().is_some_and(|b| b.chars().count() <= 500));
    assert!(todo.proposal.as_ref().is_some_and(|p| p.why.chars().count() == MAX_TEXT));
    assert!(todo.log.iter().all(|l| l.text.chars().count() <= MAX_TEXT));
    assert!(imported.warnings.iter().any(|w| w.contains("body of WI-001 was cut")) && imported.warnings.iter().any(|w| w.contains("first 100 checklist items")), "{:?}", imported.warnings);
    // An id twice in one file: the first item, and a warning.
    let mut book = TodoBook::default();
    let twice = "- [ ] **WI-001: [[WI-001 - First]]** - a\n- [ ] **WI-1: [[WI-1 - Second]]** - b\n";
    let made = book.apply(TodoOp::Import { board: "Canvas App".into(), prefix: None, text: twice.into() }, &you(), &ctx()).unwrap();
    assert_eq!((book.todos.len(), book.todos[0].title.as_str()), (1, "First"));
    assert!(made.message.starts_with("Imported 1 todo(s)") && !made.message.contains("already on the canvas"), "{}", made.message);
    assert_eq!(made.warnings, ["WI-1 is in the file more than once: its first item was imported"]);
    // A long file keeps its order (ranks spread once at the end).
    let long: String = (1..=3_000).map(|n| format!("- [ ] **LG-{n}: [[LG-{n} - Item {n}]]** - x\n")).collect();
    let mut book = TodoBook::default();
    book.apply(TodoOp::Import { board: "Long".into(), prefix: None, text: long }, &you(), &ctx()).unwrap();
    let view = book.board_view("long").unwrap();
    let column = view.columns.iter().find(|c| c.status == Status::Todo).unwrap();
    let titles: Vec<&str> = column.cells[0].iter().map(|t| t.title.as_str()).collect();
    assert_eq!((titles.len(), titles[0], titles[2_999]), (3_000, "Item 1", "Item 3000"));
    assert!(column.cells[0].windows(2).all(|w| w[0].rank < w[1].rank && w[1].rank.len() <= 3), "in file order, short ranks");
    assert_eq!(add(&mut book, new("Long", "Next"), &you()).unwrap().created.as_deref(), Some("LG-3001"));
    // Lines of any shape never break the reader.
    for odd in ["- [", "- [x", "- [ ] **", "- [ ] **WI-1", "- [ ] (bot:", "- [ ] (canvas: blocked:", "- [x] Done 2026-1", "- [ ] x Done 2026-10-3é", "## Relationships\n- WI-1", "## Details\n### ", "- [ ] [[", "- [ ] [[WI-1 - ]]", "\t\t- [x] é", "- [ ] (bot: blocKed x)", "- [ ] (bot: bloc\u{212a}ed: x)"] {
        let imported = import_markdown(odd, "b", "");
        assert!(imported.todos.iter().all(|t| !t.title.is_empty()), "{odd:?}");
    }
}
