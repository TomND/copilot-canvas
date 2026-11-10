//! ViewModel snapshot tests (PLAN.md §10, "Core"): scripted app states,
//! built in-process with a detached connection and no backend, rendered in
//! both modes. Each snapshot holds the TUI screen (`RenderMode::Cells`) as
//! text and the GPU view model (`RenderMode::Gpu`: canvas model, tiles,
//! cursor, modal) as a stable dump, plus the dialog overlay when one is open.
//!
//! Review a change in the `.snap.new` file the failure names, then accept it:
//! `UPDATE_SNAPSHOTS=1 cargo test -p canvas-core --test view_snapshots`.
//! Inputs are fixed (paths, animation tick, no clock-dependent text), so the
//! snapshots are the same on every machine.

mod snapshot;

use std::sync::Arc;

use canvas_core::agents::AgentKind;
use canvas_core::app::{App, Drag, Focus};
use canvas_core::config::{McpServer, Snapshot};
use canvas_core::dialogs::{Action, McpTarget, TextInput};
use canvas_core::doc::{Doc, Group, Node, NodeKind};
use canvas_core::protocol::{Activity, AgentStatus, AgentSummary, Run, SessionSummary, TermInfo};
use canvas_core::view::RenderMode;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;

/// The screen every state is rendered at (cells).
const SCREEN: (u16, u16) = (150, 40);
/// The folder of every session (no temporary paths in the snapshots).
const CWD: &str = r"C:\work\canvas";

/// A client on `doc` with no service and no backend, as the unit tests use.
fn app(doc: Doc, demo: bool) -> App {
    let (tx, _rx) = crossbeam_channel::bounded(64);
    let (_service, service_rx) = crossbeam_channel::bounded(1);
    let (app_tx, _app_rx) = crossbeam_channel::unbounded();
    let conn = canvas_core::conn::Conn::detached(tx, service_rx);
    let mut app = App::new(conn, Arc::new(canvas_core::rpc::Offline), canvas_core::sync::Opened::detached(doc), demo, CWD.into(), app_tx, None);
    // "The backend did not answer": there is none here.
    app.notice = None;
    app.pan_quiet_until = None;
    app.tick = 0;
    // Animations as with Windows' "Show animations" on, whatever this machine says.
    app.visual.motion = Some(canvas_core::view::Motion::On);
    app.installed_agents = vec![AgentKind::Copilot, AgentKind::OpenCode];
    app.detected_agent = AgentKind::Copilot;
    app
}

fn node(id: &str, name: &str, x: i32, y: i32) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x,
        y,
        cwd: Some(CWD.into()),
        parent: None,
        agent_key: None,
        source: None,
        command: None,
        fresh: false,
        automation: None,
        synced_name: None,
        reviewed_ms: None,
        mcp_off: None,
        passive: None,
        no_messages: false,
        agent: None,
        agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None, subagents_shown: None, unplaced: false, hidden: false, placement: None,
    }
}

fn summary(id: &str, activity: Activity, live: bool) -> SessionSummary {
    SessionSummary { id: id.into(), name: id.into(), cwd: CWD.into(), branch: "main".into(), tracked: true, live, activity, ..Default::default() }
}

fn term(id: &str, alive: bool, exit_code: Option<u32>) -> TermInfo {
    TermInfo { id: id.into(), alive, exit_code, ..Default::default() }
}

fn agent(key: &str, name: &str, status: AgentStatus) -> AgentSummary {
    AgentSummary { key: key.into(), name: name.into(), description: format!("{name} (subagent)"), status, finished_ms: None }
}

/// Agent configurations with three MCP servers (one off in the agent's config).
fn configs(app: &mut App) {
    let server = |name: &str, enabled: bool| McpServer { name: name.into(), source: "user config".into(), transport: "stdio".into(), target: format!("{name}-mcp"), enabled };
    for kind in canvas_core::agents::ALL {
        let snapshot = Snapshot { mcp: vec![server("github", true), server("playwright", true), server("filesystem", false)], ..Default::default() };
        app.agent_configs.insert(kind, snapshot);
    }
}

