//! Screen layout and drawing of the toolbar, sidebar, terminal tiles and status bar.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};

use crate::app::{App, Focus, SideKind, SideRow};
use crate::dialogs::{Action, Hit};
use crate::doc::{Node, NodeKind};
use crate::paint::{
    ACCENT, DARK, FAINT, GRAY, HEAVY, LIGHT, MUTED, PANEL, PANEL_HI, NEGATIVE, ROUND, SELECT, TEXT, ATTENTION, bold, fg, fill, outline, put,
    put_sym, truncate, truncate_left,
};
use crate::status::{Status, StatusStyle};
use crate::paths::now_ms;
use crate::view::{RenderMode, View};

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let view = render(app, frame.buffer_mut(), area, None);
    if let Some((x, y)) = view.cursor {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Draws one frame of `screen` into `buf`. In Gpu mode (`app.render_mode`)
/// the canvas region and the terminal grids stay blank and are described by
/// the returned [`View`]; dialogs and menus go to `overlay` when given.
pub fn render(app: &mut App, buf: &mut Buffer, screen: Rect, overlay: Option<&mut Buffer>) -> View {
    let gpu = app.render_mode == RenderMode::Gpu;
    let mut view = View { banner: app.banner.clone(), ..View::default() };
    app.layout.hits.clear();
    app.layout.links.clear();
    app.layout.modal_rect = None;
    app.layout.html_pane = None;
    app.layout.screen = screen;
    if screen.width < 60 || screen.height < 16 {
        put(buf, 1, 1, "Copilot Canvas needs at least 60×16 cells. Ctrl+Q quits.", fg(ATTENTION), screen);
        return view;
    }
    let toolbar = Rect::new(0, 0, screen.width, 1);
    let status = Rect::new(0, screen.height - 1, screen.width, 1);
    let body = Rect::new(0, 1, screen.width, screen.height - 2);
    // VS Code-style activity bar, always visible.
    let bar = Rect::new(0, body.y, crate::ui_side::BAR_W, body.height);
    // The Reports inbox needs room for titles and who sent them.
    let wanted = if app.side_view == crate::side::SideView::Reports { app.doc.ui.sidebar_width.max(46) } else { app.doc.ui.sidebar_width };
    let sidebar_w = if app.doc.ui.sidebar_visible { wanted.clamp(18, body.width / 2) } else { 0 };
    let sidebar = Rect::new(bar.right(), body.y, sidebar_w, body.height);
    let rest_x = bar.right() + if sidebar_w > 0 { sidebar_w + 1 } else { 0 };
    let rest_w = body.width - rest_x;
    let want_terms = !app.visible_terminals().is_empty() || app.selection.last().is_some() || app.selected_group.is_some() || app.right_detail.is_some();
    let term_w = if want_terms { (rest_w as u32 * app.doc.ui.terminal_percent as u32 / 100) as u16 } else { 0 }.min(rest_w.saturating_sub(20));
    let canvas_w = rest_w - term_w - if term_w > 0 { 1 } else { 0 };
    let canvas = Rect::new(rest_x, body.y, canvas_w, body.height);
    let terms = Rect::new(rest_x + canvas_w + if term_w > 0 { 1 } else { 0 }, body.y, term_w, body.height);
    app.layout.toolbar = toolbar;
    app.layout.status = status;
    app.layout.sidebar = sidebar;
    app.layout.canvas = canvas;
    app.layout.terms = terms;
    app.layout.sidebar_divider = (sidebar_w > 0).then_some(sidebar.right());
    app.layout.term_divider = (term_w > 0).then_some(canvas.right());

    let mut mark = std::time::Instant::now();
    let mut phase = |app: &mut App, name: &'static str| {
        if let Some(perf) = &mut app.perf {
            perf.phase(name, mark.elapsed());
        }
        mark = std::time::Instant::now();
    };
    phase(app, "layout");
    draw_toolbar(app, buf, toolbar);
    crate::ui_side::draw_activity_bar(app, buf, bar);
    phase(app, "toolbar");
    if sidebar_w > 0 {
        if app.side_view == crate::side::SideView::Sessions {
            draw_sidebar(app, buf, sidebar);
        } else {
            refresh_side_rows(app);
            crate::ui_side::draw_list(app, buf, sidebar);
        }
        for y in body.y..body.bottom() {
            put_sym(buf, sidebar.right() as i32, y as i32, "│", fg(if app.focus == Focus::Sidebar { ACCENT } else { FAINT }), screen);
        }
    } else {
        refresh_side_rows(app);
        app.layout.sidebar_rows.clear();
    }
    phase(app, "sidebar");
    if gpu {
        view.canvas = Some(crate::ui_canvas::canvas_model(app, canvas));
    } else {
        crate::ui_canvas::draw_canvas(app, buf, canvas);
    }
    phase(app, "canvas");
    if app.focus == Focus::Canvas {
        put(buf, canvas.x as i32, canvas.y as i32, "▌", fg(ACCENT), canvas);
    }
    if term_w > 0 {
        for y in body.y..body.bottom() {
            put_sym(buf, canvas.right() as i32, y as i32, "│", fg(FAINT), screen);
        }
    }
    let cursor = draw_terminals(app, buf, terms, &mut view);
    phase(app, "terminals");
    // Plain-text URLs and paths: terminals first (Ctrl+click), then everything
    // else. (Gpu mode scanned each terminal before blanking its grid.)
    let inners: Vec<Rect> = app.layout.tiles.iter().map(|(_, _, inner)| *inner).collect();
    let mut links = std::mem::take(&mut app.layout.links);
    if !gpu {
        for inner in &inners {
            crate::links::scan(buf, *inner, true, &[], &mut links);
        }
    }
    crate::links::scan(buf, body, false, &inners, &mut links);
    app.layout.links = links;
    phase(app, "links");
    draw_status(app, buf, status);
    let modal_cursor = match overlay {
        Some(overlay) if gpu => crate::ui_modals::draw_modals(app, overlay, screen),
        _ => crate::ui_modals::draw_modals(app, buf, screen),
    };
    phase(app, "status+modals");
    view.cursor = modal_cursor.or(cursor);
    view.modal = app.layout.modal_rect;
    view.html = app.layout.html_pane.clone().map(|(rect, id, document)| crate::view::HtmlPane { rect, id, document });
    view
}

fn toolbar_button(app: &mut App, buf: &mut Buffer, x: i32, label: &str, action: Action, area: Rect) -> i32 {
    let end = put(buf, x, area.y as i32, label, Style::default().fg(TEXT).bg(PANEL_HI), area);
    app.layout.hits.push((Rect::new(x as u16, area.y, (end - x).max(0) as u16, 1), Hit::Toolbar(action)));
    end + 1
}

/// The canvas-wide agents' entry: their count, a spinner while one works,
/// gold while one needs attention (or cannot run).
pub fn agents_button_label(app: &App) -> (String, bool) {
    let states: Vec<crate::canvas_wide::AgentState> = app.canvas_agents().iter().map(|n| app.canvas_agent_state(n)).collect();
    let attention = states.iter().filter(|s| s.needs_attention()).count();
    let working = states.iter().any(|s| matches!(s, crate::canvas_wide::AgentState::Working(_)));
    let mark = if working { crate::paint::SPINNER[app.tick % crate::paint::SPINNER.len()] } else { crate::canvas_wide::MARK };
    let label = match (states.len(), attention) {
        (0, _) => format!(" {mark} Agents "),
        (n, 0) => format!(" {mark} Agents {n} "),
        (n, a) => format!(" {mark} Agents {n} · ⚠ {a} "),
    };
    (label, attention > 0)
}

