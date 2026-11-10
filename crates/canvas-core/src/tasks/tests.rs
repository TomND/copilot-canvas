//! Tests of the Tasks space: layout, hit testing, drags, keys, the
//! sidebar's lists, the links from the session canvas, optimistic changes
//! and the dialogs.

use std::collections::BTreeMap;

use canvas_model::todos::{Actor, Ctx, NewTodo, Status, TodoBook, TodoOp, TodoType, demo_book};
use crossbeam_channel::Receiver;
use ratatui::buffer::Buffer;

use super::layout::{Cells, Chip, Dir, Filter, Input, Spot, Target, View};
use super::*;
use crate::app::{AppEvent, Focus};
use crate::dialogs::Modal;
use crate::doc::Doc;
use crate::input::{InputEvent, KeyCode, Modifiers, MouseButton, MouseEventKind};
use crate::protocol::ClientMsg;
use crate::side::RightDetail;

/// The screen the app tests draw (wide enough for three columns of a board at 100 %).
const W: u16 = 240;

fn add(board: &str, title: &str, status: Status) -> TodoOp {
    TodoOp::Add(Box::new(NewTodo { board: board.into(), title: title.into(), status: Some(status), ..Default::default() }))
}

/// A book with one board ("Board", BRD) linked to `group`: `open` accepted
/// todos (the first `in_progress` of them in progress) and `proposed` proposals.
pub(crate) fn book_with(group: &str, open: usize, in_progress: usize, proposed: usize) -> TodoBook {
    let mut book = TodoBook::default();
    let (you, ctx) = (Actor::human(), Ctx::at(1_000));
    book.apply(TodoOp::AddBoard { name: "Board".into(), prefix: Some("BRD".into()), parent: None, groups: vec![group.into()], x: None, y: None }, &you, &ctx).unwrap();
    for i in 0..open {
        book.apply(add("Board", &format!("Todo {i}"), if i < in_progress { Status::InProgress } else { Status::Backlog }), &you, &ctx).unwrap();
    }
    for i in 0..proposed {
        book.apply(TodoOp::Add(Box::new(NewTodo { board: "Board".into(), title: format!("Idea {i}"), propose: true, ..Default::default() })), &you, &ctx).unwrap();
    }
    book
}

struct Rig {
    app: App,
    sent: Receiver<ClientMsg>,
    events: Receiver<AppEvent>,
}

fn rig(doc: Doc, demo: bool) -> Rig {
    let (tx, sent) = crossbeam_channel::unbounded();
    let (_service, service_rx) = crossbeam_channel::bounded(1);
    let (app_tx, events) = crossbeam_channel::unbounded();
    let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), demo, ".".into(), app_tx, None);
    app.notice = None;
    Rig { app, sent, events }
}

/// The demo canvas in the Tasks space (its demo book), drawn once.
fn demo() -> Rig {
    let mut rig = rig(crate::demo::demo_doc("."), true);
    rig.app.switch_space(Space::Tasks);
    render(&mut rig.app, W, 50);
    rig
}

/// The demo canvas with the demo book as the service's (changes are sent).
fn served() -> Rig {
    let mut rig = rig(crate::demo::demo_doc("."), false);
    rig.app.bus.todos = Some(demo_book());
    rig.app.switch_space(Space::Tasks);
    render(&mut rig.app, W, 50);
    rig
}

fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    crate::ui::render(app, &mut buf, area, None);
    buf
}

fn row_text(buf: &Buffer, r: Rect) -> String {
    (r.y..r.bottom()).map(|y| (r.x..r.right()).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
}

/// The todo operations sent to the service.
fn ops(sent: &Receiver<ClientMsg>) -> Vec<serde_json::Value> {
    sent.try_iter()
        .filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str::<serde_json::Value>(&json).ok() } else { None })
        .filter(|v| v["action"] == "todos")
        .map(|v| v["op"].clone())
        .collect()
}

fn mouse(app: &mut App, kind: MouseEventKind, (x, y): (i32, i32)) {
    app.on_input(vec![InputEvent::Mouse(crate::input::MouseEvent::at_cell(kind, x as u16, y as u16, Modifiers::NONE))]);
}

fn drag(app: &mut App, from: (i32, i32), to: (i32, i32)) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), from);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), ((from.0 + to.0) / 2, (from.1 + to.1) / 2));
    mouse(app, MouseEventKind::Drag(MouseButton::Left), to);
    mouse(app, MouseEventKind::Up(MouseButton::Left), to);
}

fn click(app: &mut App, at: (i32, i32)) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), at);
    mouse(app, MouseEventKind::Up(MouseButton::Left), at);
}

fn key(app: &mut App, code: KeyCode) {
    app.on_input(vec![InputEvent::Key(crate::input::KeyEvent::new(code, Modifiers::NONE))]);
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn todo<'a>(app: &'a App, id: &str) -> &'a canvas_model::todos::Todo {
    book(app).and_then(|b| b.todo(id)).expect("the todo")
}

fn card(app: &App, id: &str) -> Cells {
    app.tasks.scene.card(id).map(|c| c.rect).unwrap_or_else(|| panic!("{id} is drawn"))
}

fn scene_at(book: &TodoBook, zoom: u8) -> layout::Scene {
    let groups: BTreeMap<String, String> = [("g-demo".to_owned(), "Canvas project".to_owned())].into();
    let view = View { area: Rect::new(0, 0, 400, 80), pan: (0, 0), zoom };
    layout::layout(book, &Input { view, filter: Filter::default(), groups: &groups, moving: None })
}

#[test]
fn type_glyphs_are_one_cell_wide_and_distinct() {
    let glyphs: Vec<&str> = TodoType::ALL.iter().map(|k| type_glyph(*k)).collect();
    assert_eq!(glyphs, ["✱", "✚", "▲", "»", "∷", "¶", "?"]);
    for glyph in glyphs.iter().chain(["◢", "◫", "✎", "☑", "‼", "⚑", "⟳", "◇", "◎", "⚠", "▦", "▔", "✓"].iter()) {
        assert_eq!(crate::paint::text_width(glyph), 1, "{glyph}");
    }
    let colors: std::collections::HashSet<String> = TodoType::ALL.iter().map(|k| format!("{:?}", type_color(*k))).collect();
    assert_eq!(colors.len(), TodoType::ALL.len());
}

#[test]
fn frames_keep_their_places_at_every_zoom_and_only_their_detail_changes() {
    let book = demo_book();
    let (full, half, quarter) = (scene_at(&book, 0), scene_at(&book, 1), scene_at(&book, 2));
    let rects = |s: &layout::Scene| s.frames.iter().map(|f| (f.board.clone(), f.rect)).collect::<Vec<_>>();
    assert_eq!(rects(&full), [("canvas-app".to_owned(), Cells::new(0, 0, 150, 40)), ("decomps".to_owned(), Cells::new(156, 0, 150, 40))]);
    assert_eq!(rects(&half), [("canvas-app".to_owned(), Cells::new(0, 0, 75, 20)), ("decomps".to_owned(), Cells::new(78, 0, 75, 20))]);
    assert_eq!(rects(&quarter), [("canvas-app".to_owned(), Cells::new(0, 0, 38, 10)), ("decomps".to_owned(), Cells::new(39, 0, 38, 10))]);
    // 100 %: the columns fill the frame, Done half as wide; full cards in their columns.
    let f = &full.frames[0];
    assert_eq!(f.columns.iter().map(|c| c.status).collect::<Vec<_>>(), [Status::Proposed, Status::Backlog, Status::Todo, Status::InProgress, Status::InReview, Status::Done]);
    assert_eq!(f.columns.iter().map(|c| c.w).sum::<i32>(), f.rect.w - 2);
    assert!(f.columns.last().unwrap().w < f.columns[0].w);
    let in_progress = f.columns.iter().find(|c| c.status == Status::InProgress).unwrap();
    assert_eq!((in_progress.count, in_progress.wip, in_progress.over), (1, Some(2), false));
    for card in &f.cards {
        let column = f.columns.iter().find(|c| c.status == card.status).unwrap();
        assert_eq!((card.rect.x, card.rect.w, card.rect.h), (column.x + 1, column.w - 2, layout::CARD_H), "{}", card.id);
        assert!(card.rect.y >= f.rect.y + 2 && card.rect.bottom() < f.rect.bottom());
    }
    assert!(f.cards.iter().all(|c| c.status != Status::Done), "Done is collapsed");
    assert_eq!(f.done.iter().map(|d| d.2).collect::<Vec<_>>(), [1]);
    assert_eq!(f.chips.iter().map(|(_, text, chip)| (text.as_str(), chip.clone())).collect::<Vec<_>>(), [(" ◫ Canvas project ", Chip::Group("g-demo".into())), (" ✎ + ", Chip::NoNotes)]);
    // Decomps: two swimlanes with headers, the canceled count in the title row.
    let d = &full.frames[1];
    assert_eq!(d.lanes.iter().map(|l| (l.board.as_str(), l.head)).collect::<Vec<_>>(), [("decomps", true), ("decomps-parser", true)]);
    let parser = &d.lanes[1];
    let dec5 = d.cards.iter().find(|c| c.id == "DEC-005").unwrap();
    assert_eq!(dec5.lane, "decomps-parser");
    assert!(dec5.rect.y > parser.y && dec5.rect.bottom() <= parser.y + parser.h);
    assert_eq!(d.canceled.as_ref().map(|(_, t)| t.as_str()), Some(" 1 canceled "));
    assert_eq!(d.chips[0].2, Chip::NoGroup);
    // 50 %: the same cards, two rows each; 25 %: summaries only.
    let ids = |s: &layout::Scene| s.frames.iter().flat_map(|f| f.cards.iter().map(|c| c.id.clone())).collect::<Vec<_>>();
    assert_eq!(ids(&half), ids(&full));
    assert!(half.frames.iter().flat_map(|f| &f.cards).all(|c| c.rect.h == layout::CARD_H_HALF));
    assert!(quarter.frames.iter().all(|f| f.summary && f.cards.is_empty() && f.columns.is_empty()));
}

#[test]
fn a_full_column_keeps_a_row_for_how_many_more() {
    let mut book = book_with("g", 0, 0, 0);
    for i in 0..12 {
        book.apply(add("Board", &format!("Card {i}"), Status::Backlog), &Actor::human(), &Ctx::at(5)).unwrap();
    }
    let scene = scene_at(&book, 0);
    let f = &scene.frames[0];
    // 37 rows below the headers: 7 cards of 4 rows and gaps, then `+5 more`.
    assert_eq!(f.cards.len(), 7);
    assert_eq!(f.more.iter().map(|m| m.2).collect::<Vec<_>>(), [5]);
    assert!(f.cards.iter().all(|c| c.rect.bottom() <= f.more[0].1));
}