fn lines(buf: &Buffer) -> String {
    let area = buf.area;
    (area.y..area.bottom())
        .map(|y| (area.x..area.right()).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The non-empty part of the dialog overlay, with its position.
fn overlay_text(overlay: &Buffer) -> Option<String> {
    let area = overlay.area;
    let used: Vec<(u16, u16)> = (area.y..area.bottom()).flat_map(|y| (area.x..area.right()).map(move |x| (x, y))).filter(|&(x, y)| overlay[(x, y)] != Cell::EMPTY).collect();
    let (x0, x1) = (used.iter().map(|p| p.0).min()?, used.iter().map(|p| p.0).max()?);
    let (y0, y1) = (used.iter().map(|p| p.1).min()?, used.iter().map(|p| p.1).max()?);
    let body = (y0..=y1).map(|y| (x0..=x1).map(|x| overlay[(x, y)].symbol()).collect::<String>().trim_end().to_owned()).collect::<Vec<_>>().join("\n");
    Some(format!("## gpu overlay at {x0},{y0}\n{body}"))
}

/// The Gpu-mode screen as the GUI's test hooks show it: the chrome, with the
/// canvas model painted as characters where the chrome leaves it blank.
fn painted(chrome: &Buffer, view: &canvas_core::view::View) -> String {
    let mut grid = Buffer::empty(chrome.area);
    if let Some(model) = &view.canvas {
        canvas_core::ui_canvas::paint_model(model, &mut grid);
        for y in chrome.area.y..chrome.area.bottom() {
            for x in chrome.area.x..chrome.area.right() {
                let inside = x >= model.area.x && x < model.area.right() && y >= model.area.y && y < model.area.bottom();
                let cell = &chrome[(x, y)];
                if !inside || cell.symbol() != " " || cell.bg != ratatui::style::Color::Reset {
                    grid[(x, y)] = cell.clone();
                }
            }
        }
    }
    lines(&grid)
}

#[derive(Clone, Copy)]
struct Sections {
    /// The TUI screen (off for scales the TUI does not have).
    cells: bool,
    /// The Gpu chrome with the canvas model painted as characters.
    painted: bool,
}

const BOTH: Sections = Sections { cells: true, painted: false };

/// Renders `app` in both modes and compares the text with snapshot `name`.
#[track_caller]
fn check(name: &str, app: &mut App, sections: Sections) {
    check_sized(name, app, sections, SCREEN);
}

/// As `check`, on a screen of `size` cells.
#[track_caller]
fn check_sized(name: &str, app: &mut App, sections: Sections, size: (u16, u16)) {
    let area = Rect::new(0, 0, size.0, size.1);
    let mut out = vec![format!("# {name} ({}x{} cells)", size.0, size.1)];
    if sections.cells {
        app.render_mode = RenderMode::Cells;
        let mut buf = Buffer::empty(area);
        canvas_core::ui::render(app, &mut buf, area, None);
        out.push("## cells (the TUI screen)".into());
        out.push(lines(&buf));
    }
    app.render_mode = RenderMode::Gpu;
    let (mut chrome, mut overlay) = (Buffer::empty(area), Buffer::empty(area));
    let view = canvas_core::ui::render(app, &mut chrome, area, Some(&mut overlay));
    out.push("## gpu view model".into());
    out.push(view.dump());
    if let Some(text) = overlay_text(&overlay) {
        out.push(text);
    }
    if sections.painted {
        out.push("## gpu screen (chrome and the model painted as text)".into());
        out.push(painted(&chrome, &view));
    }
    snapshot::assert_snapshot(name, &out.join("\n"));
}

/// The demo canvas at a zoom step (Cells) and the same scale (Gpu).
fn demo_at(zoom: u8, scale: f32) -> App {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    doc.view.zoom = zoom;
    let mut app = app(doc, true);
    app.view_scale = scale;
    app
}

#[test]
fn demo_canvas_at_100_percent() {
    check("demo-100", &mut demo_at(0, 1.0), BOTH);
}

#[test]
fn demo_canvas_at_50_percent() {
    check("demo-50", &mut demo_at(1, 0.5), BOTH);
}

#[test]
fn demo_canvas_at_25_percent() {
    check("demo-25", &mut demo_at(2, 0.25), BOTH);
}

#[test]
fn demo_canvas_at_a_continuous_scale() {
    // 70 % with a fractional origin: only the GPU client has it.
    let mut app = demo_at(1, 0.7);
    app.view_frac = (0.25, 0.5);
    check("demo-70-continuous", &mut app, Sections { cells: false, painted: true });
}

/// Sessions in every status, from synthetic history summaries and terminals.
#[test]
fn every_status() {
    let names = [
        ("s-working", "Working (history)"),
        ("s-waiting", "Needs you"),
        ("s-review", "Done, review"),
        ("s-idle", "Idle"),
        ("s-stopped", "Stopped (exit 0)"),
        ("s-failed", "Failed (exit 2)"),
        ("s-elsewhere", "Open elsewhere"),
        ("s-running", "Shell running here"),
        ("s-preparing", "Preparing"),
        ("s-error", "Launch error"),
        ("s-history-failed", "Failed (history)"),
        ("s-progress", "Working (OSC 9;4)"),
    ];
    let mut doc = Doc::default();
    for (i, (id, name)) in names.iter().enumerate() {
        let mut n = node(id, name, 1 + (i as i32 % 3) * 36, 1 + (i as i32 / 3) * 8);
        n.reviewed_ms = Some(if *id == "s-review" { 1_000 } else { 50_000 });
        if *id == "s-running" || *id == "s-progress" {
            n.command = Some(vec!["powershell.exe".into()]);
        }
        doc.nodes.push(n);
    }
    doc.view.x = -1;
    doc.view.y = 0;
    let mut app = app(doc, false);
    let live = |id: &str, activity: Activity| summary(id, activity, true);
    app.sessions.insert("s-working".into(), live("s-working", Activity::Working));
    let mut waiting = live("s-waiting", Activity::Working);
    waiting.pending = 1;
    app.sessions.insert("s-waiting".into(), waiting);
    let mut review = live("s-review", Activity::Idle);
    review.finished_ms = Some(2_000);
    app.sessions.insert("s-review".into(), review);
    app.sessions.insert("s-idle".into(), live("s-idle", Activity::Idle));
    app.sessions.insert("s-elsewhere".into(), live("s-elsewhere", Activity::Unknown));
    app.sessions.insert("s-history-failed".into(), summary("s-history-failed", Activity::Failed, false));
    app.terms.insert("s-stopped".into(), term("s-stopped", false, Some(0)));
    app.terms.insert("s-failed".into(), term("s-failed", false, Some(2)));
    app.terms.insert("s-running".into(), term("s-running", true, None));
    app.terms.insert("s-progress".into(), TermInfo { progress: 3, progress_seen: true, ..term("s-progress", true, None) });
    app.preparing.insert("s-preparing".into());
    app.node_errors.insert("s-error".into(), "copilot.exe was not found".into());
    app.sessions_loaded = true;
    check("every-status", &mut app, BOTH);
}

/// A session with three subagents (working, finished, failed), a subsession
/// (dashed connector), a looping session and a queued passive message. The
/// session shows its subagent boxes (its own choice; folded is the default:
/// `subagent-decks`).
#[test]
fn subagents_subsessions_and_a_loop() {
    let mut doc = Doc::default();
    let mut orch = node("orch", "Orchestrator", 1, 1);
    orch.subagents_shown = Some(true);
    doc.nodes.push(orch);
    for (i, (key, name)) in [("explore", "Explore the API"), ("review", "Review the diff"), ("lint", "Lint the tree")].iter().enumerate() {
        let mut a = node(&format!("orch:{key}"), name, 5, 7 + i as i32 * 4);
        a.kind = NodeKind::Agent;
        a.parent = Some("orch".into());
        a.agent_key = Some((*key).into());
        doc.nodes.push(a);
    }
    let mut sub = node("follow", "Follow-up", 36, 7);
    sub.parent = Some("orch".into());
    doc.nodes.push(sub);
    doc.nodes.push(node("nightly", "Nightly check", 36, 15));
    doc.view.x = -1;
    doc.view.y = 0;
    doc.selected = Some("orch".into());
    // A narrower details panel, so every box is in view.
    doc.ui.terminal_percent = 40;
    let mut app = app(doc, false);
    let mut orch = summary("orch", Activity::Idle, true);
    orch.agents = vec![agent("explore", "Explore the API", AgentStatus::Working), agent("review", "Review the diff", AgentStatus::Finished), agent("lint", "Lint the tree", AgentStatus::Failed)];
    app.sessions.insert("orch".into(), orch);
    app.sessions.insert("follow".into(), summary("follow", Activity::Idle, true));
    let mut nightly = summary("nightly", Activity::Idle, true);
    nightly.loop_ms = 900_000;
    app.sessions.insert("nightly".into(), nightly);
    app.bus.mailbox.insert("follow".into(), 2);
    // The loop dot where a fixed clock puts it.
    app.visual.clock_ms = Some(NOW);
    check("subagents-subsessions-loop", &mut app, BOTH);
}

/// Two sessions whose subagent boxes are folded into decks (the default,
/// on a canvas saved before there was a choice): a slim "◇ N subagents" card
/// beneath each, with the most important state of what it holds (a failure
/// first; working while one runs, "1 of 2").
fn subagent_decks() -> App {
    let mut doc = Doc::default();
    let mut add = |id: &str, name: &str, x: i32, agents: &[(&str, &str)]| {
        doc.nodes.push(node(id, name, x, 1));
        for (i, (key, agent)) in agents.iter().enumerate() {
            let mut a = node(&format!("{id}:{key}"), agent, x + 4, 6 + i as i32 * 4);
            a.kind = NodeKind::Agent;
            a.parent = Some(id.into());
            a.agent_key = Some((*key).into());
            doc.nodes.push(a);
        }
    };
    add("orch", "Orchestrator", 1, &[("explore", "Explore the API"), ("review", "Review the diff"), ("lint", "Lint the tree")]);
    add("fix", "Fix the build", 36, &[("search", "Search the logs"), ("patch", "Patch the config")]);
    doc.view.x = -1;
    doc.view.y = 0;
    doc.ui.terminal_percent = 40;
    let mut app = app(doc, false);
    let mut orch = summary("orch", Activity::Idle, true);
    orch.agents = vec![agent("explore", "Explore the API", AgentStatus::Working), agent("review", "Review the diff", AgentStatus::Finished), agent("lint", "Lint the tree", AgentStatus::Failed)];
    app.sessions.insert("orch".into(), orch);
    let mut fix = summary("fix", Activity::Idle, true);
    fix.agents = vec![agent("search", "Search the logs", AgentStatus::Working), agent("patch", "Patch the config", AgentStatus::Finished)];
    app.sessions.insert("fix".into(), fix);
    app.selection.clear();
    app
}

#[test]
fn subagent_decks_folded_by_default() {
    check("subagent-decks", &mut subagent_decks(), BOTH);
}

/// K on "Fix the build": its subagent boxes at their saved places, the
/// other session's deck folded as before.
#[test]
fn subagent_decks_one_unfolded() {
    let mut app = subagent_decks();
    app.selection = vec!["fix".into()];
    app.run(Action::Visual(canvas_core::visual::VisualAction::ToggleStack));
    assert_eq!(app.doc.node("fix").and_then(|n| n.subagents_shown), Some(true));
    check("subagent-decks-unfolded", &mut app, BOTH);
}

/// A click on a deck lists its subagent boxes in its session's details
/// panel: each with its state and Inspect (viewing starts nothing).
#[test]
fn subagent_deck_details() {
    let mut app = subagent_decks();
    app.run(Action::Visual(canvas_core::visual::VisualAction::ViewSubagents("orch".into())));
    check("subagent-deck-details", &mut app, Sections { cells: true, painted: true });
}

/// Two looping sessions (`/every`) and one that does not loop: each arc with
/// its lit dot and two-cell trail where the clock puts it (TUI), the model's
/// loops with their lap, "↻ every …" in the status lines, ↻ in the sidebar.
fn looping(motion: canvas_core::view::Motion) -> App {
    let mut doc = Doc::default();
    doc.nodes.push(node("nightly", "Nightly check", 1, 1));
    doc.nodes.push(node("hourly", "Hourly sync", 1, 8));
    doc.nodes.push(node("plain", "Not looping", 40, 1));
    doc.view.x = -1;
    doc.view.y = 0;
    doc.ui.terminal_percent = 30;
    let mut app = app(doc, false);
    for (id, every) in [("nightly", 900_000), ("hourly", 3_600_000), ("plain", 0)] {
        let mut s = summary(id, Activity::Idle, true);
        s.loop_ms = every;
        app.sessions.insert(id.into(), s);
    }
    app.visual.clock_ms = Some(NOW + 700);
    app.visual.motion = Some(motion);
    app
}

#[test]
fn looping_sessions() {
    check("looping-sessions", &mut looping(canvas_core::view::Motion::On), BOTH);
}

/// The same with reduced motion: no moving dot; the arc, its lit arrowhead
/// and (GPU) a ↻ badge hold still.
#[test]
fn looping_sessions_in_reduced_motion() {
    check("looping-sessions-reduced-motion", &mut looping(canvas_core::view::Motion::Reduced), BOTH);
}

/// The working glow on a fixed clock: one session at work (faded in a while
/// ago: its ring and steady glow), one that stopped 300 ms ago (its glow
/// fading out, its last ring travelling on) and an idle one (none). The TUI's
/// dots near them take lit colours (these snapshots hold characters, not
/// colours: the `glow` unit tests pin those).
#[test]
fn working_glow_around_working_sessions() {
    use canvas_core::glow::Fade;
    let mut doc = Doc::default();
    doc.nodes.push(node("busy", "Working on it", 1, 1));
    doc.nodes.push(node("done", "Just stopped", 40, 1));
    doc.nodes.push(node("quiet", "Idle one", 1, 14));
    doc.view.x = -1;
    doc.view.y = 0;
    let mut app = app(doc, false);
    app.sessions.insert("busy".into(), summary("busy", Activity::Working, true));
    app.sessions.insert("done".into(), summary("done", Activity::Idle, true));
    app.sessions.insert("quiet".into(), summary("quiet", Activity::Idle, true));
    let clock = NOW + 700;
    app.visual.glow.insert("busy".into(), Fade { on: true, since: Some((NOW - 10_000, 0.0)) });
    app.visual.glow.insert("done".into(), Fade { on: false, since: Some((clock - 300, 1.0)) });
    app.visual.clock_ms = Some(clock);
    check("working-glow", &mut app, BOTH);
}

/// Two groups: one with a folder (chips lit), one without (dim chips); the
/// first is selected, so the right panel shows its folders and members.
#[test]
fn group_with_folder_chips() {
    let mut doc = Doc::default();
    doc.nodes.push(node("a", "Write the spec", 2, 2));
    doc.nodes.push(node("b", "Draft the docs", 2, 7));
    doc.nodes.push(node("c", "Unfiled idea", 2, 18));
    doc.groups.push(Group { id: "g-docs".into(), name: "Docs".into(), x: 0, y: 0, w: 48, h: 13, color: 1, folder: Some(r"C:\work\docs".into()), source: None, defaults: None });
    doc.groups.push(Group { id: "g-ideas".into(), name: "Ideas".into(), x: 0, y: 16, w: 44, h: 8, color: 3, folder: None, source: None, defaults: None });
    doc.view.x = -1;
    doc.view.y = -1;
    let mut app = app(doc, false);
    app.selection.clear();
    app.selected_group = Some("g-docs".into());
    check("group-folder-chips", &mut app, BOTH);
}

/// The todo book of the spaces snapshots: Docs's board (DEC) with 4 open
/// todos (2 in progress) and a proposal; "Write the spec" (a) claimed DEC-031.
fn spaces_book() -> canvas_model::todos::TodoBook {
    use canvas_model::todos::{Actor, Ctx, NewTodo, Status, TodoBook, TodoOp, TodoType};
    let mut book = TodoBook::default();
    let (you, spec) = (Actor::human(), Actor::agent("a", "Write the spec"));
    let ctx = Ctx::at(1_792_998_000_000);
    let add = |title: &str, status: Status, propose: bool| TodoOp::Add(Box::new(NewTodo { board: "Decomps".into(), title: title.into(), kind: TodoType::Feature, status: (!propose).then_some(status), propose, why: propose.then(|| "Faster runs.".into()), ..Default::default() }));
    book.apply(TodoOp::AddBoard { name: "Decomps".into(), prefix: Some("DEC".into()), parent: None, groups: vec!["g-docs".into()], x: None, y: None }, &you, &ctx).unwrap();
    book.seq.insert("DEC".into(), 27);
    for (title, status) in [("Map the structs", Status::Backlog), ("Name the loader", Status::Todo), ("Parse the tables", Status::InProgress), ("Write the loader spec", Status::Todo)] {
        book.apply(add(title, status, false), &you, &ctx).unwrap();
    }
    book.apply(TodoOp::Claim { id: "DEC-031".into(), force: false, session: None }, &spec, &ctx).unwrap();
    book.apply(add("Cache the output", Status::Proposed, true), &spec, &ctx).unwrap();
    book
}

/// Two groups for the spaces snapshots: Docs (a folder, a linked board with
/// a proposal waiting) and Ideas (neither); "Write the spec" claimed DEC-031.
fn spaces_app() -> App {
    let mut doc = Doc::default();
    doc.nodes.push(node("a", "Write the spec", 2, 2));
    doc.nodes.push(node("b", "Draft the docs", 2, 7));
    doc.nodes.push(node("c", "Unfiled idea", 2, 18));
    doc.groups.push(Group { id: "g-docs".into(), name: "Docs".into(), x: 0, y: 0, w: 64, h: 13, color: 1, folder: Some(r"C:\work\docs".into()), source: None, defaults: None });
    doc.groups.push(Group { id: "g-ideas".into(), name: "Ideas".into(), x: 0, y: 16, w: 44, h: 8, color: 3, folder: None, source: None, defaults: None });
    doc.view.x = -1;
    doc.view.y = -1;
    let mut app = app(doc, false);
    app.bus.todos = Some(spaces_book());
    app.selection.clear();
    app
}

/// The spaces foundation on the session canvas, wide enough for the full
/// tabs: Docs's chips (✎ Notes; ☑ 4 · 2▸ · 1◇ in gold: a proposal waits),
/// Ideas's (✎ Set folder…, ☑ +), the session's ☑ DEC-031 badge, and the
/// selected group's NOTES and TODOS.
#[test]
fn canvas_with_space_tabs_todo_chips_and_a_claimed_session() {
    let mut app = spaces_app();
    app.selected_group = Some("g-docs".into());
    check_sized("canvas-spaces-chips", &mut app, Sections { cells: true, painted: true }, (180, 40));
}

/// The Notebook space opened from a group's ✎ Notes chip with no backend:
/// compact tabs, the ✎ EXPLORER with the group's folder as a root (listing
/// on its way), the reader without a note and the info panel, drawn from
/// cells in both clients (no canvas model).
#[test]
fn notebook_space_from_a_group_chip() {
    let mut app = spaces_app();
    app.run(Action::Notebook(canvas_core::notebook::Action::OpenGroup("g-docs".into())));
    assert_eq!(app.space, canvas_core::spaces::Space::Notebook);
    check("notebook-space", &mut app, BOTH);
}

/// The Tasks space reached by a click on a session's ☑ DEC-031 badge: the
/// todo selected on its board, its details in the right pane.
#[test]
fn tasks_space_from_a_session_badge() {
    let mut app = spaces_app();
    let area = Rect::new(0, 0, SCREEN.0, SCREEN.1);
    canvas_core::ui::render(&mut app, &mut Buffer::empty(area), area, None);
    let open = Action::Tasks(canvas_core::tasks::Action::OpenTodo("DEC-031".into()));
    let (rect, _) = app.layout.hits.iter().find(|(_, hit)| *hit == canvas_core::dialogs::Hit::Toolbar(open.clone())).cloned().expect("the badge is clickable");
    let at = |kind| canvas_core::input::MouseEvent::at_cell(kind, rect.x + 1, rect.y, canvas_core::input::Modifiers::NONE);
    app.on_mouse(at(canvas_core::input::MouseEventKind::Down(canvas_core::input::MouseButton::Left)));
    app.on_mouse(at(canvas_core::input::MouseEventKind::Up(canvas_core::input::MouseButton::Left)));
    assert_eq!(app.space, canvas_core::spaces::Space::Tasks);
    assert_eq!(app.right_detail, Some(canvas_core::side::RightDetail::Todo("DEC-031".into())));
    assert_eq!(app.tasks.selected.as_deref(), Some("DEC-031"));
    assert!(app.launching.is_empty(), "a badge never starts its session");
    check("tasks-space", &mut app, BOTH);
}

/// The demo canvas's Tasks space (its demo book) at a zoom level: two
/// boards, Decomps with its Parser swimlane; 100 % full cards, 50 % one
/// row a card, 25 % a summary a board.
fn tasks_demo(zoom: u8) -> App {
    let mut app = demo_at(0, 1.0);
    app.switch_space(canvas_core::spaces::Space::Tasks);
    app.tasks.zoom = zoom;
    app.tasks.pan = [(-2, -1), (-4, -2), (-8, -4)][zoom as usize];
    app.notice = None;
    app
}

#[test]
fn tasks_demo_boards_at_100_percent() {
    check("tasks-demo-100", &mut tasks_demo(0), BOTH);
}

#[test]
fn tasks_demo_boards_at_50_percent() {
    check("tasks-demo-50", &mut tasks_demo(1), BOTH);
}

#[test]
fn tasks_demo_boards_at_25_percent() {
    check("tasks-demo-25", &mut tasks_demo(2), BOTH);
}

/// A selected card (WI-002: a checklist, its session's claim, a note in
/// its log) in view, with its details in the right pane.
#[test]
fn tasks_selected_card_with_details() {
    let mut app = tasks_demo(0);
    app.run(Action::Tasks(canvas_core::tasks::Action::Select("WI-002".into())));
    app.notice = None;
    assert_eq!(app.right_detail, Some(canvas_core::side::RightDetail::Todo("WI-002".into())));
    check("tasks-details", &mut app, BOTH);
}

/// With a VM pack: a card's details say where its board's sessions start
/// (here the board's *Start on VM* policy), and *Start on VM…* opens the
/// session dialog on a VM with the todo's branch and a first prompt that
/// ends with a pull request.
#[test]
fn tasks_start_on_vm() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    with_vm_pack(&mut app);
    app.bus.todos = Some(canvas_model::todos::demo_book());
    app.switch_space(canvas_core::spaces::Space::Tasks);
    app.tasks.pan = (-2, -1);
    let book = app.bus.todos.as_mut().unwrap();
    let board = book.todo("WI-002").map(|t| t.board.clone()).unwrap();
    let root = book.root(&board).map(|b| b.id.clone()).unwrap();
    let policy = canvas_model::todos::AgentPolicy { start_on_vm: true, ..book.board(&root).unwrap().policy };
    book.apply(canvas_model::todos::TodoOp::UpdateBoard { board: root.clone(), name: None, prefix: None, wip: None, color: None, notes: None, policy: Some(policy), statuses: None }, &canvas_model::todos::Actor::human(), &canvas_model::todos::Ctx::at(1)).unwrap();
    app.tasks.board = Some(root);
    app.notice = None;
    check_sized("tasks-board-policy-vm", &mut app, Sections { cells: true, painted: false }, (124, 40));
    app.run(Action::Tasks(canvas_core::tasks::Action::StartSession("WI-002".into())));
    check_sized("tasks-start-on-vm", &mut app, Sections { cells: true, painted: false }, (124, 40));
}

/// A group without a board: its ☑ + chip offers the boards to link and a
/// new board for the group.
#[test]
fn tasks_link_boards_dialog() {
    let mut app = spaces_app();
    app.bus.todos.as_mut().unwrap().apply(canvas_model::todos::TodoOp::AddBoard { name: "Release".into(), prefix: None, parent: None, groups: Vec::new(), x: None, y: None }, &canvas_model::todos::Actor::human(), &canvas_model::todos::Ctx::at(1)).unwrap();
    app.run(Action::Tasks(canvas_core::tasks::Action::OpenGroup("g-ideas".into())));
    assert!(matches!(app.modals.last(), Some(canvas_core::dialogs::Modal::Tasks(_))));
    check("tasks-link-boards", &mut app, BOTH);
}

/// Go to (Ctrl+P) on the demo canvas with a query: the matches best first,
/// their matched letters lit, where they are on the right.
#[test]
fn go_to_dialog_with_results() {
    let mut app = demo_at(0, 1.0);
    app.doc.groups.push(Group { id: "g-review".into(), name: "Reviews".into(), x: 0, y: 40, w: 40, h: 8, color: 2, folder: None, source: None, defaults: None });
    app.open_goto();
    for c in "rev".chars() {
        app.on_input(vec![canvas_core::input::InputEvent::Key(canvas_core::input::KeyEvent::new(canvas_core::input::KeyCode::Char(c), canvas_core::input::Modifiers::NONE))]);
    }
    check("goto-dialog", &mut app, BOTH);
}

/// A canvas with a group (a session and its group passive session) and the
/// Canvas agents frame with three canvas-wide agents: monitoring (with a
/// skill), paused, and one that reported an alert.
fn canvas_agents_app() -> App {
    use canvas_core::doc::{CANVAS_AGENT, CANVAS_AGENTS, Passive, PassiveScope};
    let mut doc = Doc { canvas_id: Some("canvas-1".into()), ..Doc::default() };
    doc.groups.push(Group { id: "g-pay".into(), name: "Payments".into(), x: 0, y: 0, w: 70, h: 8, color: 0, folder: Some(r"C:\work\pay".into()), source: None, defaults: None });
    doc.groups.push(Group { id: "g-agents".into(), name: "Canvas agents".into(), x: 0, y: 10, w: 104, h: 8, color: 3, folder: None, source: Some(CANVAS_AGENTS.into()), defaults: None });
    doc.nodes.push(node("s-fix", "Fix flaky test", 2, 2));
    let mut guard = node("p-guard", "Drift guard", 36, 2);
    guard.passive = Some(Passive { key: "k-guard".into(), group: "g-pay".into(), slug: "drift-guard".into(), template: "Drift guard".into(), subscribe: vec!["pr.pushed".into()], max_runs_per_day: 24, rollover_runs: 20, ..Default::default() });
    doc.nodes.push(guard);
    for (i, (id, name, subscribe, skills, paused)) in [
        ("a-pr", "PR coordinator", vec!["pr.*", "build.failed"], vec!["pr-etiquette"], false),
        ("a-digest", "Cross-group digest", vec!["session.done"], vec![], true),
        ("a-release", "Release watch", vec!["rollout.started"], vec![], false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut n = node(id, name, 2 + i as i32 * 34, 12);
        n.source = Some(CANVAS_AGENT.into());
        n.agent = Some("copilot".into());
        n.passive = Some(Passive {
            key: format!("k-{id}"),
            group: "g-agents".into(),
            slug: id.into(),
            template: name.into(),
            subscribe: subscribe.into_iter().map(str::to_owned).collect(),
            skills: skills.into_iter().map(str::to_owned).collect(),
            paused,
            scope: PassiveScope::Canvas,
            max_runs_per_day: 24,
            rollover_runs: 20,
            prs: (id == "a-pr").then(Default::default),
            ..Default::default()
        });
        doc.nodes.push(n);
    }
    doc.view.x = -1;
    doc.view.y = -1;
    let mut app = app(doc, false);
    app.info.home = r"C:\data".into();
    app.bus.passive.insert("k-a-release".into(), canvas_core::bus::PassiveView { status: "alert".into(), headline: "ring 2 is failing".into(), ..Default::default() });
    app.bus.passive.insert("k-a-pr".into(), canvas_core::bus::PassiveView { status: "ok".into(), headline: "2 PRs tracked".into(), ..Default::default() });
    app.canvas_skills = Some(Ok(vec![canvas_model::canvas_wide::SkillInfo {
        name: "pr-etiquette".into(),
        description: "How PR feedback reaches the owning session".into(),
        path: r"C:\data\canvas-agents\canvas-1\skills\pr-etiquette\SKILL.md".into(),
        problem: String::new(),
    }]));
    app.selection.clear();
    // The agents' loop arcs: their dots at a fixed time.
    app.visual.clock_ms = Some(NOW);
    app
}

/// The panel of a group's passive sessions (its ⟲ chip, sidebar row or menu
/// opens it): each one's state and buttons, and the canvas-wide agents
/// named apart.
#[test]
fn group_passive_sessions_panel() {
    let mut app = canvas_agents_app();
    app.run(Action::Visual(canvas_core::visual::VisualAction::GroupPassives("g-pay".into())));
    check("group-passives-panel", &mut app, Sections { cells: true, painted: false });
}

/// A group without passive sessions: its panel's empty state with Create.
#[test]
fn group_passive_sessions_empty() {
    let mut doc = Doc::default();
    doc.nodes.push(node("a", "Write the spec", 2, 2));
    doc.groups.push(Group { id: "g-docs".into(), name: "Docs".into(), x: 0, y: 0, w: 48, h: 8, color: 1, folder: Some(r"C:\work\docs".into()), source: None, defaults: None });
    doc.view.x = -1;
    doc.view.y = -1;
    let mut app = app(doc, false);
    app.selection.clear();
    app.run(Action::Visual(canvas_core::visual::VisualAction::GroupPassives("g-docs".into())));
    check("group-passives-empty", &mut app, Sections { cells: true, painted: false });
}

/// The top bar's ◎ Agents entry (one needs attention), the frame with its
/// chips and dashed ◎ boxes, the sidebar rows, and the management panel.
#[test]
fn canvas_wide_agents_panel() {
    let mut app = canvas_agents_app();
    app.run(Action::Agents(canvas_core::canvas_wide::AgentAction::Panel));
    // The panel reads the library from the backend (none here): keep the test's.
    app.canvas_skills = canvas_agents_app().canvas_skills;
    check("canvas-agents-panel", &mut app, Sections { cells: true, painted: true });
}

/// One canvas-wide agent's panel (its box selected): what it watches, its
/// skill, its state and every control.
#[test]
fn canvas_wide_agent_detail() {
    let mut app = canvas_agents_app();
    app.select_node("a-pr", false);
    check("canvas-agent-detail", &mut app, BOTH);
}

/// An agent's settings with *More* open, its instructions read, and a chat
/// asked for while a run answers (it opens when the run ends).
#[test]
fn canvas_wide_agent_settings_with_more() {
    let mut app = canvas_agents_app();
    app.agent_ui.more.insert("a-release".into());
    app.agent_ui.rules.insert("a-release".into(), ("# Release watch\n\nFollow ring 2 of the release; alert when it fails.\n".into(), std::time::Instant::now()));
    app.agent_ui.chat_after_run.insert("a-release".into());
    app.bus.passive.insert("k-a-release".into(), canvas_core::bus::PassiveView { running: true, pending: 1, ..Default::default() });
    app.select_node("a-release", false);
    check("canvas-agent-settings", &mut app, BOTH);
}

/// The *New canvas agent* dialog as it opens: no template, the default
/// agent and its remembered model, no triggers (only when you talk to it),
/// every group, no skills.
#[test]
fn new_canvas_agent_dialog() {
    let mut app = canvas_agents_app();
    app.doc.defaults.set_model(AgentKind::Copilot, Some("gpt-remembered"));
    app.run(Action::Agents(canvas_core::canvas_wide::AgentAction::New));
    check("new-canvas-agent-dialog", &mut app, BOTH);
}

/// The dialog prefilled from a template (*Start from… PR coordinator*), a
/// name of your own and a first message.
#[test]
fn new_canvas_agent_dialog_from_a_template() {
    let mut app = canvas_agents_app();
    app.run(Action::Agents(canvas_core::canvas_wide::AgentAction::New));
    app.apply_agent_pick(canvas_core::dialogs::PickPurpose::AgentTemplate, "PR coordinator");
    if let Some(dialog) = canvas_core::agent_form::dialog_mut(&mut app) {
        dialog.name.set("Team PRs");
        dialog.message.set("Which pull requests are open?");
        dialog.focus = canvas_core::agent_form::Field::Instructions;
    }
    check("new-canvas-agent-template", &mut app, BOTH);
}

/// The tracked pull requests (◎ Agents › Pull requests) at a fixed clock: a
/// follow-up blocked because its owner is stopped (Resume owner, Reassign,
/// Dismiss), a monitor that cannot sign in, one with three new comments and
/// a delivered follow-up, and a merged one whose monitoring stopped.
#[test]
fn tracked_pull_requests_panel() {
    use canvas_model::prs::{Dispatch, DispatchState, Monitor, MonitorStatus, PrRef, PrState, PrsView, Registry, Role, TrackedPr};
    let mut app = canvas_agents_app();
    app.doc.nodes.push(node("s-api", "Payments API", 2, 40));
    app.visual.clock_ms = Some(NOW);
    let min = |m: u64| NOW - m * 60_000;
    let tracked = |url: &str, owner: &str, owner_name: &str, title: &str| {
        let mut pr = TrackedPr::new(PrRef::parse(url).unwrap(), Role::Owned, owner, owner_name, "g-pay", "handed off by the session", min(60));
        pr.title = title.into();
        pr.monitor = Monitor { status: MonitorStatus::Ok, last_ms: min(2), next_ms: NOW + 3 * 60_000, polls: 4, ..Default::default() };
        pr
    };
    let mut comments = tracked("https://github.com/octo/app/pull/42", "s-fix", "Fix flaky test", "Fix flaky upload test");
    comments.comments = 5;
    comments.new_comments = 0;
    comments.decision = "changes_requested".into();
    comments.last_change = "3 new comment(s)".into();
    comments.last_change_ms = min(2);
    comments.dispatches.push(Dispatch {
        id: "d-1-0001".into(),
        key: "comments:i:1203".into(),
        event: "pr.comment.new".into(),
        to: "s-fix".into(),
        to_name: "Fix flaky test".into(),
        text: "Review and address the 3 new review comments (ana: retry logic, ben: naming): https://github.com/octo/app/pull/42#discussion_r1203".into(),
        from: "PR coordinator".into(),
        state: DispatchState::Delivered,
        created_ms: min(2),
        updated_ms: min(1),
        delivered_ms: min(1),
        ..Default::default()
    });
    let mut blocked = tracked("https://dev.azure.com/contoso/Payments/_git/api/pullrequest/1234", "s-api", "Payments API", "Retry on 429");
    blocked.failing = vec!["CI".into()];
    blocked.last_change = "a check is failing: CI".into();
    blocked.last_change_ms = min(5);
    blocked.dispatches.push(Dispatch {
        id: "d-2-0002".into(),
        key: "checks:c0ffee:CI".into(),
        event: "pr.checks.failed".into(),
        to: "s-api".into(),
        to_name: "Payments API".into(),
        text: "Investigate the failing CI build and fix it".into(),
        from: "PR coordinator".into(),
        state: DispatchState::Blocked,
        reason: "the session is not running: resume it (the follow-up is typed in once it is idle), reassign or dismiss".into(),
        created_ms: min(5),
        updated_ms: min(5),
        ..Default::default()
    });
    let mut signed_out = tracked("https://github.com/octo/svc/pull/7", "s-fix", "Fix flaky test", "Bump the SDK");
    signed_out.monitor = Monitor { status: MonitorStatus::Unavailable, reason: "gh is not signed in on the service's machine: run gh auth login".into(), last_ms: min(10), next_ms: NOW + 20 * 60_000, failures: 1, polls: 0 };
    let mut merged = tracked("https://github.com/octo/app/pull/40", "s-fix", "Fix flaky test", "Upload retries");
    merged.state = PrState::Merged;
    merged.monitor.status = MonitorStatus::Stopped;
    merged.monitor.reason = "merged: monitoring stopped".into();
    merged.last_change = "merged".into();
    merged.last_change_ms = min(90);
    let registry = Registry { prs: vec![comments, blocked, signed_out, merged] };
    app.bus.prs = Some(PrsView::of("canvas-1", &registry, vec!["PR coordinator".into()], String::new(), Vec::new()));
    app.run(Action::Prs(canvas_core::prs::PrAction::Panel));
    check("prs-panel", &mut app, Sections { cells: true, painted: true });
}

/// Review follow-ups (◎ Agents › Review follow-ups) at a fixed clock: a draft
/// awaiting your approval, a request that needs clarification (two reviewed
/// pull requests, one button each), a sent message, the watched conversation
/// with its last and next check, and a delivered request.
#[test]
fn review_follow_ups_panel() {
    use canvas_model::reviews::{CheckState, Conversation, FollowRequest, Message, MessageState, RequestState, Review, ReviewSettings, ReviewsView};
    let mut app = canvas_agents_app();
    app.doc.nodes.push(node("s-rev", "Review the SDK bump", 2, 40));
    app.visual.clock_ms = Some(NOW);
    let min = |m: u64| NOW - m * 60_000;
    let review = |id: &str, n: u32, outcome: &str, message: Message, ago: u64| Review {
        id: id.into(),
        pr: format!("github.com/octo/sdk#{n}"),
        url: format!("https://github.com/octo/sdk/pull/{n}"),
        short: format!("octo/sdk#{n}"),
        title: "Bump the SDK".into(),
        author: "Ana Silva".into(),
        author_id: "ana".into(),
        outcome: outcome.into(),
        results: "Two blocking issues: retries never stop; the 429 path is untested.".into(),
        reviewer: "s-rev".into(),
        reviewer_name: "Review the SDK bump".into(),
        created_ms: min(ago),
        message,
        ..Default::default()
    };
    let sent = Message { state: MessageState::Sent, draft: "Hi Ana, …".into(), channel: "mcp".into(), conversation: "teams:chat-ana".into(), recipient: "Ana Silva".into(), sent_ms: min(50), updated_ms: min(50), ..Default::default() };
    let awaiting = Message {
        state: MessageState::AwaitingApproval,
        draft: "Hi Ana, I reviewed https://github.com/octo/sdk/pull/12: changes requested. Retries never stop and the 429 path needs a test; details in my two comments.".into(),
        agent: "k-a-review".into(),
        updated_ms: min(3),
        ..Default::default()
    };
    let reviews = vec![review("rv-2", 12, "changes-requested", awaiting, 5), review("rv-1", 11, "approved", sent, 55)];
    let conversation = Conversation { id: "teams:chat-ana".into(), channel: "mcp".into(), contact: "ana".into(), contact_name: "Ana Silva".into(), reviews: vec!["rv-1".into()], watching: true, last_checked_ms: min(12), mark: "m-41".into(), created_ms: min(50), active_ms: min(20), seen: vec![] };
    let unclear = FollowRequest { id: "q-2".into(), conversation: "teams:chat-ana".into(), message_id: "m-41".into(), kind: "another-review".into(), text: "Could you look again?".into(), state: RequestState::Ambiguous, reason: "which pull request? octo/sdk#11 and octo/sdk#12 were reviewed for Ana Silva".into(), created_ms: min(12), updated_ms: min(12), ..Default::default() };
    let delivered = FollowRequest {
        id: "q-1".into(),
        conversation: "teams:chat-ana".into(),
        message_id: "m-30".into(),
        kind: "re-review".into(),
        text: "Pushed the fixes, please re-review".into(),
        pr: "github.com/octo/sdk#11".into(),
        short: "octo/sdk#11".into(),
        review: "rv-1".into(),
        state: RequestState::Delivered,
        to: "s-rev".into(),
        to_name: "Review the SDK bump".into(),
        created_ms: min(40),
        updated_ms: min(38),
        delivered_ms: min(38),
        ..Default::default()
    };
    let settings = ReviewSettings::parse("mcp teams approval every 60m recipients: the author's Teams account (their GitHub email)").unwrap().unwrap();
    let mut view = ReviewsView { canvas: "canvas-1".into(), agents: vec!["Review follow-up".into()], settings: Some(settings), paused: String::new(), checks: CheckState { last_ms: min(12), next_ms: NOW + 48 * 60_000, asked_ms: min(12), failures: 0, last_error: String::new() }, reviews, conversations: vec![conversation], requests: vec![unclear, delivered] };
    view.conversations[0].reviews.push("rv-2".into());
    app.bus.reviews = Some(view);
    app.run(Action::Reviews(canvas_core::review_followups::ReviewAction::Panel));
    check("review-followups", &mut app, Sections { cells: true, painted: true });
}

/// Two boxes selected while a right-drag rubber band is open.
#[test]
fn selection_and_rubber_band() {
    let mut app = demo_at(0, 1.0);
    app.selection = vec!["demo-main".into(), "demo-review".into()];
    app.drag = Some(Drag::Rubber { start: (38, 3), now: (70, 14), additive: false, right: true, click: None });
    app.drag_origin = (38.5, 3.5);
    app.pointer = (70.5, 14.5);
    check("selection-rubber-band", &mut app, BOTH);
}

/// A mixed area selection (a session with its subagent box, a session with
/// its subsession): every selected box is highlighted the same way, at
/// 100 % and at 50 % (the TUI's compact boxes underline it). The session
/// shows its subagent box (K), so the box can be selected.
#[test]
fn selection_with_subagents_and_subsessions() {
    for (name, zoom, scale) in [("selection-subsessions-100", 0, 1.0), ("selection-subsessions-50", 1, 0.5)] {
        let mut app = demo_at(zoom, scale);
        app.doc.node_mut("demo-main").unwrap().subagents_shown = Some(true);
        app.selection = ["demo-main", "demo-main:explore", "demo-review", "demo-sub"].map(String::from).to_vec();
        check(name, &mut app, Sections { cells: true, painted: true });
    }
}

/// The new-session dialog: name, folder, the Agent row (one agent not
/// installed), the model it starts with (the initial default), MCP servers,
/// first prompt.
#[test]
fn new_session_dialog() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    configs(&mut app);
    app.run(Action::NewSession { at: None, parent: None });
    check("new-session-dialog", &mut app, BOTH);
}

/// A service with a VM pack (docs/VM-SESSIONS.md): the bus snapshot's VM view.
fn with_vm_pack(app: &mut App) {
    let repo = canvas_model::vm::RepoSummary { key: "contoso-app".into(), default_branch: "main".into(), folders: vec!["**/canvas".into()] };
    let pack = canvas_model::vm::PackSummary { name: "contoso".into(), provider: "aca".into(), repos: vec![repo], sizes: vec!["S".into(), "M".into()], ..Default::default() };
    app.bus.vm = Some(canvas_model::vm::VmView { pack, ..Default::default() });
}

/// With a VM pack the dialog has *Run on*, and it starts on this machine
/// (VM sessions are optional; local stays the default).
#[test]
fn new_session_dialog_with_a_vm_pack_runs_here_by_default() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    configs(&mut app);
    with_vm_pack(&mut app);
    app.run(Action::NewSession { at: None, parent: None });
    let Some(canvas_core::dialogs::Modal::NewSession(dialog)) = app.modals.last() else { panic!("the dialog") };
    let vm = dialog.vm.as_ref().expect("Run on shows with a pack");
    assert!(!vm.on_vm, "this machine unless chosen");
    assert_eq!((vm.repo.as_str(), vm.size.as_str()), ("contoso-app", "S"), "the repository the folder belongs to");
    check("new-session-run-on-default", &mut app, BOTH);
}