fn agents_button(app: &mut App, buf: &mut Buffer, x: i32, area: Rect) -> i32 {
    let (label, attention) = agents_button_label(app);
    let style = if attention { bold(PANEL).bg(ATTENTION) } else { Style::default().fg(crate::canvas_wide::COLOR).bg(PANEL_HI) };
    let end = put(buf, x, area.y as i32, &label, style, area);
    app.layout.hits.push((Rect::new(x as u16, area.y, (end - x).max(0) as u16, 1), Hit::Toolbar(Action::Agents(crate::canvas_wide::AgentAction::Panel))));
    end + 1
}

fn draw_toolbar(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, 1, Style::default().bg(PANEL), area);
    let mut x = put(buf, 1, area.y as i32, "◆ Copilot Canvas", bold(ACCENT).bg(PANEL), area) + 2;
    if app.demo {
        x = put(buf, x, area.y as i32, "DEMO · native shells", fg(ATTENTION).bg(PANEL), area) + 2;
    }
    if app.restarting.is_some() {
        x = put(buf, x, area.y as i32, "⟳ updating service…", bold(ATTENTION).bg(PANEL), area) + 2;
    } else if app.build_mismatch {
        let end = put(buf, x, area.y as i32, " ⟳ Update service ", bold(PANEL).bg(ATTENTION), area);
        app.layout.hits.push((Rect::new(x as u16, area.y, (end - x).max(0) as u16, 1), Hit::Toolbar(Action::UpdateService)));
        x = end + 2;
    }
    x = toolbar_button(app, buf, x, " + New ", Action::NewSession { at: None, parent: None }, area);
    if !app.demo {
        x = toolbar_button(app, buf, x, " ⇩ Import ", Action::ImportCopilot, area);
        x = toolbar_button(app, buf, x, " ⟳ VS Code ", Action::SyncVs, area);
        x = agents_button(app, buf, x, area);
    }
    x = toolbar_button(app, buf, x, if app.doc.ui.tiled { " ▦ Tiled " } else { " ▢ Single " }, Action::ToggleTiled, area);
    x = toolbar_button(app, buf, x, " Show all nodes ", Action::FitAll, area);
    x = toolbar_button(app, buf, x, " − ", Action::Zoom(1), area);
    x = toolbar_button(app, buf, x, " + ", Action::Zoom(-1), area);
    let mut x = toolbar_button(app, buf, x, " ? ", Action::Help, area);
    if app.render_mode == crate::view::RenderMode::Gpu {
        x = toolbar_button(app, buf, x, " ⧉ ", Action::NewWindow, area);
    }
    let mut x = toolbar_button(app, buf, x, " ⏻ Quit ", Action::Quit, area);
    // Subsessions and subagent boxes are hidden: say how many, and show them with a click.
    if app.doc.ui.hide_subsessions {
        let hidden = canvas_model::visual::hidden_subsessions(&app.doc).len();
        let end = put(buf, x, area.y as i32, &format!(" ↳ {hidden} hidden "), bold(PANEL).bg(GRAY), area);
        app.layout.hits.push((Rect::new(x as u16, area.y, (end - x).max(0) as u16, 1), Hit::Toolbar(Action::Visual(crate::visual::VisualAction::ToggleHideSubsessions))));
        x = end + 1;
    }
    let buttons_end = x;

    // Status counts on the right, never over the buttons.
    let mut counts: HashMap<Status, usize> = HashMap::new();
    for node in app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
        *counts.entry(app.node_status(node)).or_default() += 1;
    }
    let sessions = app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).count();
    let parts = toolbar_counts(app.failing_automations().len(), &counts, sessions, (area.right() as i32 - 2 - buttons_end).max(0) as usize);
    let total: usize = parts.iter().map(|p| crate::paint::text_width(&p.text) + 3).sum();
    let mut rx = area.right() as i32 - total as i32 - 1;
    for part in parts {
        if part.failing {
            let end = put(buf, rx, area.y as i32, &part.text, bold(PANEL).bg(NEGATIVE), area);
            app.layout.hits.push((Rect::new(rx.max(0) as u16, area.y, (end - rx).max(0) as u16, 1), Hit::Toolbar(Action::ShowAutomations)));
            rx = end + 3;
            continue;
        }
        rx = put(buf, rx, area.y as i32, &part.text, fg(part.color).bg(PANEL), area) + 3;
    }
}

/// One status count on the toolbar.
#[derive(Clone, Debug, PartialEq)]
pub struct CountPart {
    pub text: String,
    pub color: Color,
    /// The failing-automations badge (red, clickable).
    pub failing: bool,
}

/// The toolbar's status counts that fit in `room` cells (each takes its
/// width plus 3). The lowest-priority parts go first (sessions total, open
/// elsewhere, done · review, working, failed); then the rest is abbreviated
/// (icon and count); "needs you" and failing automations go last.
pub fn toolbar_counts(failing: usize, counts: &HashMap<Status, usize>, sessions: usize, room: usize) -> Vec<CountPart> {
    let width = |parts: &[(u8, CountPart, String)], short: bool| -> usize {
        parts.iter().map(|(_, p, s)| crate::paint::text_width(if short { s } else { &p.text }) + 3).sum()
    };
    // (drop rank: lower goes first, full part, abbreviation)
    let mut parts: Vec<(u8, CountPart, String)> = Vec::new();
    if failing > 0 {
        let text = format!(" ✗ {failing} automation{} failing ", if failing == 1 { "" } else { "s" });
        parts.push((9, CountPart { text, color: NEGATIVE, failing: true }, format!(" ✗ {failing} ")));
    }
    let rank = |s: Status| match s {
        Status::Waiting => 8,
        Status::Failed => 5,
        Status::Working => 4,
        Status::JustFinished => 3,
        _ => 2,
    };
    for status in [Status::Working, Status::Waiting, Status::JustFinished, Status::External, Status::Failed] {
        if let Some(count) = counts.get(&status).filter(|c| **c > 0) {
            let part = CountPart { text: format!("{} {count} {}", status.icon(0), status.label()), color: status.color(), failing: false };
            parts.push((rank(status), part, format!("{} {count}", status.icon(0))));
        }
    }
    parts.push((1, CountPart { text: format!("{sessions} sessions"), color: GRAY, failing: false }, format!("{sessions}")));
    let mut short = false;
    loop {
        if width(&parts, short) <= room {
            break;
        }
        // Drop the lowest-priority part until only the important ones are left, then abbreviate.
        let lowest = parts.iter().enumerate().filter(|(_, (r, _, _))| *r < 8).min_by_key(|(_, (r, _, _))| *r).map(|(i, _)| i);
        match (lowest, short) {
            (Some(i), _) => {
                parts.remove(i);
            }
            (None, false) => short = true,
            (None, true) => {
                if parts.is_empty() {
                    break;
                }
                // Even the abbreviations do not fit: "needs you" before the badge.
                let i = parts.iter().enumerate().min_by_key(|(_, (r, _, _))| *r).map(|(i, _)| i).unwrap_or(0);
                parts.remove(i);
            }
        }
    }
    parts
        .into_iter()
        .map(|(_, mut part, abbreviation)| {
            if short {
                part.text = abbreviation;
            }
            part
        })
        .collect()
}