#[test]
fn hit_testing_finds_corners_chips_titles_cards_columns_and_the_background() {
    let book = demo_book();
    let scene = scene_at(&book, 0);
    let f = &scene.frames[0];
    let app_board = || "canvas-app".to_owned();
    assert_eq!(scene.hit(149, 39), Target::Corner(app_board()));
    let chip = f.chips[0].0;
    assert_eq!(scene.hit(chip.x, 0), Target::Chip(app_board(), Chip::Group("g-demo".into())));
    assert_eq!(scene.hit(chip.right() - 1, 0), Target::Chip(app_board(), Chip::Group("g-demo".into())));
    assert_eq!(scene.hit(chip.right(), 0), Target::Title(app_board()));
    assert_eq!(scene.hit(3, 0), Target::Title(app_board()));
    let c = f.cards.iter().find(|c| c.id == "WI-004").unwrap().rect;
    assert_eq!(scene.hit(c.x, c.y), Target::Card("WI-004".into()));
    assert_eq!(scene.hit(c.right() - 1, c.bottom() - 1), Target::Card("WI-004".into()));
    assert_eq!(scene.hit(c.right(), c.y), Target::Cell { board: app_board(), lane: app_board(), status: Status::Todo }, "the column's padding");
    let backlog = f.columns.iter().find(|c| c.status == Status::Backlog).unwrap();
    assert_eq!(scene.hit(backlog.x + 2, 37), Target::Cell { board: app_board(), lane: app_board(), status: Status::Backlog });
    assert_eq!(scene.hit(0, 20), Target::Frame(app_board()), "the border");
    assert_eq!(scene.hit(backlog.x + 2, 1), Target::Frame(app_board()), "the column headers");
    assert_eq!(scene.hit(152, 5), Target::Background, "between the frames");
    assert_eq!(scene.hit(200, 60), Target::Background);
}

/// The cards of a lane's column, top to bottom.
fn column_ids(scene: &layout::Scene, lane: &str, status: Status) -> Vec<String> {
    let mut cards: Vec<_> = scene.frames.iter().flat_map(|f| &f.cards).filter(|c| c.lane == lane && c.status == status).collect();
    cards.sort_by_key(|c| c.rect.y);
    cards.iter().map(|c| c.id.clone()).collect()
}

#[test]
fn dropping_a_card_moves_it_with_a_rank_between_its_new_neighbours() {
    let mut book = demo_book();
    let scene = scene_at(&book, 0);
    let todo_ids = column_ids(&scene, "canvas-app", Status::Todo);
    assert_eq!(todo_ids.len(), 2);
    let (first, second) = (scene.card(&todo_ids[0]).unwrap().rect, scene.card(&todo_ids[1]).unwrap().rect);
    let rank = |id: &str, book: &TodoBook| book.todo(id).unwrap().rank.clone();
    let moved = |op: Option<TodoOp>| match op {
        Some(TodoOp::Move { id, status, board, rank: Some(rank), after: None, before: None }) => (id, status, board, rank),
        other => panic!("a move with a rank: {other:?}"),
    };
    // WI-007 (Backlog) onto the top of Todo, between the two cards, below them.
    let x = first.x + 3;
    let top = scene.drop_at(x, first.y, "WI-007").unwrap();
    assert_eq!((top.status, top.after.as_deref(), top.before.as_deref()), (Some(Status::Todo), None, Some(todo_ids[0].as_str())));
    let (id, status, board, r) = moved(layout::move_op(&book, "WI-007", &top));
    assert_eq!((id.as_str(), status, board), ("WI-007", Some(Status::Todo), None));
    assert!(r < rank(&todo_ids[0], &book));
    let (_, _, _, r) = moved(layout::move_op(&book, "WI-007", &scene.drop_at(x, first.bottom(), "WI-007").unwrap()));
    assert!(r > rank(&todo_ids[0], &book) && r < rank(&todo_ids[1], &book), "between them");
    let (_, _, _, r) = moved(layout::move_op(&book, "WI-007", &scene.drop_at(x, second.bottom() + 3, "WI-007").unwrap()));
    assert!(r > rank(&todo_ids[1], &book), "after the last");
    // Within its column: the other order, and none where it already is.
    let (_, status, _, r) = moved(layout::move_op(&book, &todo_ids[1], &scene.drop_at(x, first.y, &todo_ids[1]).unwrap()));
    assert!(status.is_none() && r < rank(&todo_ids[0], &book));
    assert_eq!(layout::move_op(&book, &todo_ids[0], &scene.drop_at(x, first.y, &todo_ids[0]).unwrap()), None);
    assert_eq!(layout::move_op(&book, &todo_ids[1], &scene.drop_at(x, second.y + 1, &todo_ids[1]).unwrap()), None);
    // Into another board's swimlane (same column): its board changes.
    let d = scene.frame("decomps").unwrap();
    let backlog = d.columns.iter().find(|c| c.status == Status::Backlog).unwrap();
    let lane = &d.lanes[1];
    let drop = scene.drop_at(backlog.x + 2, lane.y + lane.h - 1, "WI-007").unwrap();
    let op = layout::move_op(&book, "WI-007", &drop);
    let (_, status, board, _) = moved(op.clone());
    assert_eq!((status, board.as_deref()), (None, Some("decomps-parser")));
    book.apply(op.unwrap(), &Actor::human(), &Ctx::at(9)).unwrap();
    assert_eq!(column_ids(&scene_at(&book, 0), "decomps-parser", Status::Backlog), ["DEC-006", "WI-007"]);
    // At 25 %: onto a board's summary, keeping its column.
    let quarter = scene_at(&book, 2);
    let drop = quarter.drop_at(quarter.frames[0].rect.x + 3, 4, "DEC-006").unwrap();
    assert_eq!((drop.board.as_str(), drop.status), ("canvas-app", None));
    let (_, status, board, _) = moved(layout::move_op(&book, "DEC-006", &drop));
    assert_eq!((status, board.as_deref()), (None, Some("canvas-app")));
    assert_eq!(quarter.drop_at(500, 100, "DEC-006"), None);
}

#[test]
fn arrows_walk_cards_columns_and_lanes() {
    let book = demo_book();
    let grid = layout::grid(&book, "decomps-parser", &Filter::default()).unwrap();
    assert_eq!(grid.board, "decomps", "a swimlane's board is its parent's");
    let at = |id: &str| grid.find(id).unwrap();
    let id = |spot: Result<Spot, i32>| spot.map(|s| grid.id(&s).map(str::to_owned));
    assert_eq!(at("DEC-003"), Spot { lane: 0, column: 2, card: Some(0) });
    assert_eq!(id(layout::step(&grid, &at("DEC-003"), Dir::Down)), Ok(Some("DEC-005".into())), "the lane below");
    assert_eq!(id(layout::step(&grid, &at("DEC-005"), Dir::Up)), Ok(Some("DEC-003".into())));
    assert_eq!(id(layout::step(&grid, &at("DEC-003"), Dir::Left)), Ok(Some("DEC-004".into())));
    assert_eq!(id(layout::step(&grid, &at("DEC-005"), Dir::Down)), Ok(Some("DEC-005".into())), "the last lane");
    let review = layout::step(&grid, &at("DEC-002"), Dir::Right).unwrap();
    assert_eq!((review.column, review.card), (4, None), "an empty column");
    let done = layout::step(&grid, &review, Dir::Right).unwrap();
    assert_eq!((done.column, done.card), (5, None), "Done shows no cards");
    assert_eq!(layout::step(&grid, &done, Dir::Right), Err(1));
    assert_eq!(layout::step(&grid, &at("DEC-007"), Dir::Left), Err(-1));
    let filtered = layout::grid(&book, "decomps", &Filter { text: "", kind: Some(TodoType::Bug) }).unwrap();
    assert_eq!(filtered.cells.iter().flatten().flatten().collect::<Vec<_>>(), ["DEC-003"]);
}

#[test]
fn group_chips_and_session_badges_come_from_the_book() {
    let mut rig = rig(crate::demo::demo_doc("."), false);
    let node = |app: &App, id: &str| app.doc.node(id).unwrap().clone();
    assert_eq!(group_counts(&rig.app, "g-demo"), None, "no book from the service yet");
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-main")), None);
    rig.app.bus.todos = Some(demo_book());
    assert_eq!(group_counts(&rig.app, "g-demo"), Some(TodoCounts { open: 5, in_progress: 1, proposed: 1 }));
    assert_eq!(group_counts(&rig.app, "elsewhere"), None, "no board linked: the chip offers one");
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-main")).as_deref(), Some("WI-002"));
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-review")).as_deref(), Some("WI-003"), "in review, still claimed");
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-sub")), None);
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-main:explore")), None, "a subagent's box");
    let group = rig.app.doc.group("g-demo").unwrap().clone();
    let chips = rig.app.group_chips(&group, false);
    let chip = chips.iter().find(|c| matches!(c.action, crate::dialogs::Action::Tasks(_))).unwrap();
    assert_eq!((chip.label.trim(), chip.attention), ("☑ 5 · 1▸ · 1◇", true));
    // Done: no longer counted, and the badge goes.
    book::apply(&mut rig.app, TodoOp::Done { id: "WI-002".into(), note: None });
    assert_eq!(group_counts(&rig.app, "g-demo"), Some(TodoCounts { open: 4, in_progress: 0, proposed: 1 }));
    assert_eq!(session_badge(&rig.app, &node(&rig.app, "demo-main")), None);
    // Demo mode shows the demo book without a service.
    let demo = super::tests::rig(crate::demo::demo_doc("."), true);
    assert_eq!(group_counts(&demo.app, "g-demo"), Some(TodoCounts { open: 5, in_progress: 1, proposed: 1 }));
}

#[test]
fn stale_claims_are_those_of_sessions_gone_or_stopped() {
    let mut rig = rig(crate::demo::demo_doc("."), false);
    rig.app.bus.todos = Some(demo_book());
    let stale = |app: &App| -> Vec<String> {
        side::rows(app, book(app).unwrap()).iter().filter_map(|r| if let side::Row::Todo { id, mark: "⚠", .. } = r { Some(id.clone()) } else { None }).collect()
    };
    // Before the sessions are known, a box on the canvas counts as live.
    assert!(!session_gone(&rig.app, "demo-main") && stale(&rig.app).is_empty());
    assert!(session_gone(&rig.app, "not-on-the-canvas"));
    // Known and stopped: stale; running: live.
    rig.app.sessions_loaded = true;
    assert!(session_gone(&rig.app, "demo-main"));
    assert_eq!(stale(&rig.app), ["WI-002", "WI-003"]);
    rig.app.terms.insert("demo-main".into(), crate::protocol::TermInfo { id: "demo-main".into(), alive: true, ..Default::default() });
    assert!(!session_gone(&rig.app, "demo-main"));
    assert_eq!(stale(&rig.app), ["WI-003"]);
    // A box removed from the canvas.
    rig.app.doc.nodes.retain(|n| n.id != "demo-main");
    assert_eq!(stale(&rig.app), ["WI-002", "WI-003"]);
    let ctx = book::ctx(&rig.app);
    assert!(ctx.live.as_ref().is_some_and(|live| !live.contains("demo-main") && !live.contains("demo-review")));
}