/// *Run on: VM* chosen: the repository, size and branch rows; a box made
/// from it runs on a VM (☁ before its name).
#[test]
fn new_session_dialog_run_on_vm() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    configs(&mut app);
    with_vm_pack(&mut app);
    app.run(Action::NewSession { at: None, parent: None });
    if let Some(canvas_core::dialogs::Modal::NewSession(dialog)) = app.modals.last_mut() {
        dialog.vm.as_mut().unwrap().on_vm = true;
        dialog.focus = canvas_core::dialogs::NsField::VmRepo;
    }
    check("new-session-run-on-vm", &mut app, BOTH);
}

/// A box on a VM shows ☁ before its name; the others are unchanged.
#[test]
fn a_vm_box_is_marked() {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    doc.nodes[0].placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: "contoso-app".into(), ..Default::default() })));
    let mut app = app(doc, false);
    with_vm_pack(&mut app);
    check("vm-box-marker", &mut app, BOTH);
}

/// VM boxes in every state of their VM: the status line says it unless the
/// VM simply runs (then the agent's own state shows).
#[test]
fn vm_box_states() {
    use canvas_model::vm::{VmInfo, VmState};
    let now = canvas_core::paths::now_ms();
    let boxes = [
        ("v-queued", "Queued", VmInfo { state: VmState::Queued, note: "4 of 4 VMs are running".into(), ..Default::default() }),
        ("v-starting", "Starting", VmInfo { state: VmState::Starting, ..Default::default() }),
        ("v-running", "Running, idle", VmInfo { state: VmState::Running, ..Default::default() }),
        ("v-offline", "Integration offline", VmInfo { state: VmState::Running, offline_ms: now - 330_000, ..Default::default() }),
        ("v-suspended", "Suspended", VmInfo { state: VmState::Suspended, ..Default::default() }),
        ("v-resuming", "Resuming", VmInfo { state: VmState::Resuming, ..Default::default() }),
        ("v-lost", "Lost", VmInfo { state: VmState::Lost, ..Default::default() }),
        ("v-deleting", "Deleting", VmInfo { state: VmState::Deleting, ..Default::default() }),
        ("v-failed", "Failed", VmInfo { state: VmState::Failed, note: "the image is not built yet".into(), ..Default::default() }),
    ];
    let mut doc = Doc::default();
    for (i, (id, name, _)) in boxes.iter().enumerate() {
        let mut n = node(id, name, 1 + (i as i32 % 3) * 40, 1 + (i as i32 / 3) * 8);
        n.placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: "contoso-app".into(), ..Default::default() })));
        doc.nodes.push(n);
    }
    doc.view.x = -1;
    doc.view.y = 0;
    let mut app = app(doc, false);
    with_vm_pack(&mut app);
    let view = app.bus.vm.as_mut().unwrap();
    for (id, _, info) in boxes {
        view.boxes.insert(id.into(), info);
    }
    app.sessions.insert("v-running".into(), summary("v-running", Activity::Idle, true));
    app.sessions_loaded = true;
    check_sized("vm-box-states", &mut app, BOTH, (124, 26));
}