/// FNV-1a: much cheaper than SipHash for the short id strings of the sidebar maps.
#[derive(Default)]
pub struct Fnv(u64);

impl std::hash::Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Keys are ids (UUIDs): length plus first and last 8 bytes identify them
        // in O(1); equality still compares whole keys, so collisions only cost time.
        let word = |b: &[u8]| b.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64);
        let n = bytes.len();
        self.write_u64(n as u64);
        self.write_u64(word(&bytes[..n.min(8)]));
        if n > 8 {
            self.write_u64(word(&bytes[n - 8..]));
        }
    }
    fn write_u64(&mut self, v: u64) {
        let h = if self.0 == 0 { 0xcbf29ce484222325 } else { self.0 };
        self.0 = (h ^ v).wrapping_mul(0x100000001b3).rotate_left(29);
    }
    fn write_i32(&mut self, v: i32) {
        self.write_u64(v as u64);
    }
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
    fn write_u8(&mut self, v: u8) {
        self.write_u64(v as u64);
    }
}

type FastMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fnv>>;
type FastSet<K> = std::collections::HashSet<K, std::hash::BuildHasherDefault<Fnv>>;

/// Rebuilds the sidebar tree only when something it shows changed (or once a
/// second, so time-based states such as "just finished" expire).
fn refresh_side_rows(app: &mut App) {
    use std::hash::{Hash, Hasher};
    let started = std::time::Instant::now();
    let mut h = Fnv::default();
    // Numbers only (cheap); renames that keep the length are picked up by the
    // once-a-second refresh below.
    h.write_usize(app.doc.nodes.len());
    for node in &app.doc.nodes {
        h.write_i32(node.x);
        h.write_i32(node.y);
        h.write_usize(node.name.len() * 2 + node.parent.is_some() as usize);
        h.write_u64(node.reviewed_ms.unwrap_or(0));
        h.write_u64(node.accessed_ms.unwrap_or(0));
    }
    for group in &app.doc.groups {
        (group.x, group.y, group.w, group.h, group.name.len()).hash(&mut h);
    }
    (&app.selection, &app.selected_group, &app.side_filter.text, &app.doc.ui.collapsed).hash(&mut h);
    (app.sessions_rev, app.terms_rev, app.preparing.len(), app.launching.len(), app.node_errors.len(), app.automations.len(), app.demo).hash(&mut h);
    (app.recent.hold_until.is_some(), app.recent_room()).hash(&mut h);
    (now_ms() / 1000).hash(&mut h);
    let key = h.finish();
    if let Some(perf) = &mut app.perf {
        perf.phase("side-key", started.elapsed());
    }
    if key != app.side_rows_key {
        let t = std::time::Instant::now();
        // The highlighted row stays on the same entry when rows come or go above it.
        let current = app.side_rows.get(app.side_sel).map(|r| r.kind.clone());
        app.side_rows = build_side_rows(app);
        app.side_rows_key = key;
        if let Some(index) = current.and_then(|kind| app.side_rows.iter().position(|r| r.kind == kind)) {
            app.side_sel = index;
        }
        if app.recent.hold_until.is_none() {
            app.recent.shown = app.side_rows.iter().filter_map(|r| if let SideKind::Recent(id) = &r.kind { Some(id.clone()) } else { None }).collect();
        }
        if let Some(perf) = &mut app.perf {
            perf.phase("side-build", t.elapsed());
        }
    }
}

/// The RECENT section (when anything was used yet): its header, the
/// recently used sessions newest first (unless folded), and a line before
/// the tree.
fn recent_rows(app: &App, rows: &mut Vec<SideRow>) {
    let recent = app.recent_ids();
    if recent.is_empty() {
        return;
    }
    let collapsed = app.doc.ui.collapsed.iter().any(|c| c == crate::recent::COLLAPSED);
    rows.push(SideRow { kind: SideKind::RecentHeader, label: "RECENT".into(), depth: 0, status: None, count: Some(recent.len()), collapsed });
    if !collapsed {
        for node in recent.iter().filter_map(|id| app.doc.node(id)) {
            rows.push(SideRow { kind: SideKind::Recent(node.id.clone()), label: node.name.clone(), depth: 0, status: Some(app.node_status(node)), count: None, collapsed: false });
        }
    }
    let sessions = app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).count();
    rows.push(SideRow { kind: SideKind::Section, label: "All sessions".into(), depth: 0, status: None, count: Some(sessions), collapsed: false });
}