#[test]
fn the_sidebar_lists_what_needs_you_ready_claims_and_boards() {
    let mut rig = demo();
    let rows = side::rows(&rig.app, book(&rig.app).unwrap());
    let heads: Vec<(String, usize)> = rows.iter().filter_map(|r| if let side::Row::Head { text, count, .. } = r { Some((text.clone(), *count)) } else { None }).collect();
    assert_eq!(heads, [("NEEDS YOU".to_owned(), 3), ("READY".to_owned(), 3), ("CLAIMED BY SESSIONS".to_owned(), 2), ("BOARDS".to_owned(), 2)]);
    let todos: Vec<&str> = rows.iter().filter_map(|r| if let side::Row::Todo { id, .. } = r { Some(id.as_str()) } else { None }).collect();
    assert_eq!(&todos[..3], ["WI-006", "DEC-007", "WI-003"], "proposals, then in review");
    assert!(rows.contains(&side::Row::Board { id: "decomps-parser".into(), depth: 1 }) && rows.last() == Some(&side::Row::NewBoard));
    // READY follows the group viewed last: its boards only.
    rig.app.tasks.last_group = Some("g-demo".into());
    let rows = side::rows(&rig.app, book(&rig.app).unwrap());
    assert!(rows.iter().any(|r| matches!(r, side::Row::Head { text, count: 1, .. } if text == "READY · Canvas project")));
    // The filters narrow every list.
    rig.app.tasks.last_group = None;
    rig.app.tasks.kind = Some(TodoType::Bug);
    let rows = side::rows(&rig.app, book(&rig.app).unwrap());
    let todos: Vec<&str> = rows.iter().filter_map(|r| if let side::Row::Todo { id, .. } = r { Some(id.as_str()) } else { None }).collect();
    assert_eq!(todos, ["DEC-003"], "the one bug that is ready (WI-005 is blocked)");
    rig.app.tasks.kind = None;
    // A click on a row selects the todo, shows its details and its card.
    rig.app.tasks.last_group = None;
    rig.app.tasks.pan = (500, 500);
    render(&mut rig.app, W, 50);
    let (rect, _) = rig.app.layout.hits.iter().find(|(_, h)| *h == crate::dialogs::Hit::Tasks(Hit::Todo("DEC-007".into()))).cloned().expect("its row");
    click(&mut rig.app, (rect.x as i32 + 4, rect.y as i32));
    assert_eq!((rig.app.tasks.selected.as_deref(), rig.app.right_detail.clone()), (Some("DEC-007"), Some(RightDetail::Todo("DEC-007".into()))));
    assert_eq!(rig.app.focus, Focus::Sidebar);
    render(&mut rig.app, W, 50);
    let area = rig.app.layout.canvas;
    let r = card(&rig.app, "DEC-007");
    assert!(r.x >= area.x as i32 && r.right() <= area.right() as i32 && r.y >= area.y as i32, "panned to its card: {r:?} in {area:?}");
    // Keys: down, Enter.
    key(&mut rig.app, KeyCode::Down);
    key(&mut rig.app, KeyCode::Enter);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("WI-003"));
}

#[test]
fn dragging_a_card_moves_it_and_sends_the_move() {
    let mut rig = served();
    rig.app.tasks.pan = (-2, -1);
    render(&mut rig.app, W, 50);
    let from = card(&rig.app, "WI-007");
    let backlog_rank = todo(&rig.app, "WI-007").rank.clone();
    let target = rig.app.tasks.scene.frames[0].columns.iter().find(|c| c.status == Status::Todo).cloned().unwrap();
    drag(&mut rig.app, (from.x + 2, from.y + 1), (target.x + 3, 40));
    assert!(rig.app.tasks.press.is_none() && rig.app.drag.is_none());
    let moved = todo(&rig.app, "WI-007");
    assert_eq!(moved.status, Status::Todo, "applied at once");
    assert_ne!(moved.rank, backlog_rank);
    let sent = ops(&rig.sent);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!((sent[0]["op"].as_str(), sent[0]["id"].as_str(), sent[0]["status"].as_str()), (Some("move"), Some("WI-007"), Some("todo")));
    assert_eq!(sent[0]["rank"].as_str(), Some(moved.rank.as_str()));
    // The dragged card is selected with its details; Esc closes them.
    assert_eq!((rig.app.focus, rig.app.right_detail.clone()), (Focus::Canvas, Some(RightDetail::Todo("WI-007".into()))));
    key(&mut rig.app, KeyCode::Esc);
    assert_eq!((rig.app.tasks.selected.clone(), rig.app.right_detail.clone()), (None, None));
    // A click (no drag) selects and shows the details; nothing is sent.
    render(&mut rig.app, W, 50);
    let r = card(&rig.app, "WI-004");
    click(&mut rig.app, (r.x + 2, r.y + 1));
    assert_eq!((rig.app.tasks.selected.as_deref(), rig.app.right_detail.clone()), (Some("WI-004"), Some(RightDetail::Todo("WI-004".into()))));
    assert!(ops(&rig.sent).is_empty());
    // Dropped on Proposed: refused here, so nothing is sent.
    render(&mut rig.app, W, 50);
    let r = card(&rig.app, "WI-004");
    let proposed = rig.app.tasks.scene.frames[0].columns[0].clone();
    assert_eq!(proposed.status, Status::Proposed);
    drag(&mut rig.app, (r.x + 2, r.y + 1), (proposed.x + 3, 30));
    assert_eq!(todo(&rig.app, "WI-004").status, Status::Todo);
    assert!(rig.app.notice.as_ref().is_some_and(|n| n.error), "why not");
    assert!(ops(&rig.sent).is_empty());
}

#[test]
fn boards_move_by_their_title_resize_by_their_corner_and_the_background_pans() {
    let mut rig = served();
    rig.app.tasks.pan = (-2, -1);
    rig.app.tasks.zoom = 1;
    render(&mut rig.app, W, 50);
    let frame = rig.app.tasks.scene.frames[0].clone();
    // The title, at 50 %: one cell is two world cells.
    drag(&mut rig.app, (frame.rect.x + 3, frame.rect.y), (frame.rect.x + 8, frame.rect.y + 2));
    let board = book(&rig.app).unwrap().board("canvas-app").unwrap().clone();
    assert_eq!((board.x, board.y, board.w), (10, 4, 150));
    let sent = ops(&rig.sent);
    assert_eq!((sent[0]["op"].as_str(), sent[0]["x"].as_i64(), sent[0]["y"].as_i64()), (Some("move_board"), Some(10), Some(4)));
    render(&mut rig.app, W, 50);
    // The corner, at 25 % (four world cells a cell).
    rig.app.tasks.zoom = 2;
    render(&mut rig.app, W, 50);
    let frame = rig.app.tasks.scene.frames[0].clone();
    assert!(crate::paint::rect_contains(rig.app.layout.canvas, frame.corner.0 as u16, frame.corner.1 as u16));
    drag(&mut rig.app, frame.corner, (frame.corner.0 - 5, frame.corner.1 - 3));
    let board = book(&rig.app).unwrap().board("canvas-app").unwrap().clone();
    assert_eq!((board.x, board.y, board.w, board.h), (10, 4, 130, 28));
    // The background pans (no operation).
    let _ = ops(&rig.sent);
    render(&mut rig.app, W, 50);
    let pan = rig.app.tasks.pan;
    let area = rig.app.layout.canvas;
    let empty = (area.right() as i32 - 3, area.bottom() as i32 - 4);
    assert_eq!(rig.app.tasks.scene.hit(empty.0, empty.1), Target::Background);
    drag(&mut rig.app, empty, (empty.0 - 4, empty.1 - 1));
    assert_eq!(rig.app.tasks.pan, (pan.0 + 16, pan.1 + 4));
    assert!(ops(&rig.sent).is_empty());
    // The wheel pans too; Alt+wheel zooms around the middle.
    mouse(&mut rig.app, MouseEventKind::ScrollDown, empty);
    assert_eq!(rig.app.tasks.pan, (pan.0 + 16, pan.1 + 16));
    rig.app.on_input(vec![InputEvent::Mouse(crate::input::MouseEvent::at_cell(MouseEventKind::ScrollUp, empty.0 as u16, empty.1 as u16, Modifiers::ALT))]);
    assert_eq!(rig.app.tasks.zoom, 1);
}

#[test]
fn keys_select_cards_zoom_fit_and_act_on_proposals() {
    let mut rig = demo();
    rig.app.set_focus(Focus::Canvas);
    // Nothing selected: an arrow picks the first card in view.
    key(&mut rig.app, KeyCode::Down);
    let first = rig.app.tasks.selected.clone().expect("a card");
    assert_eq!(rig.app.right_detail, None, "arrows do not open the details");
    flows::select_todo(&mut rig.app, "WI-004", false, true);
    key(&mut rig.app, KeyCode::Right);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("WI-002"));
    key(&mut rig.app, KeyCode::Right);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("WI-003"));
    key(&mut rig.app, KeyCode::Right);
    assert_eq!((rig.app.tasks.selected.as_deref(), rig.app.tasks.cell.clone()), (None, Some(("canvas-app".to_owned(), Status::Done))));
    key(&mut rig.app, KeyCode::Right);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("DEC-007"), "on to the next board");
    key(&mut rig.app, KeyCode::Enter);
    assert_eq!(rig.app.right_detail, Some(RightDetail::Todo("DEC-007".into())));
    key(&mut rig.app, KeyCode::Down);
    assert_eq!((rig.app.tasks.selected.as_deref(), rig.app.tasks.cell.clone()), (None, Some(("decomps-parser".to_owned(), Status::Proposed))), "the swimlane below, empty there");
    assert_eq!(rig.app.right_detail, Some(RightDetail::Todo("DEC-007".into())), "the details stay");
    key(&mut rig.app, KeyCode::Up);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("DEC-007"));
    key(&mut rig.app, KeyCode::Char('a'));
    assert_eq!(todo(&rig.app, "DEC-007").status, Status::Backlog, "accepted");
    flows::select_todo(&mut rig.app, "WI-006", false, false);
    key(&mut rig.app, KeyCode::Char('d'));
    assert_eq!(todo(&rig.app, "WI-006").status, Status::Canceled, "declined");
    // Space peeks; Esc clears the selection.
    flows::select_todo(&mut rig.app, &first, false, false);
    key(&mut rig.app, KeyCode::Char(' '));
    assert!(matches!(rig.app.modals.last(), Some(Modal::Tasks(d)) if matches!(**d, Dialog::Peek { .. })));
    key(&mut rig.app, KeyCode::Esc);
    assert!(rig.app.modals.is_empty());
    key(&mut rig.app, KeyCode::Esc);
    assert_eq!(rig.app.tasks.selected, None);
    // Zoom and fit; Del asks first.
    key(&mut rig.app, KeyCode::Char('-'));
    key(&mut rig.app, KeyCode::Char('-'));
    assert_eq!(rig.app.tasks.zoom, 2);
    key(&mut rig.app, KeyCode::Char('+'));
    assert_eq!(rig.app.tasks.zoom, 1);
    key(&mut rig.app, KeyCode::Char('f'));
    let area = rig.app.layout.canvas;
    assert_eq!(rig.app.tasks.zoom, layout::fit(book(&rig.app).unwrap(), area).1);
    flows::select_todo(&mut rig.app, "WI-004", false, false);
    key(&mut rig.app, KeyCode::Delete);
    assert!(matches!(rig.app.modals.last(), Some(Modal::Confirm(_))));
    key(&mut rig.app, KeyCode::Left);
    key(&mut rig.app, KeyCode::Enter);
    assert_eq!(todo(&rig.app, "WI-004").status, Status::Canceled);
    assert!(rig.app.launching.is_empty() && ops(&rig.sent).is_empty(), "demo mode: local only");
}