/// A VM box's details (viewing it starts nothing): its VM's state, where
/// it runs, cost, repository and link, and the operations its state allows.
#[test]
fn vm_details() {
    use canvas_model::vm::{GitState, VmInfo, VmState};
    let now = canvas_core::paths::now_ms();
    let mut doc = Doc::default();
    let mut n = node("v1", "Fix the flaky test", 1, 1);
    n.placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: "contoso-app".into(), branch: Some("atelier/fix-the-flaky-test".into()), ..Default::default() })));
    doc.nodes.push(n);
    doc.view.x = -1;
    doc.view.y = 0;
    let mut app = app(doc, false);
    with_vm_pack(&mut app);
    let git = GitState { branch: "atelier/fix-the-flaky-test".into(), ahead: 2, dirty: 1, unpushed: vec!["atelier/fix-the-flaky-test".into()], ..Default::default() };
    let info = VmInfo {
        state: VmState::Running,
        sandbox: "sbx-0123456789ab".into(),
        repo: "contoso-app".into(),
        branch: "atelier/fix-the-flaky-test".into(),
        size: "M".into(),
        image_ms: now - 2 * 3_600_000 - 60_000,
        started_ms: now - 40 * 60_000 - 30_000,
        cost_cents: 37,
        git: Some(git),
        refusals: 2,
        ..Default::default()
    };
    app.bus.vm.as_mut().unwrap().boxes.insert("v1".into(), info);
    app.sessions.insert("v1".into(), summary("v1", Activity::Idle, true));
    app.terms.insert("v1".into(), term("v1", true, None));
    app.sessions_loaded = true;
    // Running here: its box menu's *VM details…* shows the panel over its terminal.
    let (_, items) = app.node_menu("v1");
    let details = items.iter().find(|i| i.label == "☁ VM details…").expect("the box menu has VM details").action.clone();
    assert!(items.iter().any(|i| i.label == "Suspend VM" && i.enabled));
    app.run(details);
    check_sized("vm-details", &mut app, Sections { cells: true, painted: false }, (124, 40));
}