/// Sidebar tree: groups (nested) → sessions → subsessions → active subagents.
pub fn build_side_rows(app: &App) -> Vec<SideRow> {
    let filter = app.side_filter.text.trim().to_lowercase();
    let mut rows = Vec::new();
    let session_row = |node: &Node, depth: u16| SideRow {
        kind: if node.kind == NodeKind::Agent { SideKind::Agent(node.id.clone()) } else { SideKind::Session(node.id.clone()) },
        label: node.name.clone(),
        depth,
        status: Some(app.node_status(node)),
        count: None,
        collapsed: false,
    };
    if !filter.is_empty() {
        for group in &app.doc.groups {
            if group.name.to_lowercase().contains(&filter) {
                rows.push(SideRow { kind: SideKind::Group(group.id.clone()), label: group.name.clone(), depth: 0, status: None, count: None, collapsed: true });
            }
        }
        for node in app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
            let summary = app.summary(node);
            let haystack = format!(
                "{} {} {} {}",
                node.name,
                node.cwd.as_deref().unwrap_or_default(),
                summary.map(|s| s.branch.as_str()).unwrap_or_default(),
                summary.map(|s| s.cwd.as_str()).unwrap_or_default()
            )
            .to_lowercase();
            if haystack.contains(&filter) {
                rows.push(session_row(node, 0));
            }
        }
        return rows;
    }
    // Recently used sessions first, then the tree.
    recent_rows(app, &mut rows);
    // Everything below is built once per frame so the tree costs O(boxes),
    // whatever the number of sessions, subagents and groups.
    // Only sessions/automations need their group (subagents follow their parent).
    let mut group_of: FastMap<&str, Option<&str>> = FastMap::with_capacity_and_hasher(app.doc.nodes.len() / 4 + 8, Default::default());
    for node in app.doc.nodes.iter().filter(|n| n.kind != NodeKind::Agent) {
        group_of.insert(node.id.as_str(), app.doc.group_of(node).map(|g| g.id.as_str()));
    }
    let parent_of_group: FastMap<&str, Option<&str>> =
        app.doc.groups.iter().map(|g| (g.id.as_str(), app.doc.parent_group(g).map(|p| p.id.as_str()))).collect();
    let mut sorted_groups: Vec<&crate::doc::Group> = app.doc.groups.iter().collect();
    sorted_groups.sort_by_key(|g| (g.y, g.x));
    let mut by_position: Vec<&Node> = app.doc.nodes.iter().collect();
    by_position.sort_by_key(|n| (n.y, n.x));
    let mut ids: FastSet<&str> = FastSet::with_capacity_and_hasher(app.doc.nodes.len(), Default::default());
    ids.extend(app.doc.nodes.iter().map(|n| n.id.as_str()));
    let sessions = app.doc.nodes.iter().filter(|n| n.kind != NodeKind::Agent).count();
    let mut children: FastMap<&str, Vec<&Node>> = FastMap::with_capacity_and_hasher(sessions, Default::default());
    let mut members_of: FastMap<&str, Vec<&Node>> = FastMap::with_capacity_and_hasher(app.doc.groups.len(), Default::default());
    for node in &by_position {
        if let Some(parent) = node.parent.as_deref() {
            children.entry(parent).or_default().push(node);
        }
        if matches!(node.kind, NodeKind::Session | NodeKind::Automation)
            && let Some(Some(group)) = group_of.get(node.id.as_str())
        {
            members_of.entry(group).or_default().push(node);
        }
    }
    // Sessions whose subagents are listed: the selected ones and parents of selected boxes.
    let mut expanded: FastSet<&str> = FastSet::default();
    for id in &app.selection {
        expanded.insert(id.as_str());
        if let Some(parent) = app.doc.node(id).and_then(|n| n.parent.as_deref()) {
            expanded.insert(parent);
        }
    }

    struct Tree<'a> {
        group_of: FastMap<&'a str, Option<&'a str>>,
        parent_of_group: FastMap<&'a str, Option<&'a str>>,
        groups: Vec<&'a crate::doc::Group>,
        children: FastMap<&'a str, Vec<&'a Node>>,
        members_of: FastMap<&'a str, Vec<&'a Node>>,
        ids: FastSet<&'a str>,
        expanded: FastSet<&'a str>,
    }

    fn emit_session<'a>(app: &'a App, node: &'a Node, depth: u16, rows: &mut Vec<SideRow>, tree: &Tree<'a>, row: &dyn Fn(&Node, u16) -> SideRow) {
        rows.push(row(node, depth));
        let expanded = tree.expanded.contains(node.id.as_str());
        // Subagents shown when the session is expanded or the subagent is working
        // (decided from the parent's summary once, not per subagent box).
        let summary = app.summary(node);
        let running = app.owned_alive(&node.id) || summary.is_some_and(|s| s.live);
        let agents: FastMap<&str, &crate::protocol::AgentSummary> = summary.map(|s| s.agents.iter().map(|a| (a.key.as_str(), a)).collect()).unwrap_or_default();
        let now = now_ms();
        for child in tree.children.get(node.id.as_str()).map(Vec::as_slice).unwrap_or_default() {
            match child.kind {
                NodeKind::Agent => {
                    let status = match child.agent_key.as_deref().and_then(|k| agents.get(k)) {
                        Some(agent) => crate::status::agent_status(agent.status, agent.finished_ms, running, now),
                        None if app.demo => Status::JustFinished,
                        None => Status::Unknown,
                    };
                    if expanded || status == Status::Working {
                        rows.push(SideRow {
                            kind: SideKind::Agent(child.id.clone()),
                            label: child.name.clone(),
                            depth: depth + 1,
                            status: Some(status),
                            count: None,
                            collapsed: false,
                        });
                    }
                }
                NodeKind::Session if tree.group_of.get(child.id.as_str()) == tree.group_of.get(node.id.as_str()) => {
                    emit_session(app, child, depth + 1, rows, tree, row);
                }
                NodeKind::Session | NodeKind::Automation => {}
            }
        }
    }

    fn emit_group<'a>(app: &'a App, group: &'a crate::doc::Group, depth: u16, rows: &mut Vec<SideRow>, tree: &Tree<'a>, row: &dyn Fn(&Node, u16) -> SideRow) {
        let collapsed = app.doc.ui.collapsed.contains(&group.id);
        let members = tree.members_of.get(group.id.as_str()).map(Vec::as_slice).unwrap_or_default();
        rows.push(SideRow { kind: SideKind::Group(group.id.clone()), label: group.name.clone(), depth, status: None, count: Some(members.len()), collapsed });
        if collapsed {
            return;
        }
        // The group's folders (a click opens their menu).
        if !group.is_automations() && !group.is_canvas_agents() {
            let label = match group.folder.as_deref() {
                Some(folder) => folder.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(folder).to_owned(),
                None => "Set folder…".to_owned(),
            };
            rows.push(SideRow { kind: SideKind::Folder(group.id.clone()), label, depth: depth + 1, status: None, count: None, collapsed: false });
        }
        for child in tree.groups.iter().filter(|g| tree.parent_of_group.get(g.id.as_str()).copied().flatten() == Some(group.id.as_str())) {
            emit_group(app, child, depth + 1, rows, tree, row);
        }
        for node in members.iter().filter(|n| n.passive.is_none()) {
            let parent_here = node.parent.as_deref().is_some_and(|p| tree.group_of.get(p) == tree.group_of.get(node.id.as_str()) && tree.ids.contains(p));
            if !parent_here {
                emit_session(app, node, depth + 1, rows, tree, row);
            }
        }
        // Its passive sessions, wherever their boxes are (a click opens their panel).
        for node in app.doc.nodes.iter().filter(|n| n.passive.as_ref().is_some_and(|p| p.group == group.id)) {
            rows.push(row(node, depth + 1));
        }
    }

    let tree = Tree { group_of, parent_of_group, groups: sorted_groups.clone(), children, members_of, ids, expanded };
    for group in sorted_groups.iter().filter(|g| tree.parent_of_group.get(g.id.as_str()).copied().flatten().is_none()) {
        emit_group(app, group, 0, &mut rows, &tree, &session_row);
    }
    let group_of = &tree.group_of;
    let ungrouped: Vec<&&Node> =
        by_position.iter().filter(|n| n.kind == NodeKind::Session && group_of.get(n.id.as_str()).copied().flatten().is_none() && !n.passive.as_ref().is_some_and(|p| app.doc.group(&p.group).is_some())).collect();
    if !ungrouped.is_empty() {
        if !app.doc.groups.is_empty() {
            rows.push(SideRow { kind: SideKind::Section, label: "Ungrouped".into(), depth: 0, status: None, count: Some(ungrouped.len()), collapsed: false });
        }
        for node in ungrouped {
            let parent_here = node.parent.as_deref().is_some_and(|p| tree.ids.contains(p) && group_of.get(p) == group_of.get(node.id.as_str()));
            if !parent_here {
                emit_session(app, node, 0, &mut rows, &tree, &session_row);
            }
        }
    }
    rows
}