#[test]
fn changes_apply_at_once_and_the_services_next_book_replaces_them() {
    let mut rig = served();
    let before = book(&rig.app).unwrap().clone();
    let applied = book::apply(&mut rig.app, TodoOp::Claim { id: "WI-004".into(), force: false, session: None }).expect("applied here");
    assert!(applied.message.contains("Claimed WI-004"));
    assert_eq!(todo(&rig.app, "WI-004").status, Status::InProgress);
    let sent = ops(&rig.sent);
    assert_eq!(sent, [serde_json::json!({ "op": "claim", "id": "WI-004" })]);
    // The service refused it: its error, then its book. The change is undone at the error.
    rig.app.on_service(crate::protocol::ServiceMsg::Error { id: None, message: "Todo change refused: WI-004 is claimed by another session".into() });
    assert_eq!(todo(&rig.app, "WI-004").status, Status::Todo);
    assert!(rig.app.notice.as_ref().is_some_and(|n| n.error && n.text.starts_with("Todo change refused")));
    assert!(rig.app.tasks.pending.is_empty());
    let snapshot = crate::bus::Snapshot { todos: Some(before.clone()), ..Default::default() };
    rig.app.on_bus(&serde_json::to_string(&snapshot).unwrap());
    assert_eq!(todo(&rig.app, "WI-004").status, Status::Todo);
    // Refused here (a live claim needs forcing): nothing is sent.
    rig.app.sessions_loaded = true;
    rig.app.terms.insert("demo-main".into(), crate::protocol::TermInfo { id: "demo-main".into(), alive: true, ..Default::default() });
    assert!(book::apply(&mut rig.app, TodoOp::Claim { id: "WI-002".into(), force: false, session: None }).is_none());
    assert!(rig.app.notice.as_ref().is_some_and(|n| n.error && n.text.contains("force")));
    assert!(ops(&rig.sent).is_empty());
    // Claim for… you takes it over.
    flows::run(&mut rig.app, Action::ClaimFor("WI-002".into()));
    let Some(Modal::Menu(menu)) = rig.app.modals.pop() else { panic!("a picker") };
    assert_eq!(menu.items[0].label, "You");
    let holder = menu.items.iter().find(|i| i.hint == "has it").expect("the session holding it");
    assert!(!holder.enabled && holder.label.starts_with("◫ "), "{holder:?}");
    rig.app.run(menu.items[0].action.clone());
    assert_eq!(todo(&rig.app, "WI-002").claimed_by.as_ref().map(|c| c.by.clone()), Some(Actor::human()));
    assert_eq!(ops(&rig.sent)[0]["force"], true);
    // Claim for… a session: its claim (not a link), sent with the session's id; its box shows the badge.
    flows::run(&mut rig.app, Action::ClaimFor("WI-002".into()));
    let Some(Modal::Menu(menu)) = rig.app.modals.pop() else { panic!("a picker") };
    let review = menu.items.iter().find(|i| i.label.contains("Review implementation")).expect("a session of the canvas").clone();
    assert!(review.enabled);
    rig.app.run(review.action);
    let claim = todo(&rig.app, "WI-002").claimed_by.clone().expect("claimed");
    assert_eq!((claim.by.session.as_deref(), claim.by.name.as_deref()), (Some("demo-review"), Some("Review implementation")));
    assert!(todo(&rig.app, "WI-002").links.is_empty(), "no link workaround");
    assert_eq!(ops(&rig.sent), [serde_json::json!({ "op": "claim", "id": "WI-002", "force": true, "session": "demo-review" })]);
    let node = rig.app.doc.node("demo-review").cloned().unwrap();
    assert_eq!(session_badge(&rig.app, &node).as_deref(), Some("WI-002"));
    assert!(rig.app.launching.is_empty(), "assigning never starts the session");
    // No book yet: a notice, nothing sent.
    rig.app.bus.todos = None;
    assert!(book::apply(&mut rig.app, TodoOp::Release { id: "WI-002".into() }).is_none());
    assert!(ops(&rig.sent).is_empty());
}

#[test]
fn every_glyph_of_the_space_is_one_cell_wide() {
    // A wide glyph (☰ is two cells since Unicode 16; Windows' console draws it in one) shifts the rest of its row in a terminal.
    let wide = |buf: &Buffer| -> Vec<String> { buf.content.iter().map(|c| c.symbol()).filter(|s| unicode_width::UnicodeWidthStr::width(*s) > 1).map(str::to_owned).collect() };
    let mut rig = demo();
    let board = render(&mut rig.app, W, 50);
    assert!(row_text(&board, Rect::new(0, 0, W, 50)).contains("Canvas App"), "the board's summary is drawn");
    assert_eq!(wide(&board), Vec::<String>::new(), "the board summary");
    flows::show_todo(&mut rig.app, "WI-002", true);
    assert_eq!(wide(&render(&mut rig.app, W, 50)), Vec::<String>::new(), "a todo's details");
}

#[test]
fn demo_changes_outlive_the_services_snapshots() {
    let mut rig = demo();
    assert!(book::apply(&mut rig.app, TodoOp::Accept { id: "WI-006".into(), status: None }).is_some());
    assert!(ops(&rig.sent).is_empty(), "demo mode sends nothing");
    // The service routes no canvas in demo mode: its snapshots carry no book.
    rig.app.on_bus(&serde_json::to_string(&crate::bus::Snapshot::default()).unwrap());
    assert_eq!(todo(&rig.app, "WI-006").status, Status::Backlog, "the accepted proposal stays accepted");
}

#[test]
fn n_opens_the_new_todo_dialog_for_the_selected_column() {
    let mut rig = served();
    rig.app.set_focus(Focus::Canvas);
    flows::select_todo(&mut rig.app, "DEC-005", false, false);
    key(&mut rig.app, KeyCode::Char('n'));
    match rig.app.modals.last() {
        Some(Modal::Tasks(d)) => match &**d {
            Dialog::NewTodo(form) => assert_eq!((form.lane.as_str(), form.status), ("decomps-parser", Status::Todo)),
            other => panic!("{other:?}"),
        },
        _ => panic!("the dialog"),
    }
    let buf = render(&mut rig.app, W, 50);
    let rect = rig.app.layout.modal_rect.unwrap();
    assert!(row_text(&buf, rect).contains("New todo · Parser › Todo"));
    // Empty title: kept open with why.
    key(&mut rig.app, KeyCode::Enter);
    assert!(matches!(rig.app.modals.last(), Some(Modal::Tasks(_))));
    typed(&mut rig.app, "Fuzz the parser");
    key(&mut rig.app, KeyCode::Tab);
    key(&mut rig.app, KeyCode::Left);
    key(&mut rig.app, KeyCode::Tab);
    key(&mut rig.app, KeyCode::Right);
    key(&mut rig.app, KeyCode::Enter);
    assert!(rig.app.modals.is_empty());
    let made = todo(&rig.app, "DEC-009");
    assert_eq!((made.title.as_str(), made.kind, made.priority, made.status, made.board.as_str()), ("Fuzz the parser", TodoType::Bug, canvas_model::todos::Priority::Urgent, Status::Todo, "decomps-parser"));
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("DEC-009"));
    let sent = ops(&rig.sent);
    assert_eq!((sent[0]["op"].as_str(), sent[0]["title"].as_str(), sent[0]["type"].as_str(), sent[0]["status"].as_str()), (Some("add"), Some("Fuzz the parser"), Some("bug"), Some("todo")));
    // From a Done column, a new todo goes to Backlog.
    rig.app.tasks.selected = None;
    rig.app.tasks.cell = Some(("canvas-app".into(), Status::Done));
    key(&mut rig.app, KeyCode::Char('N'));
    assert!(matches!(rig.app.modals.last(), Some(Modal::Tasks(d)) if matches!(&**d, Dialog::NewTodo(f) if f.status == Status::Backlog)));
}

#[test]
fn a_groups_todo_chip_links_boards_or_creates_one() {
    let mut doc = crate::demo::demo_doc(".");
    doc.groups.push(crate::doc::Group { id: "g-docs".into(), name: "Docs".into(), x: 0, y: 30, w: 40, h: 10, ..Default::default() });
    let mut rig = rig(doc, false);
    rig.app.bus.todos = Some(demo_book());
    // A group without a board: Open (its chip) offers to link one.
    rig.app.run(crate::dialogs::Action::Tasks(Action::OpenGroup("g-docs".into())));
    assert_eq!(rig.app.space, Space::Canvas, "the dialog shows where you are");
    let Some(Modal::Tasks(dialog)) = rig.app.modals.last() else { panic!("the dialog") };
    let Dialog::Checks(checks) = &**dialog else { panic!("checkboxes") };
    assert_eq!(checks.items.iter().map(|i| (i.1.as_str(), i.3)).collect::<Vec<_>>(), [("Canvas App · WI", false), ("Decomps · DEC", false), ("Decomps › Parser", false)]);
    let buf = render(&mut rig.app, W, 50);
    let rect = rig.app.layout.modal_rect.unwrap();
    let text = row_text(&buf, rect);
    assert!(text.contains("Todo boards of “Docs”") && text.contains("[ ] Decomps · DEC") && text.contains("+ New board for this group"), "{text}");
    // Tick Decomps (a click), save.
    let (row, _) = rig.app.layout.hits.iter().find(|(_, h)| *h == crate::dialogs::Hit::ListRow(1)).cloned().unwrap();
    click(&mut rig.app, (row.x as i32 + 2, row.y as i32));
    key(&mut rig.app, KeyCode::Enter);
    assert!(rig.app.modals.is_empty());
    assert_eq!(book(&rig.app).unwrap().board("decomps").unwrap().canvas_groups, ["g-docs"]);
    assert_eq!(ops(&rig.sent), [serde_json::json!({ "op": "link_board", "board": "decomps", "group": "g-docs" })]);
    assert!(group_counts(&rig.app, "g-docs").is_some());
    // Now Open goes to the board in Tasks.
    rig.app.run(crate::dialogs::Action::Tasks(Action::OpenGroup("g-docs".into())));
    assert_eq!((rig.app.space, rig.app.tasks.board.as_deref()), (Space::Tasks, Some("decomps")));
    // `+ New board for this group`: a board named after it, linked to it.
    rig.app.run(crate::dialogs::Action::Tasks(Action::LinkBoards("g-ideas".into())));
    assert!(rig.app.modals.is_empty(), "no such group");
    rig.app.doc.groups.push(crate::doc::Group { id: "g-ideas".into(), name: "Ideas".into(), x: 50, y: 30, w: 40, h: 10, ..Default::default() });
    rig.app.run(crate::dialogs::Action::Tasks(Action::LinkBoards("g-ideas".into())));
    key(&mut rig.app, KeyCode::End);
    for _ in 0..3 {
        key(&mut rig.app, KeyCode::Down);
    }
    key(&mut rig.app, KeyCode::Enter);
    let made = book(&rig.app).unwrap().boards.iter().find(|b| b.name == "Ideas").cloned().expect("created");
    assert_eq!((made.canvas_groups.clone(), made.prefix.as_str()), (vec!["g-ideas".to_owned()], "IDE"));
    assert_eq!(rig.app.tasks.board.as_deref(), Some(made.id.as_str()));
    assert!(rig.app.launching.is_empty());
}

