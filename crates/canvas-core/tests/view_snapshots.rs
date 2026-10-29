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
        agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
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
    let area = Rect::new(0, 0, SCREEN.0, SCREEN.1);
    let mut out = vec![format!("# {name} ({}x{} cells)", SCREEN.0, SCREEN.1)];
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
/// (dashed connector), a looping session and a queued passive message.
#[test]
fn subagents_subsessions_and_a_loop() {
    let mut doc = Doc::default();
    doc.nodes.push(node("orch", "Orchestrator", 1, 1));
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

/// Two boxes selected while a right-drag rubber band is open.
#[test]
fn selection_and_rubber_band() {
    let mut app = demo_at(0, 1.0);
    app.selection = vec!["demo-main".into(), "demo-review".into()];
    app.drag = Some(Drag::Rubber { start: (38, 3), now: (70, 14), additive: false, right: true });
    app.drag_origin = (38.5, 3.5);
    app.pointer = (70.5, 14.5);
    check("selection-rubber-band", &mut app, BOTH);
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