fn draw_sidebar(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(PANEL), area);
    refresh_side_rows(app);
    let focused = app.focus == Focus::Sidebar;
    put(buf, area.x as i32 + 1, area.y as i32, "SESSIONS", bold(if focused { ACCENT } else { GRAY }).bg(PANEL), area);
    let filter_y = area.y + 1;
    app.layout.sidebar_filter_row = Some(filter_y);
    let filter_text = if app.side_filter.text.is_empty() && !app.side_filter_active {
        "/ filter…".to_owned()
    } else {
        format!("/ {}", app.side_filter.text)
    };
    let filter_style = if app.side_filter_active { fg(TEXT).bg(PANEL_HI) } else { fg(MUTED).bg(PANEL) };
    fill(buf, area.x as i32 + 1, filter_y as i32, area.width as i32 - 2, 1, filter_style, area);
    put(buf, area.x as i32 + 1, filter_y as i32, &truncate(&filter_text, area.width as usize - 2), filter_style, area);

    let list_top = area.y + 3;
    let visible = area.height.saturating_sub(3) as usize;
    let total = app.side_rows.len();
    app.side_sel = app.side_sel.min(total.saturating_sub(1));
    if app.side_sel < app.side_scroll {
        app.side_scroll = app.side_sel;
    } else if focused && app.side_sel >= app.side_scroll + visible {
        app.side_scroll = app.side_sel + 1 - visible;
    }
    app.side_scroll = app.side_scroll.min(total.saturating_sub(visible));
    app.layout.sidebar_rows.clear();
    let width = area.right() as i32;
    let concealed = app.concealed_now();
    for (i, row) in app.side_rows.iter().enumerate().skip(app.side_scroll).take(visible) {
        let y = list_top as i32 + (i - app.side_scroll) as i32;
        app.layout.sidebar_rows.push((y as u16, i));
        let is_selected = match &row.kind {
            SideKind::Group(id) => app.selected_group.as_deref() == Some(id),
            SideKind::Session(id) | SideKind::Agent(id) | SideKind::Recent(id) => app.selection.contains(id),
            SideKind::Section | SideKind::Folder(_) | SideKind::RecentHeader => false,
        };
        let highlight = focused && i == app.side_sel;
        let bg = if is_selected { PANEL_HI } else { PANEL };
        if is_selected || highlight {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(bg), area);
        }
        if highlight {
            put(buf, area.x as i32, y, "▎", fg(ACCENT).bg(bg), area);
        }
        let x = area.x as i32 + 1 + row.depth as i32 * 2;
        match &row.kind {
            SideKind::Section => {
                put(buf, area.x as i32 + 1, y, &format!("── {} ({}) ", row.label, row.count.unwrap_or(0)), fg(MUTED).bg(bg), area);
            }
            SideKind::Group(id) => {
                let group = app.doc.group(id);
                let frame = group.is_some_and(|g| g.is_canvas_agents());
                let color = group.map(|g| crate::paint::GROUP_COLORS[g.color as usize % 6]).unwrap_or(ACCENT);
                let caret = if row.collapsed { "▸" } else { "▾" };
                let end = put(buf, x, y, caret, fg(MUTED).bg(bg), area);
                let end = put(buf, end + 1, y, if frame { crate::canvas_wide::MARK } else { "▣" }, fg(color).bg(bg), area);
                let count = format!(" {}", row.count.unwrap_or(0));
                let room = (width - end - 2 - count.len() as i32).max(1) as usize;
                put(buf, end + 1, y, &truncate(&row.label, room), bold(TEXT).bg(bg), area);
                put(buf, width - count.len() as i32 - 1, y, &count, fg(MUTED).bg(bg), area);
            }
            SideKind::Folder(id) => {
                let set = app.doc.group(id).is_some_and(|g| g.folder.is_some());
                let color = app.doc.group(id).map(|g| crate::paint::GROUP_COLORS[g.color as usize % 6]).unwrap_or(ACCENT);
                let end = put(buf, x, y, "◧", fg(if set { color } else { MUTED }).bg(bg), area) + 1;
                put(buf, end, y, &truncate(&row.label, (width - end - 1).max(1) as usize), fg(if set { GRAY } else { MUTED }).bg(bg), area);
            }
            SideKind::RecentHeader => {
                // ▾ RECENT 5 … and the keys that walk it.
                let caret = if row.collapsed { "▸" } else { "▾" };
                let end = put(buf, x, y, caret, fg(MUTED).bg(bg), area) + 1;
                let end = put(buf, end, y, "RECENT", bold(GRAY).bg(bg), area);
                let end = put(buf, end + 1, y, &row.count.unwrap_or(0).to_string(), fg(MUTED).bg(bg), area);
                let hint = "Ctrl+F6 · [ ]";
                let hint_w = crate::paint::text_width(hint) as i32;
                if width - 1 - hint_w > end + 1 {
                    put(buf, width - 1 - hint_w, y, hint, fg(MUTED).bg(bg), area);
                }
            }
            SideKind::Recent(id) => {
                // Status icon and name, then the agent and how long ago.
                let status = row.status.unwrap_or(Status::Unknown);
                let node = app.doc.node(id);
                let end = put(buf, x, y, status.icon(app.tick), fg(status.color()).bg(bg), area) + 1;
                let agent = match node {
                    Some(n) if n.command.is_some() => "shell",
                    Some(n) => crate::agents::agent_of(n).id(),
                    None => "",
                };
                // "now" within a minute (no ticking seconds), then 2m, 3h, 5d.
                let now = app.visual_now();
                let age = node.and_then(|n| n.accessed_ms).map(|at| if now.saturating_sub(at) < 60_000 { "now".to_owned() } else { crate::paths::format_age(at, now) }).unwrap_or_default();
                let right = format!(" {agent} {age}");
                let right_w = crate::paint::text_width(&right) as i32;
                let style = match status.attention() {
                    _ if is_selected => bold(SELECT).bg(bg),
                    Some((color, _)) => bold(color).bg(bg),
                    _ => fg(TEXT).bg(bg),
                };
                let room = width - end - 1 - right_w;
                if room >= 6 {
                    put(buf, end, y, &truncate(&row.label, room as usize), style, area);
                    put(buf, width - right_w - 1, y, &right, fg(MUTED).bg(bg), area);
                } else {
                    put(buf, end, y, &truncate(&row.label, (width - end - 1).max(1) as usize), style, area);
                }
            }
            SideKind::Session(id) | SideKind::Agent(id) => {
                let status = row.status.unwrap_or(Status::Unknown);
                let agent = matches!(row.kind, SideKind::Agent(_));
                let node = app.doc.node(id);
                let passive = node.is_some_and(|n| n.passive.is_some());
                let sub = !agent && !passive && node.is_some_and(|n| n.parent.is_some());
                // Stacked or hidden subsessions are listed dimmed.
                let tucked = concealed.contains_key(id.as_str());
                let mut end = x;
                if agent {
                    end = put(buf, end, y, "◇", fg(MUTED).bg(bg), area) + 1;
                } else if node.is_some_and(|n| n.is_canvas_agent()) {
                    end = put(buf, end, y, crate::canvas_wide::MARK, fg(crate::canvas_wide::COLOR).bg(bg), area) + 1;
                } else if passive {
                    end = put(buf, end, y, "⟲", fg(MUTED).bg(bg), area) + 1;
                } else if sub {
                    end = put(buf, end, y, "↳", fg(if tucked { MUTED } else { ACCENT }).bg(bg), area) + 1;
                }
                // A canvas-wide agent shows its own state (off, paused, needs attention, …).
                match node.filter(|n| n.is_canvas_agent()).map(|n| app.canvas_agent_state(n)) {
                    Some(state) => end = put(buf, end, y, state.icon(app.tick), fg(state.color()).bg(bg), area) + 1,
                    None => end = put(buf, end, y, status.icon(app.tick), fg(status.color()).bg(bg), area) + 1,
                }
                if let Some(node) = app.doc.node(id)
                    && app.node_loop(node).is_some()
                {
                    end = put(buf, end, y, "↻", fg(crate::paint::LOOP).bg(bg), area) + 1;
                }
                if let Some(node) = app.doc.node(id)
                    && app.live_elsewhere(node)
                    && status != Status::External
                {
                    end = put(buf, end, y, "↗", fg(crate::paint::PURPLE).bg(bg), area) + 1;
                }
                // An unused session shows how long ago it was used.
                let age = node.and_then(|n| app.dust_of(n, status)).map(|(_, last)| format!(" {}", crate::paths::format_age(last, app.visual_now())));
                let style = match status.attention() {
                    _ if is_selected => bold(SELECT).bg(bg),
                    Some((color, _)) if !agent => bold(color).bg(bg),
                    _ if agent || tucked || age.is_some() => fg(GRAY).bg(bg),
                    _ => fg(TEXT).bg(bg),
                };
                let age_w = age.as_deref().map(|a| crate::paint::text_width(a) as i32).unwrap_or(0);
                put(buf, end, y, &truncate(&row.label, (width - end - 1 - age_w).max(1) as usize), style, area);
                if let Some(age) = &age {
                    put(buf, width - age_w - 1, y, age, fg(MUTED).bg(bg), area);
                }
            }
        }
    }
    if total > visible {
        let hint = format!(" {}–{} of {total} ", app.side_scroll + 1, (app.side_scroll + visible).min(total));
        put(buf, width - hint.len() as i32 - 1, area.y as i32, &hint, fg(MUTED).bg(PANEL), area);
    }
}