#[test]
fn starting_a_session_on_a_todo_opens_the_new_session_dialog_prefilled() {
    let mut rig = served();
    rig.app.doc.groups[0].h = 40;
    flows::run(&mut rig.app, Action::StartSession("WI-004".into()));
    let Some(Modal::NewSession(dialog)) = rig.app.modals.last() else { panic!("the new-session dialog") };
    assert_eq!(dialog.prompt.text, "Work on WI-004 (Docs for the Tasks space). Run `atelier todo show WI-004`, then `atelier todo claim WI-004`.");
    assert_eq!(dialog.name.text, "WI-004 Docs for the Tasks space");
    assert_eq!(dialog.group.as_deref(), Some("g-demo"), "the board's linked group");
    assert_eq!(dialog.at.map(|(x, y)| rig.app.doc.group_at(x, y).map(|g| g.id.clone())), Some(Some("g-demo".into())));
    assert!(rig.app.launching.is_empty() && !rig.sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "nothing starts before the dialog's Create");
    // A board without a group: no group.
    rig.app.modals.clear();
    flows::run(&mut rig.app, Action::StartSession("DEC-003".into()));
    let Some(Modal::NewSession(dialog)) = rig.app.modals.last() else { panic!("the new-session dialog") };
    assert!(dialog.prompt.text.starts_with("Work on DEC-003 (Crash when a chunk header is truncated)."));
}

#[test]
fn start_on_vm_shows_only_with_a_vm_pack_and_prefills_run_on_vm() {
    let mut rig = served();
    rig.app.doc.groups[0].h = 40;
    flows::show_todo(&mut rig.app, "WI-004", true);
    let text = row_text(&render(&mut rig.app, W, 50), rig.app.layout.terms);
    assert!(!text.contains("Start on VM"), "no VM pack: nothing about VMs\n{text}");
    let repo = canvas_model::vm::RepoSummary { key: "contoso-app".into(), ..Default::default() };
    rig.app.bus.vm = Some(canvas_model::vm::VmView { pack: canvas_model::vm::PackSummary { repos: vec![repo], sizes: vec!["S".into()], ..Default::default() }, ..Default::default() });
    let text = row_text(&render(&mut rig.app, W, 50), rig.app.layout.terms);
    assert!(text.contains("Start on VM…"), "{text}");
    flows::run(&mut rig.app, Action::StartOnVm("WI-004".into()));
    let Some(Modal::NewSession(dialog)) = rig.app.modals.last() else { panic!("the new-session dialog") };
    let vm = dialog.vm.as_ref().expect("Run on");
    assert!(vm.on_vm && vm.repo == "contoso-app");
    assert_eq!(vm.branch.text, "atelier/WI-004-docs-for-the-tasks-space");
    assert_eq!(dialog.name.text, "WI-004 Docs for the Tasks space");
    assert!(dialog.prompt.text.contains("`atelier todo prime`") && dialog.prompt.text.contains("`atelier todo submit WI-004 --pr URL`"), "{}", dialog.prompt.text);
    assert!(rig.app.launching.is_empty() && !rig.sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "nothing starts before the dialog's Create");
    // Start a session on this… stays on this machine.
    rig.app.modals.clear();
    flows::run(&mut rig.app, Action::StartSession("WI-004".into()));
    let Some(Modal::NewSession(dialog)) = rig.app.modals.last() else { panic!("the new-session dialog") };
    assert!(!dialog.on_vm());
}

#[test]
fn a_boards_start_on_vm_policy_makes_start_a_session_run_on_a_vm() {
    let mut rig = served();
    let repo = canvas_model::vm::RepoSummary { key: "contoso-app".into(), ..Default::default() };
    let board = book(&rig.app).and_then(|b| b.todo("WI-004").and_then(|t| b.root(&t.board)).map(|b| b.id.clone())).unwrap();
    flows::board_menu(&mut rig.app, &board, (1, 1));
    let Some(Modal::Menu(menu)) = rig.app.modals.pop() else { panic!("the board's menu") };
    assert!(!menu.items.iter().any(|i| i.label.starts_with("Sessions start on")), "no VM pack: nothing about VMs");
    rig.app.bus.vm = Some(canvas_model::vm::VmView { pack: canvas_model::vm::PackSummary { repos: vec![repo], sizes: vec!["S".into()], ..Default::default() }, ..Default::default() });
    flows::board_menu(&mut rig.app, &board, (1, 1));
    let Some(Modal::Menu(menu)) = rig.app.modals.pop() else { panic!("the board's menu") };
    let switch = menu.items.iter().find(|i| i.label == "Sessions start on: this machine").expect("the policy row").action.clone();
    rig.app.modals.clear();
    rig.app.run(switch);
    assert!(book(&rig.app).and_then(|b| b.board(&board)).is_some_and(|b| b.policy.start_on_vm), "applied at once");
    assert!(ops(&rig.sent).iter().any(|o| o["op"] == "update_board" && o["policy"]["start_on_vm"] == true), "and sent");
    flows::run(&mut rig.app, Action::StartSession("WI-004".into()));
    let Some(Modal::NewSession(dialog)) = rig.app.modals.last() else { panic!("the new-session dialog") };
    assert!(dialog.on_vm(), "the board's policy");
    assert!(dialog.prompt.text.contains("atelier todo submit"));
}

#[test]
fn the_details_show_a_todos_fields_and_their_buttons_act() {
    let mut rig = served();
    flows::show_todo(&mut rig.app, "WI-002", true);
    let buf = render(&mut rig.app, W, 50);
    let pane = rig.app.layout.terms;
    let text = row_text(&buf, pane);
    for part in ["✚ WI-002 · feature", "Gentle momentum when panning", "Status   In progress ▾", "Priority !! high ▾", "Board    Canvas App", "Claimed  Build session canvas", "CHECKLIST 2/4", "☑ Velocity from the last 60 ms", "☐ Stop on click", "BODY", "A flick keeps the canvas gliding", "LOG", "Glide works;"] {
        assert!(text.contains(part), "{part} in\n{text}");
    }
    let hit = |app: &App, action: Action| app.layout.hits.iter().find(|(_, h)| *h == crate::dialogs::Hit::Tasks(Hit::Do(action.clone()))).map(|(r, _)| *r);
    // Tick a checklist item.
    let tick = Action::Op(Box::new(TodoOp::Check { id: "WI-002".into(), index: 2, done: true }));
    let r = hit(&rig.app, tick).expect("the item");
    click(&mut rig.app, (r.x as i32, r.y as i32));
    assert_eq!(todo(&rig.app, "WI-002").checked(), (3, 4));
    // A picker: priority.
    render(&mut rig.app, W, 50);
    let r = hit(&rig.app, Action::PickPriority("WI-002".into())).expect("the picker");
    click(&mut rig.app, (r.x as i32 + 1, r.y as i32));
    let Some(Modal::Menu(menu)) = rig.app.modals.last().cloned() else { panic!("the priorities") };
    assert_eq!(menu.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["!!! urgent", "!! high", "! medium", "low", "none"]);
    rig.app.modals.clear();
    rig.app.run(menu.items[0].action.clone());
    assert_eq!(todo(&rig.app, "WI-002").priority, canvas_model::todos::Priority::Urgent);
    // A proposal: Accept.
    flows::show_todo(&mut rig.app, "WI-006", false);
    let buf = render(&mut rig.app, W, 50);
    let text = row_text(&buf, rig.app.layout.terms);
    assert!(text.contains("PROPOSED · WHY") && text.contains("Sessions would see their group's open work"), "{text}");
    let accept = Action::Op(Box::new(TodoOp::Accept { id: "WI-006".into(), status: None }));
    let r = hit(&rig.app, accept).expect("Accept");
    click(&mut rig.app, (r.x as i32 + 1, r.y as i32));
    assert_eq!(todo(&rig.app, "WI-006").status, Status::Backlog);
    // A relation: its todo.
    flows::show_todo(&mut rig.app, "DEC-004", false);
    render(&mut rig.app, W, 50);
    let (r, _) = rig.app.layout.hits.iter().find(|(_, h)| *h == crate::dialogs::Hit::Tasks(Hit::Todo("DEC-003".into()))).cloned().expect("blocked by DEC-003");
    click(&mut rig.app, (r.x as i32, r.y as i32));
    assert_eq!(rig.app.right_detail, Some(RightDetail::Todo("DEC-003".into())));
    // A pull request asks before the browser; a session is only viewed.
    flows::run(&mut rig.app, Action::OpenUrl("https://github.com/TomND/copilot-canvas/pull/42".into()));
    assert!(matches!(rig.app.modals.last(), Some(Modal::Confirm(c)) if c.title == "Open this link?"));
    rig.app.modals.clear();
    flows::run(&mut rig.app, Action::ViewSession("demo-review".into()));
    assert_eq!(rig.app.space, Space::Canvas);
    assert!(rig.app.launching.is_empty());
    // ✕ closes: the details and the selection (else the pane would show the same todo again).
    rig.app.switch_space(Space::Tasks);
    flows::show_todo(&mut rig.app, "WI-004", false);
    render(&mut rig.app, W, 50);
    let r = hit(&rig.app, Action::CloseDetail).expect("✕");
    click(&mut rig.app, (r.x as i32 + 1, r.y as i32));
    assert_eq!((rig.app.right_detail.clone(), rig.app.tasks.selected.clone()), (None, None));
    let buf = render(&mut rig.app, W, 50);
    let text = row_text(&buf, rig.app.layout.terms);
    assert!(text.contains("▦ Canvas App") && text.contains("KEYS") && !text.contains("Docs for the Tasks space"), "{text}");
    // With nothing else there, the pane shows the selection's details (its ✕ deselects).
    flows::select_todo(&mut rig.app, "WI-004", false, false);
    let buf = render(&mut rig.app, W, 50);
    assert!(rig.app.right_detail.is_none() && row_text(&buf, rig.app.layout.terms).contains("Docs for the Tasks space"));
    let r = hit(&rig.app, Action::CloseDetail).expect("✕");
    click(&mut rig.app, (r.x as i32 + 1, r.y as i32));
    assert_eq!(rig.app.tasks.selected, None);
    let buf = render(&mut rig.app, W, 50);
    let text = row_text(&buf, rig.app.layout.terms);
    assert!(text.contains("▦ Canvas App") && text.contains("KEYS"), "{text}");
}

#[test]
fn slash_filters_the_lists_and_the_cards() {
    let mut rig = demo();
    rig.app.set_focus(Focus::Canvas);
    key(&mut rig.app, KeyCode::Char('/'));
    assert!(rig.app.tasks.filter_active && rig.app.focus == Focus::Sidebar);
    typed(&mut rig.app, "pars");
    rig.app.on_input(vec![InputEvent::Text("e".into())]);
    assert_eq!(rig.app.tasks.filter.text, "parse");
    render(&mut rig.app, W, 50);
    let cards: Vec<String> = rig.app.tasks.scene.frames.iter().flat_map(|f| f.cards.iter().map(|c| c.id.clone())).collect();
    assert_eq!(cards, ["DEC-005"], "only the matching card");
    // Enter keeps the filter and goes to the list; a press elsewhere ends typing.
    key(&mut rig.app, KeyCode::Enter);
    assert!(!rig.app.tasks.filter_active && rig.app.tasks.filter.text == "parse");
    key(&mut rig.app, KeyCode::Char('/'));
    key(&mut rig.app, KeyCode::Esc);
    assert!(!rig.app.tasks.filter_active && rig.app.tasks.filter.text.is_empty(), "Esc clears it");
    // The type filter: its chip's menu.
    render(&mut rig.app, W, 50);
    let (chip, _) = rig.app.layout.hits.iter().find(|(_, h)| *h == crate::dialogs::Hit::Tasks(Hit::Do(Action::PickTypeFilter))).cloned().unwrap();
    click(&mut rig.app, (chip.x as i32 + 1, chip.y as i32));
    let Some(Modal::Menu(menu)) = rig.app.modals.pop() else { panic!("the types") };
    assert_eq!(menu.items.len(), 1 + TodoType::ALL.len());
    rig.app.run(menu.items[1].action.clone());
    assert_eq!(rig.app.tasks.kind, Some(TodoType::Bug));
    render(&mut rig.app, W, 50);
    assert!(rig.app.tasks.scene.frames.iter().flat_map(|f| &f.cards).all(|c| todo(&rig.app, &c.id).kind == TodoType::Bug));
}