/// The toolbar's ☁ VMs (only with a pack; gold while a start waits) and its
/// panel: the budget, the VM boxes, a start waiting with *Cancel*, the
/// sandboxes, the images with *Build now*, the newest refusals.
#[test]
fn vms_panel() {
    use canvas_model::vm::{BudgetView, ImageRow, QueuedStart, Refusal, SandboxRow, VmInfo, VmState};
    let now = canvas_core::paths::now_ms();
    let mut doc = Doc::default();
    for (i, (id, name)) in [("v1", "Fix the flaky test"), ("v2", "Upgrade the SDK")].iter().enumerate() {
        let mut n = node(id, name, 1, 1 + i as i32 * 8);
        n.placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: "contoso-app".into(), ..Default::default() })));
        doc.nodes.push(n);
    }
    doc.view.x = -1;
    doc.view.y = 0;
    let mut app = app(doc, false);
    with_vm_pack(&mut app);
    let view = app.bus.vm.as_mut().unwrap();
    view.boxes.insert("v1".into(), VmInfo { state: VmState::Running, repo: "contoso-app".into(), sandbox: "sbx-0123456789ab".into(), ..Default::default() });
    view.boxes.insert("v2".into(), VmInfo { state: VmState::Queued, repo: "contoso-app".into(), note: "1 of 1 VMs are running".into(), ..Default::default() });
    view.queued.push(QueuedStart { r#box: "v2".into(), since_ms: now - 125_000, why: "1 of 1 VMs are running".into() });
    view.budget = BudgetView { running: 1, max_running: 1, cores: 2.0, max_cores: 8.0, today_cents: 37, daily_cents: 2_000 };
    view.sandboxes.push(SandboxRow { id: "sbx-0123456789ab".into(), role: "session".into(), state: VmState::Running, repo: "contoso-app".into(), size: "M".into(), boxes: vec!["v1".into()], created_ms: now - 50 * 60_000 - 1_000 });
    view.sandboxes.push(SandboxRow { id: "sbx-ba9876543210".into(), role: "pool".into(), state: VmState::Suspended, repo: "contoso-app".into(), size: "M".into(), boxes: Vec::new(), created_ms: now - 3 * 3_600_000 - 1_000 });
    view.images.push(ImageRow { repo: "contoso-app".into(), id: "img-1".into(), built_ms: now - 26 * 3_600_000, state: "ready".into(), note: String::new() });
    view.refusals.push(Refusal { ms: now - 600_000, r#box: "v1".into(), kind: "egress".into(), what: "registry.example.com:443".into(), why: "not in the repository's egress profile".into() });
    app.run(Action::VmPanel);
    check_sized("vms-panel", &mut app, Sections { cells: true, painted: false }, (130, 40));
}

/// A group's session defaults with a VM pack: *Run new sessions on* (here
/// set to a VM for the group; the canvas's stays this machine).
#[test]
fn group_defaults_run_on() {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    let group = doc.groups[0].id.clone();
    let mut app = app(doc, false);
    with_vm_pack(&mut app);
    app.run(Action::GroupDefaults(group));
    app.cycle_run_on();
    app.cycle_run_on();
    check_sized("group-defaults-run-on", &mut app, Sections { cells: true, painted: false }, (124, 34));
}

/// A new session in a group that sets its own model: the dialog says so.
#[test]
fn new_session_dialog_in_a_group_with_its_own_model() {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    let mut overrides = canvas_core::doc::LaunchDefaults::default();
    overrides.set_model(AgentKind::Copilot, Some("claude-sonnet-5"));
    doc.groups[0].defaults = Some(overrides);
    doc.defaults.set_model(AgentKind::Copilot, Some("gpt-6-sol"));
    let mut app = app(doc, false);
    configs(&mut app);
    // Below the first session, inside the demo group.
    app.run(Action::NewSession { at: Some((2, 11)), parent: None });
    check("new-session-dialog-group-model", &mut app, BOTH);
}

/// The model picker over the new-session dialog: OpenCode's own list (`opencode
/// models`), the initial default, the agent's own default and a typed id.
#[test]
fn model_picker() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    configs(&mut app);
    app.agent_configs.get_mut(&AgentKind::OpenCode).unwrap().models =
        ["opencode/big-pickle", "github-copilot/claude-opus-5.5", "github-copilot/gpt-6-sol", "openai/gpt-6.1-sol"].map(str::to_owned).to_vec();
    app.doc.default_agent = Some("opencode".into());
    app.doc.defaults.set_model(AgentKind::OpenCode, Some("github-copilot/gpt-6-sol"));
    app.run(Action::NewSession { at: None, parent: None });
    app.open_model_picker(canvas_core::dialogs::ModelTarget::Dialog);
    check("model-picker", &mut app, BOTH);
}

/// The MCP picker over the new-session dialog, with a saved preset.
#[test]
fn mcp_picker() {
    let mut app = app(canvas_core::demo::demo_doc(CWD), false);
    configs(&mut app);
    app.doc.mcp.save_preset("Lean", &["playwright".to_owned()]);
    app.run(Action::NewSession { at: None, parent: None });
    app.open_mcp_picker(McpTarget::NewSession);
    check("mcp-picker", &mut app, BOTH);
}

/// The sidebar's `/` filter while typing.
#[test]
fn sidebar_filter() {
    let mut app = demo_at(0, 1.0);
    app.focus = Focus::Sidebar;
    app.side_filter = TextInput::new("rev");
    app.side_filter_active = true;
    check("sidebar-filter", &mut app, BOTH);
}

/// The Sessions panel's RECENT section above the tree: the sessions used
/// last (newest first) with status, agent and age; the walk keys in its
/// header; the selected one highlighted.
#[test]
fn sidebar_recent_sessions() {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    for (id, minutes) in [("demo-review", 2), ("demo-main", 30), ("demo-sub", 180)] {
        doc.node_mut(id).unwrap().accessed_ms = Some(NOW - minutes * 60_000);
    }
    let mut app = app(doc, true);
    app.visual.clock_ms = Some(NOW);
    app.selection = vec!["demo-main".into()];
    app.focus = Focus::Sidebar;
    check("sidebar-recent", &mut app, BOTH);
}

/// Two live terminals tiled, with known rows (colours, a wide character),
/// the second one focused (cursor, halo while typing).
#[test]
fn tiled_terminals() {
    let mut doc = canvas_core::demo::demo_doc(CWD);
    doc.ui.tiled = true;
    let mut app = app(doc, true);
    let run = |col: u16, text: &str, fg: u32, flags: u16| Run { col, text: text.into(), fg, bg: 0, flags, link: None };
    let rows: [Vec<Vec<Run>>; 2] = [
        vec![
            vec![run(0, "PS> Write-Output colours", 0, 0)],
            vec![run(0, "red", 2, 0), run(4, "green", 3, 0), run(10, "bold", 0, canvas_core::protocol::F_BOLD)],
            vec![run(0, "wide: 中文", 0, 0)],
            vec![run(0, "PS> ", 0, 0)],
        ],
        vec![vec![run(0, "PS> Get-Date -Format yyyy", 0, 0)], vec![run(0, "2026", 0, 0)], vec![run(0, "PS> ", 0, 0)]],
    ];
    for (id, rows) in ["demo-main", "demo-review"].into_iter().zip(rows) {
        let cursor = Some((4, rows.len() as u16 - 1));
        app.terms.insert(id.into(), TermInfo { title: "Windows PowerShell".into(), ..term(id, true, None) });
        app.views.insert(id.into(), canvas_core::app::TermView { cols: 60, rows: 16, lines: rows, cursor, ..Default::default() });
        app.tile_slots.push(id.into());
    }
    app.selection = vec!["demo-review".into()];
    app.focused_term = Some("demo-review".into());
    app.focus = Focus::Terminal;
    check("tiled-terminals", &mut app, BOTH);
}

/// A fixed "now" for dust ages (day 100 after the epoch).
const NOW: u64 = 100 * 86_400_000;

fn days_ago(days: u64) -> Option<u64> {
    Some(NOW - days * 86_400_000)
}

/// Unused sessions gather dust by age (light, medium, heavy) with their age in
/// the status line; working, needs-you and looping ones never do.
#[test]
fn dusty_sessions() {
    let mut doc = Doc::default();
    let rows = [
        ("d-fresh", "Used yesterday", 1),
        ("d-light", "Unused for 5 days", 5),
        ("d-medium", "Unused for 20 days", 20),
        ("d-heavy", "Unused for 90 days", 90),
        ("d-working", "Working, old", 90),
        ("d-waiting", "Needs you, old", 90),
    ];
    for (i, (id, name, age)) in rows.iter().enumerate() {
        let mut n = node(id, name, 1 + (i as i32 % 2) * 36, 1 + (i as i32 / 2) * 7);
        n.last_used_ms = days_ago(*age);
        n.reviewed_ms = days_ago(*age);
        doc.nodes.push(n);
    }
    doc.view.x = -1;
    doc.view.y = 0;
    doc.ui.terminal_percent = 30;
    let mut app = app(doc, false);
    app.visual.clock_ms = Some(NOW);
    app.sessions.insert("d-working".into(), summary("d-working", Activity::Working, true));
    let mut waiting = summary("d-waiting", Activity::Working, true);
    waiting.pending = 1;
    app.sessions.insert("d-waiting".into(), waiting);
    check("dusty-sessions", &mut app, BOTH);
}

/// A family of subsessions: the parent and a nested subsession.
fn family(stacked: bool) -> Doc {
    let mut doc = Doc::default();
    doc.nodes.push(node("p", "Orchestrator", 1, 1));
    for (i, (id, name)) in [("c1", "First follow-up"), ("c2", "Second follow-up"), ("c3", "Newest follow-up")].iter().enumerate() {
        let mut child = node(id, name, 40, 1 + i as i32 * 7);
        child.parent = Some("p".into());
        doc.nodes.push(child);
    }
    let mut nested = node("c3a", "Nested follow-up", 76, 15);
    nested.parent = Some("c3".into());
    doc.nodes.push(nested);
    doc.nodes[0].stacked = stacked;
    doc.view.x = -1;
    doc.view.y = 0;
    doc.ui.terminal_percent = 30;
    doc
}

/// A stack beneath its parent: offset cards, the newest on top, a +N badge,
/// the most important status of the stack (needs you) and an effect on a
/// stacked child shown on the stack.
#[test]
fn stacked_subsessions() {
    let mut app = app(family(true), false);
    let mut waiting = summary("c1", Activity::Working, true);
    waiting.pending = 1;
    app.sessions.insert("c1".into(), waiting);
    app.visual.motion = Some(canvas_core::view::Motion::Reduced);
    app.frozen_tick = Some(0);
    app.bus.effects.push(canvas_core::effects::Effect { id: "fx-1".into(), kind: "attention".into(), group: "g".into(), target: "c2".into(), note: "2 comments".into(), ms: 1, until_ms: u64::MAX, ..Default::default() });
    check("stacked-subsessions", &mut app, BOTH);
}

/// Every subsession hidden: "⋯ N hidden" on the parent, the boxes' places kept.
#[test]
fn hidden_subsessions() {
    let mut doc = family(false);
    doc.ui.hide_subsessions = true;
    let mut app = app(doc, false);
    let mut failed = summary("c3a", Activity::Failed, false);
    failed.live = false;
    app.sessions.insert("c3a".into(), failed);
    check("hidden-subsessions", &mut app, BOTH);
}

/// J hides every subagent box, folded ones too (their decks go): each
/// session counts its own ("◇ 3 hidden", in the colour of a failed one), the
/// toolbar all of them ("◇ 5 hidden"); subsessions stay (H is off).
#[test]
fn hidden_subagents() {
    let mut app = subagent_decks();
    app.doc.ui.hide_subagents = true;
    check("hidden-subagents", &mut app, BOTH);
}

/// Two subsessions hidden on their own (the canvas's hidden area; one needs
/// you, so the parent's marker is gold), its nested subsession hidden with
/// it; the toolbar's ↳ counter and the parent's hidden list open (Show per
/// box, Show all), the sidebar marking the hidden rows.
#[test]
fn hidden_area_list() {
    let mut doc = family(false);
    for id in ["c2", "c3"] {
        doc.node_mut(id).unwrap().hidden = true;
    }
    let mut app = app(doc, false);
    let mut waiting = summary("c3", Activity::Working, true);
    waiting.pending = 1;
    app.sessions.insert("c3".into(), waiting);
    app.run(Action::Visual(canvas_core::visual::VisualAction::HiddenList(Some("p".into()), false)));
    check("hidden-area-list", &mut app, BOTH);
}

/// Effects in reduced-motion mode: fireworks on a box, attention on another,
/// an alert on a group (badges in the TUI, still shapes in the GUI).
#[test]
fn effects_in_reduced_motion() {
    let mut doc = Doc::default();
    doc.nodes.push(node("e1", "Payments API", 2, 2));
    doc.nodes.push(node("e2", "Billing UI", 2, 9));
    doc.groups.push(Group { id: "g-pay".into(), name: "Payments".into(), x: 0, y: 0, w: 48, h: 16, color: 2, folder: Some(r"C:\work\pay".into()), source: None, defaults: None });
    doc.view.x = -1;
    doc.view.y = -1;
    doc.ui.terminal_percent = 30;
    let mut app = app(doc, false);
    app.visual.motion = Some(canvas_core::view::Motion::Reduced);
    app.frozen_tick = Some(0);
    let effect = |id: &str, kind: &str, target: &str, note: &str| canvas_core::effects::Effect {
        id: id.into(),
        kind: kind.into(),
        group: "g-pay".into(),
        target: target.into(),
        note: note.into(),
        from: "Rollout tracker".into(),
        ms: 1,
        until_ms: u64::MAX,
    };
    app.bus.effects = vec![effect("fx-a", "fireworks", "e1", "PR !12 approved"), effect("fx-b", "attention", "e2", ""), effect("fx-c", "alert", "", "rollout stuck")];
    check("effects-reduced-motion", &mut app, BOTH);
}

/// The Reports inbox: a session's unread morning brief (Markdown), a thread
/// of two updates about a pull request (queued, then delivered), a read HTML
/// triage report from a scheduled task, and a report refused after it was
/// queued; the activity bar counts the unread rows.
fn inbox_app() -> App {
    use canvas_core::reports_model::{Format, Link, Rejected, Report, Snapshot};
    let mut doc = canvas_core::demo::demo_doc(CWD);
    doc.nodes.push(node("s-fix", "Fix flaky test", 2, 30));
    let mut app = app(doc, true);
    // A connection whose requests (read state) are kept, as the service would take them.
    let (tx, rx) = crossbeam_channel::bounded(256);
    std::mem::forget(rx);
    let (_service, service_rx) = crossbeam_channel::bounded(1);
    app.conn = canvas_core::conn::Conn::detached(tx, service_rx);
    app.visual.clock_ms = Some(NOW);
    let minutes = |m: u64| NOW - m * 60_000;
    let report = |id: &str, title: &str, format: Format, kind: &str, ago: u64, read: bool| Report {
        id: id.into(),
        title: title.into(),
        format,
        kind: kind.into(),
        created_ms: minutes(ago),
        stored_ms: minutes(ago),
        source: "s-fix".into(),
        source_name: "Fix flaky test".into(),
        source_kind: "session".into(),
        read_ms: if read { minutes(1) } else { 0 },
        ..Default::default()
    };
    let mut brief = report("0000000000005-aaaaaaaa", "Morning brief · day 100", Format::Markdown, "warn", 4, false);
    brief.preview = "Morning brief Needs you PR 1234 review requested".into();
    brief.links = vec![Link { title: "Board".into(), url: "https://example.com/board".into() }];
    brief.pr = "https://github.com/org/repo/pull/1234".into();
    let mut delivered = report("0000000000004-bbbbbbbb", "PR 12: the owning session got the 12 new comments", Format::Message, "delivered", 30, false);
    delivered.thread = "pr-12-comments".into();
    delivered.session = "demo-review".into();
    let mut queued = report("0000000000003-cccccccc", "PR 12 received 12 new comments; dispatched the owning session", Format::Message, "queued", 45, true);
    queued.thread = "pr-12-comments".into();
    let mut triage = report("0000000000002-dddddddd", "Daily triage", Format::Html, "info", 600, true);
    triage.source = String::new();
    triage.source_name = "Daily triage".into();
    triage.source_kind = "external".into();
    triage.preview = "Daily triage 4 new bugs, 2 assigned".into();
    let reports = vec![brief, delivered, queued, triage];
    let rejected = vec![Rejected { id: "0000000000001-eeeeeeee".into(), title: "Weekly digest".into(), reason: "the report is 2.4 MiB; at most 2 MiB".into(), ms: minutes(90) }];
    let inbox = Snapshot { version: 3, total: 4, unread: 2, reports, rejected };
    app.on_reports(&serde_json::to_string(&inbox).unwrap());
    app
}

#[test]
fn reports_inbox() {
    let mut app = inbox_app();
    app.set_view(canvas_core::side::SideView::Reports);
    check("reports-inbox", &mut app, BOTH);
}

/// The ✦ Skills view (every agent): the canvas plugin's skills, a group's
/// skill folder, the project of the canvas's sessions (each skill with the
/// agents that read its folder; `≈` where one also has a skill of its name),
/// two agents' personal folders and the canvas library; the project's
/// `review` is selected, its details on the right.
#[test]
fn skills_view_with_every_source() {
    use canvas_core::agents::AgentKind::{Claude, Copilot, OpenCode};
    use canvas_model::skills::{SkillSource, SourceKind, SourceSkill};
    let mut doc = Doc { canvas_id: Some("canvas-1".into()), ..Default::default() };
    doc.nodes.push(node("s-fix", "Fix flaky test", 2, 2));
    let defaults = canvas_core::doc::LaunchDefaults { skill_dirs: vec![r"C:\team\skills".into()], ..Default::default() };
    doc.groups.push(Group { id: "g1".into(), name: "Payments".into(), x: 0, y: 0, w: 40, h: 10, folder: Some(r"C:\notes\Payments".into()), defaults: Some(defaults), ..Default::default() });
    let mut app = app(doc, false);
    app.set_view(canvas_core::side::SideView::Skills);
    // The listing as the backend answers it (no background work in a snapshot).
    app.skills.loading = false;
    app.configs_loading.clear();
    let skill = |name: &str, folder: &str, agents: &[canvas_core::agents::AgentKind], about: &str| SourceSkill {
        name: name.into(),
        description: about.into(),
        path: format!(r"{folder}\{name}\SKILL.md"),
        agents: agents.to_vec(),
        folder: folder.into(),
        ..Default::default()
    };
    let all = [Copilot, OpenCode, Claude];
    let source = |kind: SourceKind, path: &str, skills: Vec<SourceSkill>| SkillSource { agents: canvas_model::skills::agents_of(&skills), kind, path: path.into(), folders: vec![(path.into(), skills.len())], skills, problem: String::new() };
    let plugin = r"C:\data\plugin\skills";
    let project = r"C:\work\canvas";
    app.skills.sources = Some(Ok(vec![
        source(SourceKind::Canvas, plugin, vec![skill("canvas-events", plugin, &all, "Send and react to canvas events."), skill("canvas-notes", plugin, &all, "Write notes for agents and people.")]),
        source(SourceKind::Group { id: "g1".into(), name: "Payments".into() }, r"C:\team\skills", vec![skill("payments-review", r"C:\team\skills", &all, "How payments changes are reviewed.")]),
        source(
            SourceKind::Project { root: project.into() },
            project,
            vec![
                skill("review", &format!(r"{project}\.claude\skills"), &all, "Review a change against the project's conventions before it is pushed."),
                skill("release", &format!(r"{project}\.agents\skills"), &[Copilot, OpenCode], "Cut a release."),
                skill("ci", &format!(r"{project}\.github\skills"), &[Copilot], "Fix the CI."),
            ],
        ),
        source(SourceKind::Personal { agent: Copilot }, r"C:\Users\me\.copilot\skills", vec![skill("standup", r"C:\Users\me\.copilot\skills", &[Copilot], "Write a standup.")]),
        source(SourceKind::Personal { agent: OpenCode }, r"C:\Users\me\.config\opencode\skills", vec![skill("review", r"C:\Users\me\.claude\skills", &[OpenCode, Claude], "My own review checklist.")]),
        source(SourceKind::Library, r"C:\data\canvas-agents\canvas-1\skills", vec![skill("pr-etiquette", r"C:\data\canvas-agents\canvas-1\skills", &all, "Route PR feedback.")]),
    ]));
    let rows = app.build_list();
    let at = rows.iter().position(|r| r.label == "review" && matches!(&r.item, canvas_core::side::ItemRef::SkillOf(key, _) if key.starts_with("project:"))).expect("the project's review");
    app.list_sel = at;
    app.right_detail = Some(canvas_core::side::RightDetail::Item(rows[at].item.clone()));
    check("skills-view", &mut app, BOTH);
}

/// The viewer on the morning brief: who and when, the session it came from
/// (viewed, never started), its pull request and links, the Markdown
/// formatted (headings, emphasis, a list, a table, code, a quote, a link).
#[test]
fn report_viewer_markdown() {
    let mut app = inbox_app();
    app.set_view(canvas_core::side::SideView::Reports);
    app.run(Action::Report(canvas_core::reports::ReportAction::Open("0000000000005-aaaaaaaa".into())));
    let body = "# Morning brief · day 100\n\n## Needs you\n\n- **PR 1234** review requested by *Ana*: [open it](https://github.com/org/repo/pull/1234)\n- Build `main` failed twice overnight\n\n## Status\n\n| Area | State | Note |\n|---|---|---|\n| Rollout | ring 2 | no incidents |\n| Triage | 4 new | 2 assigned |\n\n```\ncargo test -p upload\n```\n\n> Nothing else changed since yesterday.\n";
    if let Some(canvas_core::dialogs::Modal::Report(viewer)) = app.modals.last_mut() {
        viewer.content = Some(Arc::new(canvas_core::reports_model::Content { id: viewer.id.clone(), format: canvas_core::reports_model::Format::Markdown, body: body.into() }));
    }
    check("report-viewer-markdown", &mut app, BOTH);
}

/// An HTML report: the TUI shows it as formatted text and says so; the GPU
/// view model carries the sanitized page for the GUI's HTML view over the
/// content area.
#[test]
fn report_viewer_html() {
    let mut app = inbox_app();
    app.run(Action::Report(canvas_core::reports::ReportAction::Open("0000000000002-dddddddd".into())));
    let body = "<html><head><style>h1{color:#ff00ff}</style><script>document.title='x'</script></head><body><h1>Daily triage</h1><p><b>4</b> new bugs, <i>2</i> assigned.</p><ul><li>Upload fails on large files</li><li>Crash on resume</li></ul><img src=\"https://example.com/beacon.png\" alt=\"chart\"></body></html>";
    if let Some(canvas_core::dialogs::Modal::Report(viewer)) = app.modals.last_mut() {
        viewer.content = Some(Arc::new(canvas_core::reports_model::Content { id: viewer.id.clone(), format: canvas_core::reports_model::Format::Html, body: body.into() }));
    }
    check("report-viewer-html", &mut app, BOTH);
}

#[test]
fn snapshot_diff_marks_changed_lines() {
    let expected = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    let actual = "a\nb\nc\nd\ne\nF\ng\nh\ni\nj\nk\n";
    let diff = snapshot::diff(expected, actual);
    assert!(diff.starts_with("1 line(s) removed, 2 added"), "{diff}");
    assert!(diff.contains("-    6      │ f") && diff.contains("+         6 │ F") && diff.contains("+        11 │ k"), "{diff}");
    assert!(!diff.contains("│ a"), "context is limited to three lines: {diff}");
    assert_eq!(snapshot::normalize("x  \r\ny\r\n\r\n"), "x\ny\n");
}

// ---------- the Notebook with a service ----------

/// The service's machine for the Notebook snapshots: notes in memory.
#[derive(Default)]
struct Notes {
    files: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

const NB_ROOT: &str = r"C:\nb";
const NB_CANVAS: &str = r"C:\home\canvas-agents\c1";

impl Notes {
    fn put(&self, path: &str, text: &str) {
        self.files.lock().unwrap().insert(path.to_owned(), text.to_owned());
    }

    fn text(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }

    /// A folder's entries: its subfolders (from the files' paths), then its files.
    fn list(&self, dir: &str, root: &str) -> Vec<canvas_model::notebook::Entry> {
        use canvas_model::notebook::{self as nb, Entry, Kind, Meta, Place};
        let place = if dir.starts_with(NB_CANVAS) { Place::CanvasWide } else { Place::Notes };
        let rel = |path: &str| path.get(root.len() + 1..).unwrap_or_default().replace('\\', "/");
        let prefix = format!("{dir}\\");
        let files = self.files.lock().unwrap();
        let mut dirs: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for (path, text) in files.iter().filter(|(p, _)| p.starts_with(&prefix)) {
            let rest = &path[prefix.len()..];
            match rest.split_once('\\') {
                Some((sub, _)) => {
                    let sub = format!("{prefix}{sub}");
                    if !dirs.contains(&sub) {
                        dirs.push(sub);
                    }
                }
                None => {
                    let kind = Kind::of(rest);
                    let meta = nb::note_meta(kind, text);
                    out.push(Entry { name: rest.into(), path: path.clone(), rel: rel(path), kind, size: text.len() as u64, modified_ms: 0, provenance: nb::classify(&rel(path), &meta, place), title: nb::title(kind, text, &meta, rest) });
                }
            }
        }
        let folders = dirs.iter().map(|d| {
            let name = d.rsplit('\\').next().unwrap_or_default().to_owned();
            let rel = format!("{}/", rel(d));
            Entry { name: name.clone(), path: d.clone(), rel: rel.clone(), kind: Kind::Folder, provenance: nb::classify(&rel, &Meta::default(), place), title: name, ..Entry::default() }
        });
        folders.chain(out).collect()
    }
}

impl canvas_core::rpc::Backend for Notes {
    fn call(&self, request: canvas_core::rpc::Request) -> anyhow::Result<canvas_core::rpc::Response> {
        use canvas_core::rpc::{Request, Response};
        use canvas_model::notebook::{Hit, Info, NoteText, rev_of};
        let rev = |text: &str| rev_of(text.as_bytes());
        Ok(match request {
            Request::NotebookInfo { .. } => Response::NotebookInfo(Info { root: NB_ROOT.into(), canvas_dir: Some(NB_CANVAS.into()), home: r"C:\home".into() }),
            Request::NotebookList { dir, root } => Response::NoteEntries(self.list(&dir, &root.unwrap_or_else(|| dir.clone()))),
            Request::NoteRead { path } => match self.text(&path) {
                Some(text) => Response::Note(NoteText { rev: rev(&text), text }),
                None => Response::Error("not found".into()),
            },
            Request::NoteWrite { path, text, base, .. } => match self.text(&path) {
                Some(now) if base.as_deref() != Some(rev(&now).as_str()) => Response::NoteConflict(NoteText { rev: rev(&now), text: now }),
                // As the service: a write based on a revision needs the file.
                None if base.is_some() => Response::Error(format!("{path} was moved or deleted; save it as a new note")),
                _ => {
                    self.put(&path, &text);
                    Response::NoteSaved { rev: rev(&text) }
                }
            },
            Request::NoteRevs { paths } => Response::NoteRevs(paths.iter().map(|p| self.text(p).map(|t| rev(&t))).collect()),
            Request::NoteSearch { query, .. } => {
                let query = query.to_lowercase();
                let files = self.files.lock().unwrap();
                let hits = files.iter().flat_map(|(path, text)| text.lines().enumerate().filter(|(_, l)| l.to_lowercase().contains(&query)).map(|(i, l)| Hit { path: path.clone(), line: i as u32 + 1, text: l.to_owned() }).collect::<Vec<_>>()).collect();
                Response::NoteHits(hits)
            }
            other => Response::Error(format!("not here: {}", other.name())),
        })
    }
}

/// [`spaces_app`]'s canvas (the Docs group's folder is `C:\work\docs`)
/// with a notebook: a note for you with headings, a todo id and a link; an
/// agent's plan written by "Write the spec"; notes linking back.
fn notebook_app() -> (App, crossbeam_channel::Receiver<canvas_core::app::AppEvent>, Arc<Notes>) {
    let notes = Arc::new(Notes::default());
    notes.put(r"C:\nb\Overview.md", "# Overview\n\nThe plan is in [[plan]]; fix DEC-031 first.\n\n## Steps\n\n- Read the **plan**\n- Write the `spec`\n\n## Open questions\n\n> Who reviews it?\n");
    notes.put(r"C:\nb\Inbox\2026-10-31 0905.md", "---\naudience: human\nauthor: human\n---\nCall the printer people.\n");
    notes.put(r"C:\work\docs\ai-docs\plan.md", "---\naudience: ai\nauthor: ai\nagent: opencode\nsource_session: a\n---\n# Plan\n\nSee [[Overview]].\n\n1. Draft\n2. Review\n");
    notes.put(r"C:\work\docs\ai-docs\log.md", "# Log\n\nStarted.\n");
    notes.put(r"C:\work\docs\human-outputs\summary.md", "# Summary\n\nBack to [[Overview]].\n");
    let mut base = spaces_app();
    let (tx, _rx) = crossbeam_channel::bounded(64);
    let (_service, service_rx) = crossbeam_channel::bounded(1);
    let (app_tx, events) = crossbeam_channel::unbounded();
    let conn = canvas_core::conn::Conn::detached(tx, service_rx);
    let doc = std::mem::take(&mut base.doc);
    let mut app = App::new(conn, notes.clone(), canvas_core::sync::Opened::detached(doc), false, CWD.into(), app_tx, None);
    app.pan_quiet_until = None;
    app.tick = 0;
    app.visual.motion = Some(canvas_core::view::Motion::On);
    app.installed_agents = vec![AgentKind::Copilot, AgentKind::OpenCode];
    app.detected_agent = AgentKind::Copilot;
    app.tasks = std::mem::take(&mut base.tasks);
    app.bus.todos = base.bus.todos.take();
    app.selection.clear();
    (app, events, notes)
}

/// Runs the notebook's background answers until `done`.
fn settle(app: &mut App, events: &crossbeam_channel::Receiver<canvas_core::app::AppEvent>, done: impl Fn(&App) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(app) {
        assert!(std::time::Instant::now() < deadline, "the notebook's background work did not finish");
        if let Ok(event) = events.recv_timeout(std::time::Duration::from_millis(20)) {
            app.on_app_event(event);
        }
    }
}

/// Opens a note and waits until it, its folders and its backlinks are there.
fn open_note(app: &mut App, events: &crossbeam_channel::Receiver<canvas_core::app::AppEvent>, path: &str) {
    app.navigate(canvas_core::spaces::Space::Notebook, canvas_core::spaces::NavTarget::Note(path.into()));
    let area = Rect::new(0, 0, SCREEN.0, SCREEN.1);
    let path = path.to_owned();
    let ready = |app: &App| {
        let nb = &app.notebook;
        nb.info.is_some() && nb.tab().is_some_and(|t| t.loaded && t.path == path) && nb.folders.values().all(|f| f.at.is_some() && !f.loading) && nb.backlinks.values().all(|b| !b.loading && b.at.is_some())
    };
    for _ in 0..4 {
        canvas_core::ui::render(app, &mut Buffer::empty(area), area, None);
        settle(app, events, ready);
    }
    app.notice = None;
}

fn key(app: &mut App, code: canvas_core::input::KeyCode, modifiers: canvas_core::input::Modifiers) {
    app.on_input(vec![canvas_core::input::InputEvent::Key(canvas_core::input::KeyEvent::new(code, modifiers))]);
}

/// A note for you in the reader, Rendered (TUI cells; the GUI gets the
/// page as an HTML pane): breadcrumb, view switch, provenance pill
/// (inferred), buttons; the explorer with the notebook root, the Docs
/// group's folder (◫ ☑) and the canvas-wide folder; the info panel with
/// outline, links (todo, note), backlinks and provenance.
#[test]
fn notebook_reader_rendered_with_explorer_and_info() {
    let (mut app, events, _notes) = notebook_app();
    open_note(&mut app, &events, r"C:\nb\Overview.md");
    check("notebook-reader", &mut app, BOTH);
}

/// An agent's plan in Source mode (the editor has the keyboard): the pill
/// says who it is for, who wrote it and in which session, with View
/// session and Mark reviewed.
#[test]
fn notebook_source_mode_on_an_agents_note() {
    let (mut app, events, _notes) = notebook_app();
    open_note(&mut app, &events, r"C:\work\docs\ai-docs\plan.md");
    key(&mut app, canvas_core::input::KeyCode::Char('e'), canvas_core::input::Modifiers::CONTROL);
    app.focus = Focus::Canvas;
    check("notebook-source", &mut app, BOTH);
}

/// The Human filter: the agents' folder (only notes for agents) disappears
/// and the explorer says how many notes it hides.
#[test]
fn notebook_human_filter_hides_agents_notes() {
    let (mut app, events, _notes) = notebook_app();
    open_note(&mut app, &events, r"C:\work\docs\human-outputs\summary.md");
    open_note(&mut app, &events, r"C:\work\docs\ai-docs\log.md");
    app.focus = Focus::Sidebar;
    key(&mut app, canvas_core::input::KeyCode::Char('h'), canvas_core::input::Modifiers::ALT);
    key(&mut app, canvas_core::input::KeyCode::Char('h'), canvas_core::input::Modifiers::ALT);
    assert_eq!(app.notebook.filter, canvas_core::notebook::Filter::Human);
    check("notebook-human-filter", &mut app, BOTH);
}

/// Typed into a note that then changed on disk: the conflict banner with
/// Compare, Keep mine and Take theirs.
#[test]
fn notebook_conflict_banner() {
    let (mut app, events, notes) = notebook_app();
    open_note(&mut app, &events, r"C:\nb\Overview.md");
    app.focus = Focus::Canvas;
    key(&mut app, canvas_core::input::KeyCode::Char('e'), canvas_core::input::Modifiers::CONTROL);
    key(&mut app, canvas_core::input::KeyCode::Char('!'), canvas_core::input::Modifiers::NONE);
    notes.put(r"C:\nb\Overview.md", "# Overview\n\nChanged elsewhere.\n");
    canvas_core::notebook::tick(&mut app);
    settle(&mut app, &events, |app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    check("notebook-conflict", &mut app, BOTH);
}

/// Typed into a note whose file then went (moved or deleted elsewhere):
/// the banner says so, and that saving writes it again (it does: the write
/// asks for a new file).
#[test]
fn notebook_missing_file_banner() {
    let (mut app, events, notes) = notebook_app();
    open_note(&mut app, &events, r"C:\nb\Overview.md");
    app.focus = Focus::Canvas;
    key(&mut app, canvas_core::input::KeyCode::Char('e'), canvas_core::input::Modifiers::CONTROL);
    key(&mut app, canvas_core::input::KeyCode::Char('!'), canvas_core::input::Modifiers::NONE);
    notes.files.lock().unwrap().remove(r"C:\nb\Overview.md");
    canvas_core::notebook::tick(&mut app);
    settle(&mut app, &events, |app| app.notebook.tab().is_some_and(|t| t.missing) && app.notebook.folders.values().all(|f| !f.loading));
    check("notebook-missing", &mut app, BOTH);
    canvas_core::notebook::flush(&mut app);
    assert!(notes.text(r"C:\nb\Overview.md").is_some_and(|t| t.starts_with("!# Overview")), "written again");
}