fn tile_rects(area: Rect, count: usize) -> Vec<Rect> {
    match count {
        0 => Vec::new(),
        1 => vec![area],
        2 => {
            let h = area.height / 2;
            vec![Rect::new(area.x, area.y, area.width, h), Rect::new(area.x, area.y + h, area.width, area.height - h)]
        }
        _ => {
            let h = area.height / 2;
            let w = area.width / 2;
            let mut rects = vec![
                Rect::new(area.x, area.y, w, h),
                Rect::new(area.x + w, area.y, area.width - w, h),
                Rect::new(area.x, area.y + h, w, area.height - h),
                Rect::new(area.x + w, area.y + h, area.width - w, area.height - h),
            ];
            if count == 3 {
                rects.truncate(2);
                rects.push(Rect::new(area.x, area.y + h, area.width, area.height - h));
            }
            rects
        }
    }
}

fn draw_terminals(app: &mut App, buf: &mut Buffer, area: Rect, view_model: &mut View) -> Option<(u16, u16)> {
    app.layout.tiles.clear();
    let gpu = app.render_mode == RenderMode::Gpu;
    if area.width < 10 {
        return None;
    }
    let ids = app.visible_terminals();
    // An open detail panel (automation, passive session, item) takes the right
    // side until it is closed or a terminal is opened.
    if let Some(detail) = app.right_detail.clone() {
        crate::ui_side::draw_right_detail(app, buf, area, &detail);
        return None;
    }
    if ids.is_empty() {
        draw_details(app, buf, area);
        return None;
    }
    let mut cursor = None;
    for (id, outer) in ids.iter().zip(tile_rects(area, ids.len())) {
        let inner = Rect::new(outer.x + 1, outer.y + 1, outer.width.saturating_sub(2), outer.height.saturating_sub(2));
        app.layout.tiles.push((id.clone(), outer, inner));
        let focused = app.focus == Focus::Terminal && app.focused_term.as_deref() == Some(id);
        let info = app.terms.get(id).cloned().unwrap_or_default();
        let node = app.doc.node(id).cloned();
        let status = node.as_ref().map(|n| app.node_status(n)).unwrap_or(if info.alive { Status::Running } else { Status::Stopped });
        let attention = if focused { None } else { status.attention() };
        let border = match attention {
            _ if focused => bold(ACCENT),
            Some((color, _)) => bold(color),
            None => fg(MUTED),
        };
        let set = match attention {
            _ if focused => &HEAVY,
            Some(_) if status == Status::Waiting => &HEAVY,
            Some(_) => &crate::paint::DOUBLE,
            None => &ROUND,
        };
        outline(buf, outer.x as i32, outer.y as i32, outer.width as i32, outer.height as i32, set, border, outer);
        let name = node.as_ref().map(|n| n.name.clone()).unwrap_or_else(|| id.clone());
        let view = app.views.get(id);
        let scroll = view.filter(|v| v.display_offset > 0).map(|v| format!(" · ↑{}", v.display_offset)).unwrap_or_default();
        let exit = if info.alive { String::new() } else { format!(" · exited {}", info.exit_code.map(|c| c.to_string()).unwrap_or_default()) };
        // The agent's terminal title is its current intent (its default title
        // and a bare program path add nothing).
        let agent = node.as_ref().filter(|n| n.command.is_none()).map(crate::agents::agent_of);
        let intent = title_intent(agent.unwrap_or_default(), &info.title, &name);
        let intent = if intent.is_empty() { String::new() } else { format!(" · {intent}") };
        let title = format!(" {} {name} · {}{intent}{scroll}{exit} ", status.icon(app.tick), status.label());
        let title_end = put(buf, outer.x as i32 + 1, outer.y as i32, &truncate(&title, outer.width.saturating_sub(6) as usize), border, outer);
        app.layout.hits.push((Rect::new(outer.x, outer.y, (title_end - outer.x as i32).max(1) as u16, 1), Hit::TileHeader(id.clone())));
        let close_x = outer.right() as i32 - 4;
        put(buf, close_x, outer.y as i32, "[×]", fg(GRAY), outer);
        app.layout.hits.push((Rect::new(close_x as u16, outer.y, 3, 1), Hit::TileClose(id.clone())));

        let tint = info.copilot && agent == Some(crate::agents::AgentKind::Copilot);
        match view {
            Some(view) => {
                let drawn = canvas_termview::tui::draw(
                    view,
                    buf,
                    inner,
                    &canvas_termview::tui::Options {
                        // Copilot's own user-message rows get the canvas tint.
                        tint: tint.then_some((crate::paint::USER_BG, crate::paint::ACCENT)),
                        selection: app.term_select.as_ref().filter(|s| s.id == *id),
                        cursor: focused && info.alive,
                    },
                );
                for (rect, target) in drawn.links {
                    app.layout.links.push(crate::links::Link { rects: vec![rect], target, terminal: true });
                }
                if drawn.cursor.is_some() {
                    cursor = drawn.cursor;
                }
                if gpu {
                    // The front end draws the grid itself; links are found here first.
                    let mut links = std::mem::take(&mut app.layout.links);
                    crate::links::scan(buf, inner, true, &[], &mut links);
                    app.layout.links = links;
                    fill(buf, inner.x as i32, inner.y as i32, inner.width as i32, inner.height as i32, Style::default(), inner);
                }
            }
            None => {
                let text = if info.alive || app.launching.contains_key(id) { "Starting…" } else { "No output yet." };
                put(buf, inner.x as i32 + 1, inner.y as i32, text, fg(MUTED), inner);
            }
        }
        if gpu {
            view_model.tiles.push(crate::view::TileModel {
                id: id.clone(),
                outer,
                inner,
                focused,
                alive: info.alive,
                cursor: focused && info.alive,
                tint,
                title: title.trim().to_owned(),
            });
        }
        if !info.alive && !app.launching.contains_key(id) {
            let hint = " Exited — press Enter or double-click the box to restart ";
            let x = inner.x as i32 + (inner.width as i32 - crate::paint::text_width(hint) as i32).max(0) / 2;
            put(buf, x, inner.bottom() as i32 - 1, hint, Style::default().fg(Color::Black).bg(ATTENTION), inner);
        }
    }
    cursor
}