#[test]
fn the_details_follow_the_selection_and_esc_closes_them() {
    let mut rig = demo();
    rig.app.set_focus(Focus::Canvas);
    // The status bar names the history keys that work in Windows Terminal (it keeps Alt+arrows); the GUI's are Alt+arrows.
    let buf = render(&mut rig.app, W, 50);
    let status = row_text(&buf, Rect::new(0, 49, W, 1));
    assert!(status.contains("Alt+, back · Alt+. forward") && !status.contains("Alt+←"), "{status}");
    rig.app.render_mode = crate::view::RenderMode::Gpu;
    let buf = render(&mut rig.app, W, 50);
    assert!(row_text(&buf, Rect::new(0, 49, W, 1)).contains("Alt+← back · Alt+→ forward"));
    rig.app.render_mode = crate::view::RenderMode::Cells;
    flows::show_todo(&mut rig.app, "DEC-003", true);
    key(&mut rig.app, KeyCode::Left);
    assert_eq!(rig.app.right_detail, Some(RightDetail::Todo("DEC-004".into())));
    key(&mut rig.app, KeyCode::Esc);
    assert_eq!((rig.app.tasks.selected.clone(), rig.app.right_detail.clone()), (None, None));
    // Nothing selected: arrows with Ctrl pan.
    let pan = rig.app.tasks.pan;
    rig.app.on_input(vec![InputEvent::Key(crate::input::KeyEvent::new(KeyCode::Right, Modifiers::CONTROL))]);
    assert_eq!(rig.app.tasks.pan, (pan.0 + 8, pan.1));
}

#[test]
fn opening_places_shows_todos_boards_and_groups() {
    let mut rig = served();
    rig.app.tasks.pan = (900, 900);
    rig.app.navigate(Space::Tasks, NavTarget::Todo("dec-5".into()));
    assert_eq!((rig.app.tasks.selected.as_deref(), rig.app.right_detail.clone()), (Some("DEC-005"), Some(RightDetail::Todo("DEC-005".into()))));
    render(&mut rig.app, W, 50);
    let r = card(&rig.app, "DEC-005");
    let area = rig.app.layout.canvas;
    assert!(r.x >= area.x as i32 && r.right() <= area.right() as i32 && r.bottom() <= area.bottom() as i32, "in view");
    rig.app.navigate(Space::Tasks, NavTarget::Board("canvas-app".into()));
    assert_eq!(rig.app.tasks.board.as_deref(), Some("canvas-app"));
    rig.app.navigate(Space::Tasks, NavTarget::Group("g-demo".into()));
    assert_eq!((rig.app.tasks.board.as_deref(), rig.app.tasks.last_group.as_deref()), (Some("canvas-app"), Some("g-demo")));
    // A link to a todo the boards do not have stays where it is and says so.
    rig.app.navigate(Space::Tasks, NavTarget::Todo("XYZ-404".into()));
    assert!(rig.app.notice.as_ref().is_some_and(|n| n.text.contains("There is no todo XYZ-404")));
    assert_eq!(rig.app.nav.current().map(|c| c.1.clone()), Some(NavTarget::Group("g-demo".into())));
    // Details of a todo deleted meanwhile say so.
    rig.app.right_detail = Some(RightDetail::Todo("XYZ-404".into()));
    let buf = render(&mut rig.app, W, 50);
    assert!(row_text(&buf, rig.app.layout.terms).contains("not on the canvas's boards"));
    // Go to lists boards and todos.
    let items = goto_items(&rig.app);
    let board = items.iter().find(|i| i.label == "Decomps").unwrap();
    assert_eq!((board.target.clone(), board.detail.as_str(), board.glyph), (NavTarget::Board("decomps".into()), "board · DEC · 3 open", "▦"));
    let todo = items.iter().find(|i| i.label == "DEC-003 Crash when a chunk header is truncated").unwrap();
    assert_eq!((todo.target.clone(), todo.detail.as_str(), todo.space), (NavTarget::Todo("DEC-003".into()), "Decomps · Todo", Space::Tasks));
    assert!(items.iter().any(|i| i.label == "Parser" && i.detail.starts_with("swimlane of Decomps")));
}