/// Gpu mode: finds the links of tiles `ids` again after their terminals
/// changed without a full render (a front end that redraws only damaged
/// tiles); the rest of the layout stays. `scratch` is reused between calls.
pub fn rescan_tile_links(app: &mut App, ids: &[String], scratch: &mut Buffer) {
    let tiles: Vec<(String, Rect)> = app.layout.tiles.iter().filter(|(id, _, _)| ids.contains(id)).map(|(id, _, inner)| (id.clone(), *inner)).collect();
    if tiles.is_empty() {
        return;
    }
    if scratch.area != app.layout.screen {
        *scratch = Buffer::empty(app.layout.screen);
    }
    let mut links = std::mem::take(&mut app.layout.links);
    for (id, inner) in tiles {
        let inside = |r: &Rect| r.x >= inner.x && r.y >= inner.y && r.right() <= inner.right() && r.bottom() <= inner.bottom();
        links.retain(|l| !(l.terminal && l.rects.iter().all(inside)));
        let Some(view) = app.views.get(&id) else { continue };
        fill(scratch, inner.x as i32, inner.y as i32, inner.width as i32, inner.height as i32, Style::default(), inner);
        let drawn = canvas_termview::tui::draw(view, scratch, inner, &canvas_termview::tui::Options { tint: None, selection: None, cursor: false });
        for (rect, target) in drawn.links {
            links.push(crate::links::Link { rects: vec![rect], target, terminal: true });
        }
        crate::links::scan(scratch, inner, true, &[], &mut links);
    }
    app.layout.links = links;
}

/// The intent an agent shows in its terminal title, without the agent's own
/// decoration; empty when the title adds nothing to the box name.
pub fn title_intent(agent: crate::agents::AgentKind, title: &str, name: &str) -> String {
    use crate::agents::AgentKind;
    let mut text = title.trim();
    match agent {
        AgentKind::Copilot => text = text.strip_suffix(" - GitHub Copilot").unwrap_or(text).trim(),
        AgentKind::OpenCode => text = text.strip_prefix("OC | ").unwrap_or(text).trim(),
        AgentKind::Claude => {
            // A leading status glyph ("✳ ", spinner frames) changes as it works.
            let mut chars = text.chars();
            if let (Some(glyph), Some(' ')) = (chars.next(), chars.next())
                && !glyph.is_ascii()
                && !glyph.is_alphanumeric()
            {
                text = chars.as_str().trim();
            }
        }
    }
    let default = match agent {
        AgentKind::Copilot => "GitHub Copilot",
        AgentKind::OpenCode => "OpenCode",
        AgentKind::Claude => "Claude Code",
    };
    if text.is_empty() || text == default || text == name || text.contains(":\\") { String::new() } else { text.to_owned() }
}

fn draw_details(app: &mut App, buf: &mut Buffer, area: Rect) {
    outline(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, &LIGHT, fg(FAINT), area);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    let w = inner.width as usize;
    let mut lines: Vec<(String, Style)> = Vec::new();
    // Lines that act as buttons.
    let mut clicks: Vec<(usize, crate::dialogs::Action)> = Vec::new();
    if let Some(group) = app.selected_group.as_deref().and_then(|g| app.doc.group(g)).cloned().filter(|g| g.is_canvas_agents()) {
        use crate::canvas_wide::{AgentAction, MARK};
        let agents: Vec<Node> = app.canvas_agents().into_iter().cloned().collect();
        lines.push((format!("{MARK} {}", group.name), bold(TEXT)));
        lines.push((format!("{} canvas-wide agent(s): each sees the events of every group of this canvas", agents.len()), fg(MUTED)));
        clicks.push((lines.len(), crate::dialogs::Action::Agents(AgentAction::Panel)));
        lines.push((format!("{MARK} Manage canvas agents…"), fg(ACCENT)));
        if app.canvas_agents_unavailable().is_none() {
            clicks.push((lines.len(), crate::dialogs::Action::Agents(AgentAction::New)));
            lines.push(("+ New canvas-wide agent…".into(), fg(ACCENT)));
            clicks.push((lines.len(), crate::dialogs::Action::Agents(AgentAction::Library)));
            lines.push(("✦ Canvas skills…".into(), fg(ACCENT)));
        }
        lines.push((String::new(), Style::default()));
        for agent in agents.iter().take(inner.height as usize) {
            let state = app.canvas_agent_state(agent);
            clicks.push((lines.len(), crate::dialogs::Action::Agents(AgentAction::Show(agent.id.clone()))));
            lines.push((format!("{} {} — {}", state.icon(app.tick), agent.name, state.label()), fg(state.color())));
        }
        lines.push((String::new(), Style::default()));
        lines.push(("R: rename · Del: delete the frame (the agents stay)".into(), fg(MUTED)));
    } else if let Some(group) = app.selected_group.as_deref().and_then(|g| app.doc.group(g)).cloned() {
        use crate::dialogs::Action;
        let members = app.doc.members(&group.id);
        let shown = |n: &&Node| n.kind == NodeKind::Session || (group.is_automations() && n.kind == NodeKind::Automation);
        let count = members.iter().filter_map(|m| app.doc.node(m)).filter(shown).count();
        lines.push((format!("{} {}", if group.is_automations() { "◷" } else { "▣" }, group.name), bold(TEXT)));
        lines.push((format!("{count} {}", if group.is_automations() { "automations" } else { "sessions" }), fg(MUTED)));
        if !group.is_automations() {
            match &group.folder {
                Some(folder) => {
                    lines.push((format!("Folder   {}", truncate_left(folder, w.saturating_sub(9))), fg(TEXT)));
                    clicks.push((lines.len(), Action::OpenGroupFolder(group.id.clone(), false)));
                    lines.push((format!("◧ Open AI docs ({})", crate::doc::AI_DOCS), fg(ACCENT)));
                    clicks.push((lines.len(), Action::OpenGroupFolder(group.id.clone(), true)));
                    lines.push((format!("◨ Open human outputs ({})", crate::doc::HUMAN_OUTPUTS), fg(ACCENT)));
                    clicks.push((lines.len(), Action::SetGroupFolder(group.id.clone())));
                    lines.push(("  Change folder…".into(), fg(MUTED)));
                }
                None => {
                    clicks.push((lines.len(), Action::SetGroupFolder(group.id.clone())));
                    lines.push(("◧ Choose a folder for AI docs + human outputs…".into(), fg(ACCENT)));
                }
            }
            let overrides = group.defaults.as_ref().map(|d| d.summary()).unwrap_or_default();
            if !overrides.is_empty() {
                lines.push((format!("Launch   {overrides}"), fg(TEXT)));
            }
            clicks.push((lines.len(), Action::GroupDefaults(group.id.clone())));
            lines.push(("⚙ Launch defaults for this group…".into(), fg(ACCENT)));
            let passives = app.group_passives(&group.id).len();
            if passives > 0 {
                clicks.push((lines.len(), Action::Visual(crate::visual::VisualAction::GroupPassives(group.id.clone()))));
                lines.push((format!("⟲ {passives} passive session(s)…"), fg(ACCENT)));
            } else if !app.demo {
                clicks.push((lines.len(), Action::NewPassive(group.id.clone())));
                lines.push(("⟲ Add a passive session…".into(), fg(ACCENT)));
            }
        }
        lines.push((String::new(), Style::default()));
        for member in members.iter().filter_map(|m| app.doc.node(m)).filter(shown).take(inner.height as usize) {
            let status = app.node_status(member);
            lines.push((format!("{} {} — {}", status.icon(app.tick), member.name, status.label()), fg(status.color())));
        }
        lines.push((String::new(), Style::default()));
        lines.push(("Enter: show running terminals · R: rename · Del: delete group".into(), fg(MUTED)));
    } else if let Some(node) = app.primary().cloned() {
        // The same panel a click on the box opens (automations keep their own).
        let detail = match (&node.automation, node.kind) {
            (Some(key), NodeKind::Automation) => crate::side::RightDetail::Automation(key.clone()),
            _ => crate::side::RightDetail::Session(node.id.clone()),
        };
        return crate::ui_side::draw_right_detail(app, buf, area, &detail);
    } else {
        lines.push(("Select a session to see it here.".into(), fg(MUTED)));
    }
    for (i, (text, style)) in lines.into_iter().enumerate().take(inner.height as usize) {
        let end = put(buf, inner.x as i32, inner.y as i32 + i as i32, &truncate(&text, w), style, inner);
        if let Some((_, action)) = clicks.iter().find(|(line, _)| *line == i) {
            let rect = Rect::new(inner.x, inner.y + i as u16, (end - inner.x as i32).max(1) as u16, 1);
            app.layout.hits.push((rect, crate::dialogs::Hit::Toolbar(action.clone())));
        }
    }
}

fn draw_status(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, 0, area.y as i32, area.width as i32, 1, Style::default().bg(PANEL), area);
    let (text, style) = if let Some(error) = &app.save_error {
        (format!("Canvas not saved: {error}"), fg(NEGATIVE).bg(PANEL))
    } else if let Some(notice) = &app.notice {
        (notice.text.clone(), if notice.error { fg(NEGATIVE).bg(PANEL) } else { fg(TEXT).bg(PANEL) })
    } else {
        let hints = match app.focus {
            Focus::Terminal => "Typing goes to the terminal · Ctrl+] or F6 back to canvas · Ctrl+F6 / Ctrl+Shift+F6 recent sessions · drag to copy · Ctrl+V paste · Alt+V image · Ctrl+click links · Shift+PgUp history · click canvas to leave",
            Focus::Sidebar if app.side_view == crate::side::SideView::Reports => "↑↓ move · Enter read · Space read/unread · Del delete · Esc back · Alt+1…9 views · ? help",
            Focus::Sidebar if app.side_view != crate::side::SideView::Sessions => "↑↓ move · Enter open/edit · Space enable/disable · Del remove · Esc back · Alt+1…9 views · ? help",
            Focus::Sidebar => "click view · double-click/Enter start · ] [ or Ctrl+F6 recent · ←→ collapse · / filter · Tab canvas · Alt+1…9 views · ? help",
            Focus::Canvas => "click view · double-click/Enter start · ] [ or Ctrl+F6 recent · drag move · empty-drag pan · Shift/right-drag select · wheel pan · Alt+wheel zoom · right-click menu · ? help",
        };
        (hints.to_owned(), fg(MUTED).bg(PANEL))
    };
    let right = match (&app.banner, &app.perf, &app.stats) {
        // The connection banner replaces the (stale) service figures.
        (Some(banner), _, _) => format!(" ● {banner} "),
        (None, Some(perf), _) => perf.summary(),
        (None, None, stats) => match stats {
        Some(stats) => format!(
            " svc {} MB · {} live · zoom {} ",
            stats.private_bytes / (1024 * 1024),
            stats.alive,
            match app.render_mode {
                RenderMode::Cells => ["100%", "50%", "25%"][app.doc.view.zoom.min(2) as usize].to_owned(),
                RenderMode::Gpu => format!("{:.0}%", app.view_scale * 100.0),
            }
        ),
        None => String::new(),
        },
    };
    let right_w = crate::paint::text_width(&right) as i32;
    put(buf, 1, area.y as i32, &truncate(&text, (area.width as i32 - right_w - 2).max(1) as usize), style, area);
    put(buf, area.width as i32 - right_w, area.y as i32, &right, fg(if app.connected { DARK } else { NEGATIVE }).bg(PANEL), area);
    let _ = GRAY;
    let _ = SELECT;
}

#[cfg(test)]
mod tests {
    use super::{HashMap, Status, title_intent, toolbar_counts};
    use crate::agents::AgentKind;

    #[test]
    fn pane_titles_lose_each_agents_decoration() {
        assert_eq!(title_intent(AgentKind::Copilot, "Fixing the build - GitHub Copilot", "Box"), "Fixing the build");
        assert_eq!(title_intent(AgentKind::Copilot, "GitHub Copilot", "Box"), "");
        assert_eq!(title_intent(AgentKind::Claude, "✳ Refactor the parser", "Box"), "Refactor the parser");
        assert_eq!(title_intent(AgentKind::Claude, "◐ Refactor the parser", "Box"), "Refactor the parser");
        assert_eq!(title_intent(AgentKind::Claude, "◑ Claude Code", "Box"), "");
        assert_eq!(title_intent(AgentKind::Claude, "A plain title", "Box"), "A plain title");
        assert_eq!(title_intent(AgentKind::OpenCode, "OC | Review the diff", "Box"), "Review the diff");
        assert_eq!(title_intent(AgentKind::OpenCode, "OpenCode", "Box"), "");
        assert_eq!(title_intent(AgentKind::OpenCode, "OC | Box", "Box"), "", "the box name adds nothing");
        assert_eq!(title_intent(AgentKind::Claude, r"C:\Users\me\.local\bin\claude.exe", "Box"), "", "a bare program path");
    }

    #[test]
    fn toolbar_counts_drop_the_least_important_parts_first() {
        let counts: HashMap<Status, usize> = [(Status::Working, 2), (Status::Waiting, 1), (Status::JustFinished, 3), (Status::External, 1)].into_iter().collect();
        let texts = |room: usize| -> Vec<String> { toolbar_counts(1, &counts, 7, room).into_iter().map(|p| p.text).collect() };
        let all = texts(500);
        assert_eq!(all.len(), 6);
        assert!(all[0].contains("1 automation failing") && all.last().is_some_and(|s| s == "7 sessions"));
        let width = |parts: &[String]| parts.iter().map(|t| crate::paint::text_width(t) + 3).sum::<usize>();
        for room in [80, 60, 50, 40, 30, 20, 12, 6, 0] {
            let parts = texts(room);
            assert!(width(&parts) <= room, "room {room}: {parts:?}");
        }
        // Sessions total and open elsewhere go first; needs you and the failing badge stay.
        let narrow = texts(width(&all) - 4);
        assert!(!narrow.iter().any(|t| t.contains("sessions")) && narrow.iter().any(|t| t.contains("elsewhere")));
        let tight = texts(45);
        assert!(tight.iter().any(|t| t.contains("needs you")) && tight.iter().any(|t| t.contains("failing")), "{tight:?}");
        // Abbreviated before dropping them.
        let short = texts(18);
        assert!(short.len() == 2 && short.iter().all(|t| !t.contains("needs you")), "{short:?}");
    }
}