#[test]
fn spec_notes_live_in_the_notebooks_todos_folder() {
    assert_eq!(flows::spec_note_path(r"C:\Users\me\notebook\", "DEC-031", "Fix: the loader?"), r"C:\Users\me\notebook\Todos\DEC-031 - Fix- the loader.md");
    assert_eq!(flows::spec_note_path("/home/me/notebook", "WI-002", "Glide"), "/home/me/notebook/Todos/WI-002 - Glide.md");
    assert_eq!(flows::join(r"D:\Notes", &["Todos", "Backlog.md"]), r"D:\Notes\Todos\Backlog.md");
    assert_eq!(flows::start_prompt("DEC-031", "Fix it"), "Work on DEC-031 (Fix it). Run `atelier todo show DEC-031`, then `atelier todo claim DEC-031`.");
    // A todo with its note opens it; without one, the backend is asked on a thread (here: none).
    let mut rig = served();
    book::apply(&mut rig.app, TodoOp::Update(Box::new(canvas_model::todos::TodoUpdate { id: "WI-004".into(), note: Some(r"C:\n\WI-004.md".into()), ..Default::default() })));
    flows::run(&mut rig.app, Action::SpecNote("WI-004".into()));
    assert_eq!((rig.app.space, rig.app.nav.current().map(|c| c.1.clone())), (Space::Notebook, Some(NavTarget::Note(r"C:\n\WI-004.md".into()))));
    flows::run(&mut rig.app, Action::SpecNote("WI-005".into()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !rig.app.notice.as_ref().is_some_and(|n| n.error && n.text.contains("spec note")) {
        match rig.events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(AppEvent::Run(then)) => then(&mut rig.app),
            Ok(_) => {}
            Err(_) => panic!("the thread's answer"),
        }
    }
    assert!(todo(&rig.app, "WI-005").note.is_none());
}

#[test]
fn prompts_change_boards_and_todos_and_keep_why_not() {
    let mut rig = served();
    flows::run(&mut rig.app, Action::Ask(Ask::Wip("canvas-app".into())));
    let buf = render(&mut rig.app, W, 50);
    assert!(row_text(&buf, rig.app.layout.modal_rect.unwrap()).contains("WIP limit of “Canvas App”"));
    key(&mut rig.app, KeyCode::Backspace);
    typed(&mut rig.app, "x");
    key(&mut rig.app, KeyCode::Enter);
    let Some(Modal::Tasks(d)) = rig.app.modals.last() else { panic!("kept open") };
    assert!(matches!(&**d, Dialog::Prompt(p) if p.error.as_deref() == Some("A number of todos, like 3 (0: no limit).")));
    key(&mut rig.app, KeyCode::Backspace);
    typed(&mut rig.app, "3");
    key(&mut rig.app, KeyCode::Enter);
    assert!(rig.app.modals.is_empty());
    assert_eq!(book(&rig.app).unwrap().board("canvas-app").unwrap().wip, Some(3));
    // A prefix another board has: refused, the prompt stays with the reason.
    flows::run(&mut rig.app, Action::Ask(Ask::Prefix("canvas-app".into())));
    key(&mut rig.app, KeyCode::Backspace);
    key(&mut rig.app, KeyCode::Backspace);
    typed(&mut rig.app, "dec");
    key(&mut rig.app, KeyCode::Enter);
    assert!(matches!(rig.app.modals.last(), Some(Modal::Tasks(d)) if matches!(&**d, Dialog::Prompt(p) if p.error.as_deref().is_some_and(|e| e.contains("belongs to another board")))));
    key(&mut rig.app, KeyCode::Esc);
    // A new board from the menu, then shown.
    flows::run(&mut rig.app, Action::NewBoard);
    typed(&mut rig.app, "Release");
    key(&mut rig.app, KeyCode::Enter);
    let made = book(&rig.app).unwrap().boards.iter().find(|b| b.name == "Release").map(|b| b.id.clone()).expect("created");
    assert_eq!(rig.app.tasks.board.as_deref(), Some(made.as_str()));
    // The body in the editor: Ctrl+S keeps it.
    flows::run(&mut rig.app, Action::EditBody("WI-004".into()));
    render(&mut rig.app, W, 50);
    rig.app.on_input(vec![InputEvent::Paste("Write the Tasks page.".into())]);
    rig.app.on_input(vec![InputEvent::Key(crate::input::KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL))]);
    assert_eq!(todo(&rig.app, "WI-004").body, "Write the Tasks page.");
    key(&mut rig.app, KeyCode::Esc);
    assert!(rig.app.modals.is_empty() && rig.app.tasks.body.is_none());
    let sent = ops(&rig.sent);
    assert_eq!(sent.iter().map(|o| o["op"].as_str().unwrap_or_default()).collect::<Vec<_>>(), ["update_board", "add_board", "update"]);
}

/// A bus snapshot as the service sends it, with a book.
fn snapshot(book: &TodoBook) -> String {
    serde_json::to_string(&crate::bus::Snapshot { todos: Some(book.clone()), ..Default::default() }).unwrap()
}

#[test]
fn changes_in_flight_stay_on_top_of_books_sent_before_the_service_had_them() {
    let mut rig = served();
    let before = book(&rig.app).unwrap().clone();
    let claim = || TodoOp::Claim { id: "WI-004".into(), force: false, session: None };
    let ship = || add("canvas-app", "Ship it", Status::Todo);
    // Two changes in flight: a claim, then a new todo.
    book::apply(&mut rig.app, claim()).expect("applied here");
    let made = book::apply(&mut rig.app, ship()).and_then(|a| a.created).expect("a new todo");
    let count = book(&rig.app).unwrap().todos.len();
    let shows_both = |app: &App, why: &str| {
        let b = book(app).unwrap();
        assert_eq!((b.todo("WI-004").map(|t| t.status), b.todo(&made).map(|t| t.title.as_str()), b.todos.len()), (Some(Status::InProgress), Some("Ship it"), count), "{why}");
    };
    // A bus pass that read the book before either: nothing is undone.
    rig.app.on_bus(&snapshot(&before));
    shows_both(&rig.app, "a book from before both");
    // The broadcast after the first: the second stays on top (applied once, no copy).
    let (you, ctx) = (Actor::human(), book::ctx(&rig.app));
    let mut service = before.clone();
    service.apply(claim(), &you, &ctx).unwrap();
    rig.app.on_bus(&snapshot(&service));
    shows_both(&rig.app, "a book with the first only");
    // The service's book with both: it is shown as it is.
    service.apply(ship(), &you, &ctx).unwrap();
    rig.app.on_bus(&snapshot(&service));
    shows_both(&rig.app, "the service's book with both");
    assert_eq!(book(&rig.app), Some(&service));
    // A late pass that read the book before both still undoes neither.
    rig.app.on_bus(&snapshot(&before));
    shows_both(&rig.app, "a late, older book");
    // A change the service never sends back goes after a while: its last book stands.
    let long_ago = std::time::Instant::now().checked_sub(book::PENDING + std::time::Duration::from_secs(1));
    for pending in &mut rig.app.tasks.pending {
        pending.sent = long_ago.unwrap_or(pending.sent);
    }
    if long_ago.is_some() {
        tick(&mut rig.app);
        assert!(rig.app.tasks.pending.is_empty() && rig.app.tasks.served.is_none());
        assert_eq!(book(&rig.app), Some(&before), "the service's last word");
    }
    // Demo mode keeps nothing pending (nothing is sent).
    let mut demo = demo();
    book::apply(&mut demo.app, claim()).expect("applied");
    assert!(demo.app.tasks.pending.is_empty() && ops(&demo.sent).is_empty());
}

/// The layout a fresh pass makes for the scene's view (the cache's reference).
fn fresh(app: &App) -> layout::Scene {
    let groups = book::group_names(app);
    layout::layout(book(app).unwrap(), &Input { view: app.tasks.scene.view, filter: canvas::filter(app), groups: &groups, moving: None })
}

#[test]
fn the_layout_is_kept_between_frames_and_moves_with_the_pan() {
    let mut rig = served();
    rig.app.set_focus(Focus::Canvas);
    rig.app.tasks.pan = (-2, -1);
    render(&mut rig.app, W, 50);
    let made = rig.app.tasks.layouts;
    render(&mut rig.app, W, 50);
    assert_eq!(rig.app.tasks.layouts, made, "nothing changed: the same layout");
    let area = rig.app.layout.canvas;
    let middle = (area.x as i32 + area.width as i32 / 2, area.y as i32 + area.height as i32 / 2);
    for zoom in [0, 1, 2] {
        rig.app.tasks.zoom = zoom;
        render(&mut rig.app, W, 50);
        assert_eq!(rig.app.tasks.scene, fresh(&rig.app), "zoom {zoom}");
        let made = rig.app.tasks.layouts;
        // The wheel, Shift+wheel and Ctrl+arrows pan by whole cells: the layout moves.
        mouse(&mut rig.app, MouseEventKind::ScrollDown, middle);
        rig.app.on_input(vec![InputEvent::Mouse(crate::input::MouseEvent::at_cell(MouseEventKind::ScrollUp, middle.0 as u16, middle.1 as u16, Modifiers::SHIFT))]);
        rig.app.on_input(vec![InputEvent::Key(crate::input::KeyEvent::new(KeyCode::Right, Modifiers::CONTROL))]);
        render(&mut rig.app, W, 50);
        assert_eq!(rig.app.tasks.layouts, made, "zoom {zoom}: moved, not laid out again");
        assert_eq!(rig.app.tasks.scene, fresh(&rig.app), "zoom {zoom}: the same as a fresh layout");
    }
    // A pan by a part of a cell (zooming around the middle) lays it out again.
    rig.app.tasks.pan = (rig.app.tasks.pan.0 + 1, rig.app.tasks.pan.1);
    render(&mut rig.app, W, 50);
    assert_eq!(rig.app.tasks.scene, fresh(&rig.app));
    // A change of the book does too.
    rig.app.tasks.zoom = 0;
    rig.app.tasks.pan = (-2, -1);
    render(&mut rig.app, W, 50);
    let made = rig.app.tasks.layouts;
    book::apply(&mut rig.app, TodoOp::Move { id: "WI-007".into(), status: Some(Status::Todo), board: None, after: None, before: None, rank: None });
    render(&mut rig.app, W, 50);
    assert_eq!(rig.app.tasks.layouts, made + 1);
    assert_eq!(rig.app.tasks.scene.card("WI-007").map(|c| c.status), Some(Status::Todo));
    // So does a book from the service (another canvas's: the pending move no longer applies), and the filter.
    rig.app.on_bus(&snapshot(&book_with("g", 2, 0, 0)));
    render(&mut rig.app, W, 50);
    assert!(rig.app.tasks.pending.is_empty());
    let cards = |app: &App| app.tasks.scene.frames.iter().flat_map(|f| f.cards.iter().map(|c| c.id.clone())).collect::<Vec<_>>();
    assert_eq!(cards(&rig.app), ["BRD-001", "BRD-002"]);
    rig.app.tasks.kind = Some(TodoType::Bug);
    render(&mut rig.app, W, 50);
    assert_eq!(cards(&rig.app), Vec::<String>::new(), "no bugs there");
}

#[test]
fn nothing_is_under_the_pointer_beyond_the_area_the_canvas_shows() {
    let book = demo_book();
    let groups: BTreeMap<String, String> = BTreeMap::new();
    let view = View { area: Rect::new(10, 2, 100, 30), pan: (0, 3), zoom: 0 };
    let scene = layout::layout(&book, &Input { view, filter: Filter::default(), groups: &groups, moving: None });
    let f = &scene.frames[0];
    // A card under the right pane, the frame's title row under the toolbar and its ◢ corner below the area: not there.
    let beyond = f.cards.iter().find(|c| c.rect.x >= 110).expect("a card beyond the area");
    assert_eq!(scene.hit(beyond.rect.x, beyond.rect.y), Target::Background);
    assert_eq!(scene.drop_at(beyond.rect.x, beyond.rect.y, "WI-007"), None);
    assert!(f.corner.1 >= 32 && f.rect.y < 2);
    assert_eq!(scene.hit(f.corner.0, f.corner.1), Target::Background);
    assert_eq!(scene.hit(f.rect.x + 3, f.rect.y), Target::Background, "its title row, above the area");
    // In the area, as before.
    let shown = f.cards.iter().find(|c| c.rect.y >= 2 && c.rect.right() <= 110 && c.rect.bottom() <= 32).expect("a card in view");
    assert_eq!(scene.hit(shown.rect.x, shown.rect.y), Target::Card(shown.id.clone()));
    assert!(scene.drop_at(shown.rect.x, shown.rect.y, "WI-007").is_some());
    // A card dragged and let go over the right pane (a frame goes on under it) stays.
    let mut rig = served();
    rig.app.tasks.pan = (-2, -1);
    render(&mut rig.app, W, 50);
    let area = rig.app.layout.canvas;
    let review = rig.app.tasks.scene.frames[0].columns.iter().find(|c| c.status == Status::InReview).cloned().unwrap();
    assert!(review.x > area.right() as i32, "In review is under the right pane: {review:?} {area:?}");
    let from = card(&rig.app, "WI-004");
    drag(&mut rig.app, (from.x + 2, from.y + 1), (review.x + 3, from.y + 1));
    assert_eq!(todo(&rig.app, "WI-004").status, Status::Todo);
    assert!(ops(&rig.sent).is_empty() && rig.app.tasks.press.is_none());
}

#[test]
fn a_drop_on_the_summary_of_its_own_board_keeps_a_card_in_its_swimlane() {
    let book = demo_book();
    let quarter = scene_at(&book, 2);
    let decomps = quarter.frame("decomps").unwrap().rect;
    let drop = quarter.drop_at(decomps.x + 3, decomps.y + 3, "DEC-005").unwrap();
    assert_eq!((drop.board.as_str(), drop.status), ("decomps", None));
    assert_eq!(layout::move_op(&book, "DEC-005", &drop), None, "DEC-005 stays in Parser");
}

#[test]
fn the_type_filter_stays_inside_a_narrow_sidebar() {
    let mut rig = demo();
    for width in [18, 24, 30, 40] {
        rig.app.doc.ui.sidebar_width = width;
        for kind in [None, Some(TodoType::Improvement)] {
            rig.app.tasks.kind = kind;
            let buf = render(&mut rig.app, W, 50);
            let side = rig.app.layout.sidebar;
            let row = Rect::new(side.x, side.y + 1, side.width, 1);
            assert!(row_text(&buf, row).contains('▾'), "{width}: {:?}", row_text(&buf, row));
            for (r, hit) in &rig.app.layout.hits {
                if matches!(hit, crate::dialogs::Hit::Tasks(Hit::Filter | Hit::Do(Action::PickTypeFilter))) {
                    assert!(r.x >= side.x && r.right() <= side.right() && r.width > 0, "{width} {kind:?}: {hit:?} at {r:?} beyond {side:?}");
                }
            }
        }
    }
}

#[test]
fn arrows_go_to_the_board_beside_and_bring_it_into_view() {
    // Two boards, the right one first in the book; one todo on the left one.
    let mut book = TodoBook::default();
    let (you, ctx) = (Actor::human(), Ctx::at(1_000));
    book.apply(TodoOp::AddBoard { name: "Right".into(), prefix: Some("RGT".into()), parent: None, groups: Vec::new(), x: Some(400), y: Some(0) }, &you, &ctx).unwrap();
    book.apply(TodoOp::AddBoard { name: "Left".into(), prefix: Some("LFT".into()), parent: None, groups: Vec::new(), x: Some(0), y: Some(0) }, &you, &ctx).unwrap();
    book.apply(add("Left", "Only", Status::Todo), &you, &ctx).unwrap();
    let right = book.boards.iter().find(|b| b.name == "Right").map(|b| b.id.clone()).unwrap();
    let mut rig = rig(Doc::default(), false);
    rig.app.bus.todos = Some(book);
    rig.app.switch_space(Space::Tasks);
    rig.app.set_focus(Focus::Canvas);
    render(&mut rig.app, W, 50);
    // Nothing selected: the first card that shows.
    key(&mut rig.app, KeyCode::Down);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("LFT-001"));
    // In progress, In review, Done, then the board to the right (empty there): it comes into view.
    for _ in 0..4 {
        key(&mut rig.app, KeyCode::Right);
    }
    assert_eq!((rig.app.tasks.selected.clone(), rig.app.tasks.board.as_deref(), rig.app.tasks.cell.clone()), (None, Some(right.as_str()), Some((right.clone(), Status::Backlog))));
    render(&mut rig.app, W, 50);
    let area = rig.app.layout.canvas;
    let frame = rig.app.tasks.scene.frame(&right).unwrap().rect;
    assert!(frame.x >= area.x as i32 && frame.x < area.right() as i32 && frame.y >= area.y as i32, "{frame:?} in {area:?}");
    // And back: the left board's Done column, then its card again.
    key(&mut rig.app, KeyCode::Left);
    assert_eq!(rig.app.tasks.cell.as_ref().map(|c| c.1), Some(Status::Done));
    // The first card that shows is the left board's (the right one, first in the book, is out of view).
    rig.app.on_bus(&snapshot(&{
        let mut b = super::book(&rig.app).unwrap().clone();
        b.apply(add("Right", "Out of view", Status::Todo), &you, &ctx).unwrap();
        b
    }));
    rig.app.tasks.selected = None;
    rig.app.tasks.cell = None;
    rig.app.tasks.pan = (-2, -1);
    render(&mut rig.app, W, 50);
    assert!(rig.app.tasks.scene.card("RGT-001").is_some_and(|c| c.rect.x > rig.app.layout.canvas.right() as i32));
    key(&mut rig.app, KeyCode::Down);
    assert_eq!(rig.app.tasks.selected.as_deref(), Some("LFT-001"), "not the right board's card, out of view");
}

#[test]
fn badges_and_chip_counts_match_the_book_and_follow_its_changes() {
    let mut rig = served();
    let check = |app: &App, why: &str| {
        let b = book(app).unwrap();
        for node in &app.doc.nodes {
            assert_eq!(book::session_badge(app, &node.id), b.claimed_by_session(&node.id).map(|t| t.id.clone()), "{why}: {}", node.id);
        }
        for group in app.doc.groups.iter().map(|g| g.id.clone()).chain(["elsewhere".to_owned()]) {
            assert_eq!(group_counts(app, &group), b.group_counts(&group), "{why}: {group}");
        }
    };
    check(&rig.app, "the demo book");
    book::apply(&mut rig.app, TodoOp::Claim { id: "WI-004".into(), force: true, session: Some("demo-review".into()) }).expect("assigned");
    check(&rig.app, "a newer claim");
    assert_eq!(book::session_badge(&rig.app, "demo-review").as_deref(), Some("WI-004"), "the newest claim");
    book::apply(&mut rig.app, TodoOp::Done { id: "WI-004".into(), note: None }).expect("done");
    check(&rig.app, "done: not shown");
    assert_eq!(book::session_badge(&rig.app, "demo-review").as_deref(), Some("WI-003"));
    rig.app.on_bus(&snapshot(&book_with("g-demo", 2, 1, 1)));
    check(&rig.app, "another book");
}

#[test]
fn a_sessions_todo_badge_gives_way_to_its_effect_and_its_hidden_badge() {
    use crate::paint::text_width;
    let mut rig = rig(crate::demo::demo_doc("."), false);
    rig.app.bus.todos = Some(demo_book());
    let open = crate::dialogs::Hit::Toolbar(crate::dialogs::Action::Tasks(Action::OpenTodo("WI-002".into())));
    let badge = |app: &App| app.layout.hits.iter().find(|(_, h)| *h == open).map(|(r, _)| *r);
    let buf = render(&mut rig.app, W, 50);
    let r = badge(&rig.app).expect("demo-main's badge is clickable");
    assert!(row_text(&buf, r).contains("☑ WI-002"));
    let model_badge = |app: &mut App| {
        let area = app.layout.canvas;
        crate::ui_canvas::canvas_model(app, area).boxes.iter().find(|b| b.id == "demo-main").and_then(|b| b.badge.clone())
    };
    assert_eq!(model_badge(&mut rig.app).as_deref(), Some("WI-002"));
    // An effect on the box: its badge goes where the todo badge was; a click there clears the effect (never opens Tasks).
    rig.app.bus.effects.push(crate::effects::Effect { id: "e1".into(), kind: "fireworks".into(), group: "g-demo".into(), target: "demo-main".into(), ms: 1, until_ms: u64::MAX, ..Default::default() });
    render(&mut rig.app, W, 50);
    assert_eq!(badge(&rig.app), None);
    assert_eq!(model_badge(&mut rig.app), None, "the GUI's pill waits too");
    rig.app.bus.effects.clear();
    render(&mut rig.app, W, 50);
    assert!(badge(&rig.app).is_some());
    // The `⋯ N hidden` badge on the same row: the todo badge ends before it, or gives way.
    let area = rig.app.layout.canvas;
    let mut b = crate::ui_canvas::canvas_model(&mut rig.app, area).boxes.into_iter().find(|b| b.id == "demo-main").unwrap();
    b.hidden = Some(crate::view::Hidden { sessions: 2, agents: 12, color: None });
    let hidden = b.hidden.map(|h| text_width(&format!(" {} ", h.text())) as i32).unwrap();
    for (zoom, h) in [(0u8, 5), (1, 2)] {
        let (x, _, text) = crate::ui_canvas::badge_cells(&b, (0, 0, 36, h), zoom).expect("room for both");
        assert!(x + (text_width(&text) as i32) < 36 - 2 - hidden, "zoom {zoom}: {text:?} at {x}");
    }
    assert_eq!(crate::ui_canvas::badge_cells(&b, (0, 0, 22, 5), 0), None);
    b.hidden = None;
    assert_eq!(crate::ui_canvas::badge_cells(&b, (0, 0, 36, 5), 0), Some((2, 4, " ☑ WI-002 ".to_owned())), "without it, as before");
}

#[test]
fn the_space_draws_on_the_smallest_screen_with_huge_titles_and_empty_books() {
    // Longer than the book takes from a change (a book from elsewhere).
    let huge = "A very long title ".repeat(40);
    let mut book = demo_book();
    for todo in book.todos.iter_mut().filter(|t| t.id == "WI-004") {
        todo.title = huge.clone();
    }
    for board in book.boards.iter_mut().filter(|b| b.id == "decomps") {
        board.name = huge.clone();
        board.notes = Some(huge.clone());
    }
    let dialogs = |id: &str| {
        vec![
            Dialog::Peek { id: id.into() },
            Dialog::NewTodo(NewTodoForm::new("decomps", Status::Todo)),
            Dialog::Info { title: huge.clone(), lines: vec![huge.clone()] },
            Dialog::Body { id: id.into() },
        ]
    };
    for (w, h) in [(60, 16), (61, 17), (80, 20), (120, 30)] {
        for books in [Some(book.clone()), Some(TodoBook::default()), None] {
            let mut rig = rig(crate::demo::demo_doc("."), false);
            rig.app.bus.todos = books;
            rig.app.switch_space(Space::Tasks);
            rig.app.set_focus(Focus::Canvas);
            for zoom in [0, 1, 2] {
                rig.app.tasks.zoom = zoom;
                for pan in [(-2, -1), (140, 30), (-500, -500)] {
                    rig.app.tasks.pan = pan;
                    render(&mut rig.app, w, h);
                }
            }
            rig.app.tasks.zoom = 0;
            rig.app.tasks.pan = (-2, -1);
            flows::show_todo(&mut rig.app, "WI-004", true);
            render(&mut rig.app, w, h);
            for code in [KeyCode::Down, KeyCode::Right, KeyCode::Right, KeyCode::Up, KeyCode::Left] {
                key(&mut rig.app, code);
                render(&mut rig.app, w, h);
            }
            rig.app.tasks.filter_active = true;
            rig.app.tasks.kind = Some(TodoType::Bug);
            rig.app.set_focus(Focus::Sidebar);
            render(&mut rig.app, w, h);
            rig.app.tasks.kind = None;
            rig.app.tasks.filter_active = false;
            for dialog in dialogs("WI-004") {
                if matches!(dialog, Dialog::Body { .. }) {
                    flows::run(&mut rig.app, Action::EditBody("WI-004".into()));
                } else {
                    flows::push(&mut rig.app, dialog);
                }
                render(&mut rig.app, w, h);
                rig.app.modals.clear();
            }
            rig.app.open_goto();
            render(&mut rig.app, w, h);
            rig.app.modals.clear();
            rig.app.run(crate::dialogs::Action::Help);
            render(&mut rig.app, w, h);
            rig.app.modals.clear();
        }
    }
}

#[test]
fn trackpad_gestures_pan_the_board_by_cells_scroll_the_lists_and_step_the_zoom() {
    use crate::input::{Gesture, GestureKind, GesturePhase::*};
    use std::time::{Duration, Instant};
    let mut rig = demo();
    rig.app.tasks.zoom = 1;
    render(&mut rig.app, W, 50);
    let (t0, ms) = (Instant::now(), Duration::from_millis);
    let area = rig.app.layout.canvas;
    let empty = (area.right() - 3, area.bottom() - 4);
    let at = |kind, (x, y): (u16, u16), modifiers| Gesture { kind, x: x as f32 + 0.5, y: y as f32 + 0.5, modifiers };
    let scroll = |dx, dy, phase| GestureKind::Scroll { dx, dy, phase };
    let (origin, pan, d) = (rig.app.origin_f(), rig.app.tasks.pan, layout::DIVS[1]);
    // Fingers left and up by 2.5 and 1.5 cells: the board follows them by whole cells, the halves wait.
    rig.app.on_gesture_at(at(scroll(0.0, 0.0, Began), empty, Modifiers::NONE), t0);
    rig.app.on_gesture_at(at(scroll(-1.25, -0.75, Changed), empty, Modifiers::NONE), t0 + ms(8));
    rig.app.on_gesture_at(at(scroll(-1.25, -0.75, Changed), empty, Modifiers::NONE), t0 + ms(16));
    assert_eq!(rig.app.tasks.pan, (pan.0 + 2 * d, pan.1 + d));
    rig.app.on_gesture_at(at(scroll(-0.5, -0.5, Changed), empty, Modifiers::NONE), t0 + ms(24));
    assert_eq!(rig.app.tasks.pan, (pan.0 + 3 * d, pan.1 + 2 * d), "the fractions carried over");
    rig.app.on_gesture_at(at(scroll(0.0, 0.0, Ended), empty, Modifiers::NONE), t0 + ms(32));
    assert_eq!(rig.app.origin_f(), origin, "the session canvas behind never moves");
    // Command (Ctrl after the key policy) + scroll steps the zoom a level per half doubling; so does pinching.
    rig.app.on_gesture_at(at(scroll(0.0, -8.0, Began), empty, Modifiers::CONTROL), t0 + ms(1000));
    assert_eq!(rig.app.tasks.zoom, 2, "zoomed out");
    rig.app.on_gesture_at(at(GestureKind::Pinch { delta: 0.3, phase: Began }, empty, Modifiers::NONE), t0 + ms(2000));
    assert_eq!(rig.app.tasks.zoom, 2, "not yet half a doubling");
    rig.app.on_gesture_at(at(GestureKind::Pinch { delta: 0.3, phase: Changed }, empty, Modifiers::NONE), t0 + ms(2010));
    assert_eq!(rig.app.tasks.zoom, 1, "zoomed in");
    assert_eq!(rig.app.view_scale, 1.0, "the session canvas did not zoom");
    // The sidebar scrolls row by row, latched to it.
    render(&mut rig.app, W, 50);
    let side = rig.app.layout.sidebar;
    let list = (side.x + 2, side.y + 6);
    let before = rig.app.tasks.side_scroll;
    rig.app.on_gesture_at(at(scroll(0.0, -1.5, Began), list, Modifiers::NONE), t0 + ms(3000));
    rig.app.on_gesture_at(at(scroll(0.0, -1.5, Changed), empty, Modifiers::NONE), t0 + ms(3010));
    assert_eq!(rig.app.tasks.side_scroll, before + 3);
    assert_eq!(rig.app.tasks.zoom, 1, "the board did not move");
    // Smart zoom over the board: every board in view.
    rig.app.tasks.pan = (900, 900);
    rig.app.smart_zoom_at(empty.0 as f32, empty.1 as f32, t0 + ms(4000));
    assert_eq!((rig.app.tasks.pan, rig.app.tasks.zoom), layout::fit(book(&rig.app).unwrap(), area));
    assert_eq!(rig.app.origin_f(), origin);
}
