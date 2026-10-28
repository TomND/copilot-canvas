//! Canvas rendering: groups, connectors, session and subagent boxes. Cells
//! mode draws characters ([`draw_canvas`]); Gpu mode describes the same canvas
//! in world units ([`canvas_model`]) for the front end to draw at any scale.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, Drag};
use crate::doc::{Node, NodeKind};
use crate::paint::{
    self, ACCENT, BoxSet, DASHED, FAINT, GROUP_COLORS, HEAVY, LIGHT, MUTED, PANEL, ROUND, SELECT, TEXT, fill, fg, outline, put, put_sym,
    truncate,
};
use crate::status::Status;
use crate::view::{Border, BoxKind, BoxModel, CanvasModel, ChipModel, EdgeKind, EdgeModel, GroupModel, LoopModel, PulseModel, WRect};

/// Gpu mode: folder chips show (and click) from this scale up.
pub const CHIP_SCALE: f32 = 0.7;

/// Folder chips on a group's title row: label, opens human-outputs.
const CHIPS: [(&str, bool); 2] = [(" ◧ AI docs ", false), (" ◨ Outputs ", true)];

/// Coloured border and tint of a box that wants your eyes (needs you, done ·
/// review); a failed automation pulses until its next good run.
pub fn box_attention(node: &Node, status: Status, tick: usize) -> Option<(Color, Color)> {
    if node.kind == NodeKind::Agent {
        None
    } else if node.kind == NodeKind::Automation && status == Status::Failed {
        Some((if (tick / 8).is_multiple_of(2) { crate::paint::NEGATIVE } else { crate::paint::NEGATIVE_DIM }, crate::paint::NEGATIVE_BG))
    } else {
        status.attention()
    }
}

/// Colour and weight of a box's outline.
pub fn box_border_style(node: &Node, attention: Option<(Color, Color)>, selected: bool, focused_term: bool) -> Style {
    if let Some((color, _)) = attention {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(SELECT).add_modifier(Modifier::BOLD)
    } else if focused_term {
        fg(ACCENT)
    } else if node.kind == NodeKind::Agent {
        fg(MUTED)
    } else {
        fg(TEXT).add_modifier(Modifier::DIM)
    }
}

/// The outline at full zoom: heavy when selected or needing you, double for
/// done · review and automations, dashed for passive sessions.
pub fn box_border(node: &Node, status: Status, selected: bool, attention: bool) -> Border {
    if selected || status == Status::Waiting && attention {
        Border::Heavy
    } else if attention {
        Border::Double
    } else if node.passive.is_some() {
        Border::Dashed
    } else {
        match node.kind {
            NodeKind::Agent => Border::Light,
            NodeKind::Automation => Border::Double,
            NodeKind::Session => Border::Round,
        }
    }
}

fn border_set(border: Border) -> &'static BoxSet {
    match border {
        Border::Round => &ROUND,
        Border::Light => &LIGHT,
        Border::Heavy => &HEAVY,
        Border::Double => &crate::paint::DOUBLE,
        Border::Dashed => &DASHED,
    }
}

/// Marker before a box's name: passive, subagent, automation, subsession.
pub fn box_prefix(node: &Node) -> &'static str {
    match (node.kind, node.parent.is_some()) {
        _ if node.passive.is_some() => "⟲ ",
        (NodeKind::Agent, _) => "◇ ",
        (NodeKind::Automation, _) => "◷ ",
        (NodeKind::Session, true) => "↳ ",
        _ => "",
    }
}

/// The status word of a box.
pub fn box_label(node: &Node, status: Status) -> &'static str {
    match node.kind {
        NodeKind::Automation => automation_label(status),
        NodeKind::Agent if status == Status::JustFinished => "just finished",
        _ => status.label(),
    }
}

/// The status line of a box: icon and label, the loop, or a passive session's state.
fn box_line(app: &App, node: &Node, icon: &str, label: &str, every: Option<&str>) -> String {
    match every {
        _ if node.passive.is_some() => format!("{icon} {}", app.passive_line(node)),
        Some(every) => format!("{icon} {label} · ↻ {every}"),
        None => format!("{icon} {label}"),
    }
}

/// The third line of a box: a passive session's trigger, else folder and branch.
fn box_subtitle(app: &App, node: &Node) -> String {
    match &node.passive {
        Some(passive) => crate::app::App::passive_trigger(passive),
        None => app.subtitle(node),
    }
}

pub fn draw_canvas(app: &mut App, buf: &mut Buffer, area: Rect) {
    let clip = area;
    // Dot grid anchored to world coordinates so it moves with panning. It is
    // hidden while panning: shifting it rewrites nearly every cell each frame.
    let (n, d) = app.scale();
    let step_x = (6 * n / d).max(3);
    let step_y = (3 * n / d).max(2);
    let (ox, oy) = app.to_screen(0, 0);
    let panning = matches!(app.drag, Some(Drag::Pan { moved: true, .. })) || app.pan_quiet_until.is_some();
    if !panning {
        draw_grid(buf, area, (ox, oy), (step_x, step_y));
    }

    let flash = app.flash.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_millis(1500)).map(|(id, t)| (id.clone(), t.elapsed()));
    let flashing = |id: &str| flash.as_ref().is_some_and(|(f, t)| f == id && (t.as_millis() / 250) % 2 == 0);

    // Group under a dragged node (drop target).
    let drop_target = match &app.drag {
        Some(Drag::Nodes { ids, moved: true, .. }) => ids.first().and_then(|id| app.doc.node(id)).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()),
        _ => None,
    };

    let mut member_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for node in &app.doc.nodes {
        if let Some(group) = app.doc.group_of(node) {
            *member_counts.entry(group.id.as_str()).or_default() += 1;
        }
    }
    let mut chip_hits: Vec<(ratatui::layout::Rect, crate::dialogs::Hit)> = Vec::new();
    let mut groups: Vec<&crate::doc::Group> = app.doc.groups.iter().collect();
    groups.sort_by_key(|g| std::cmp::Reverse((g.w as i64) * (g.h as i64)));
    for group in groups {
        let (x, y, w, h) = app.group_screen(group);
        if x + w < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 {
            continue;
        }
        let color = GROUP_COLORS[group.color as usize % GROUP_COLORS.len()];
        let selected = app.selected_group.as_deref() == Some(&group.id) || flashing(&group.id);
        let target = drop_target.as_deref() == Some(&group.id);
        let style = if selected || target { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { fg(color).add_modifier(Modifier::DIM) };
        outline(buf, x, y, w, h, if selected { &HEAVY } else { &DASHED }, style, clip);
        let members = member_counts.get(group.id.as_str()).copied().unwrap_or(0);
        let icon = if group.is_automations() { "◷" } else { "▣" };
        let title = format!(" {icon} {} · {members} ", group.name);
        let title_end = put(buf, x + 2, y, &truncate(&title, (w - 4).max(0) as usize), Style::default().fg(color).add_modifier(Modifier::BOLD), clip);
        // Family names above each row of automation boxes.
        if group.is_automations() && app.doc.view.zoom == 0 {
            for (fx, fy, family) in app.family_labels(group) {
                let (sx, sy) = app.to_screen(fx, fy);
                if sy > y && sy < y + h - 1 {
                    put(buf, sx, sy, &truncate(&family, (x + w - 2 - sx).max(0) as usize), fg(MUTED).add_modifier(Modifier::ITALIC), clip);
                }
            }
        }
        // Folder chips: open the group's ai-docs / human-outputs in File Explorer.
        if !group.is_automations() && w >= 44 {
            let total: i32 = CHIPS.iter().map(|(t, _)| crate::paint::text_width(t) as i32 + 1).sum();
            let mut cx = x + w - 2 - total;
            if cx > title_end {
                let style = if group.folder.is_some() { Style::default().fg(color).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
                for (text, outputs) in CHIPS {
                    let end = put(buf, cx, y, text, style, clip);
                    if cx >= clip.x as i32 && end <= clip.right() as i32 && y >= clip.y as i32 && y < clip.bottom() as i32 {
                        chip_hits.push((
                            ratatui::layout::Rect::new(cx as u16, y as u16, (end - cx) as u16, 1),
                            crate::dialogs::Hit::Toolbar(crate::dialogs::Action::OpenGroupFolder(group.id.clone(), outputs)),
                        ));
                    }
                    cx = end + 1;
                }
            }
        }
        put_sym(buf, x + w - 1, y + h - 1, "◢", fg(color), clip);
    }

    app.layout.hits.extend(chip_hits);

    // Connectors: parent → subagent (thin) and parent → subsession (accent),
    // routed between facing edges in whichever direction the child lies.
    let by_id: std::collections::HashMap<&str, &Node> = app.doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for node in &app.doc.nodes {
        let Some(parent) = node.parent.as_deref().and_then(|p| by_id.get(p).copied()) else { continue };
        let (px, py, pw, ph) = app.node_screen(parent);
        let (cx, cy, cw, ch) = app.node_screen(node);
        // Skip connectors entirely off screen (their bounding box misses the canvas).
        let (lx, ly) = (px.min(cx) - 1, py.min(cy) - 1);
        let (hx, hy) = ((px + pw).max(cx + cw) + 1, (py + ph).max(cy + ch) + 1);
        if hx < clip.x as i32 || lx > clip.right() as i32 || hy < clip.y as i32 || ly > clip.bottom() as i32 {
            continue;
        }
        let subsession = node.kind == NodeKind::Session;
        let style = if subsession { fg(ACCENT) } else { fg(MUTED) };
        let (vertical, horizontal) = if subsession { ("┊", "┄") } else { ("│", "─") };
        let path = connector_path((px, py, pw, ph), (cx, cy, cw, ch));
        draw_path(buf, &path, vertical, horizontal, style, clip);
    }

    // Nodes; selected ones on top.
    let mut order: Vec<&Node> = app.doc.nodes.iter().collect();
    order.sort_by_key(|n| app.selection.contains(&n.id));
    let zoom = app.doc.view.zoom;
    let tick = app.tick;
    // The session whose terminal is the active one on the right.
    let active_term = app.focused_term.clone().filter(|id| app.visible_terminals().contains(id));
    let typing = app.focus == crate::app::Focus::Terminal;
    for node in order {
        let (x, y, w, h) = app.node_screen(node);
        if x + w < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 {
            continue;
        }
        let status = app.node_status(node);
        let selected = app.selection.contains(&node.id) || flashing(&node.id);
        let focused_term = app.focused_term.as_deref() == Some(&node.id) && app.owned_alive(&node.id);
        // Sessions that need you or finished a turn you have not looked at yet get a
        // coloured border (gold heavy / blue double) and tinted body, which stay
        // even when selected.
        let attention = box_attention(node, status, tick);
        let border = box_border_style(node, attention, selected, focused_term);
        let icon = status.icon(tick);
        let status_style = fg(status.color());
        let name_style = if selected { Style::default().fg(SELECT).add_modifier(Modifier::BOLD) } else { fg(TEXT) };
        match zoom {
            0 => {
                let body = attention.map(|(_, bg)| bg).unwrap_or(PANEL);
                fill(buf, x + 1, y + 1, w - 2, h - 2, Style::default().bg(body), clip);
                let set = border_set(box_border(node, status, selected, attention.is_some()));
                outline(buf, x, y, w, h, set, border, clip);
                let title = format!(" {}{} ", box_prefix(node), node.name);
                put(buf, x + 1, y, &truncate(&title, (w - 2) as usize), name_style, clip);
                let label = box_label(node, status);
                let line = box_line(app, node, icon, label, app.node_loop(node).as_deref());
                let label_style = if attention.is_some() { status_style.add_modifier(Modifier::BOLD) } else { status_style };
                let end = put(buf, x + 2, y + 1, &truncate(&line, (w - 4) as usize), label_style.bg(body), clip);
                if app.live_elsewhere(node) && status != crate::status::Status::External {
                    put(buf, end, y + 1, &truncate(" · ↗ elsewhere", (x + w - 2 - end).max(0) as usize), fg(crate::paint::PURPLE).bg(body), clip);
                }
                if h >= 4 {
                    put(buf, x + 2, y + 2, &truncate(&box_subtitle(app, node), (w - 4) as usize), fg(MUTED).bg(body), clip);
                }
            }
            1 => {
                let bg = attention.map(|(_, bg)| bg).unwrap_or(PANEL);
                fill(buf, x, y, w, h, Style::default().bg(bg), clip);
                let mut end = put(buf, x, y, icon, status_style.bg(bg), clip);
                end = put(buf, end + 1, y, &truncate(&node.name, (w - 2).max(0) as usize), name_style.bg(bg), clip);
                let _ = end;
                if h >= 2 {
                    let label_style = attention.map(|(c, _)| crate::paint::bold(c)).unwrap_or(fg(MUTED));
                    put(buf, x, y + 1, &truncate(status.label(), w as usize), label_style.bg(bg), clip);
                }
                if selected {
                    for xx in x..x + w {
                        paint::style_at(buf, xx, y + h - 1, Style::default().add_modifier(Modifier::UNDERLINED), clip);
                    }
                }
            }
            _ => {
                let active = active_term.as_deref() == Some(node.id.as_str());
                let style = if active {
                    crate::paint::bold(ACCENT).add_modifier(Modifier::UNDERLINED)
                } else {
                    attention.map(|(c, bg)| crate::paint::bold(c).bg(bg)).unwrap_or(name_style)
                };
                let end = put(buf, x, y, icon, attention.map(|(_, bg)| status_style.bg(bg)).unwrap_or(status_style), clip);
                put(buf, end, y, &truncate(&node.name, (w - 1).max(0) as usize), style, clip);
            }
        }
    }

    // Queued messages for a session: a badge on its top edge.
    if zoom < 2 {
        for (id, count) in app.bus.mailbox.iter().filter(|(_, c)| **c > 0) {
            if let Some(node) = app.doc.node(id) {
                let (x, y, w, _) = app.node_screen(node);
                draw_mailbox(buf, (x, y, w), *count, clip);
            }
        }
    }

    // Events in flight: a bright dot runs from the sender to each receiver.
    for (from, to, start) in &app.pulses {
        let (Some(a), Some(b)) = (app.doc.node(from), app.doc.node(to)) else { continue };
        if from == to {
            continue;
        }
        let path = connector_path(app.node_screen(a), app.node_screen(b));
        draw_pulse(buf, &path, start.elapsed().as_millis() as f32 / 1_200.0, clip);
    }

    // Looping boxes: a dotted loopback on the right side with one lit dot
    // running around it (with a short trail), like a loading bar.
    for node in app.doc.nodes.iter() {
        let (x, y, w, h) = app.node_screen(node);
        if x + w + 3 < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 {
            continue;
        }
        if app.node_loop(node).is_none() {
            continue;
        }
        // Outside the active terminal's halo, which runs one cell around the box.
        let halo = zoom < 2 && active_term.as_deref() == Some(node.id.as_str());
        draw_loop(buf, (x, y, w + halo as i32, h), tick, clip);
    }

    // Halo one cell outside the active terminal's box (drawn last so neighbours
    // cannot cover it): thin while its terminal is showing, heavy while typing.
    if zoom < 2
        && let Some(node) = active_term.as_deref().and_then(|id| app.doc.node(id))
    {
        let (x, y, w, h) = app.node_screen(node);
        let set = if typing { &HEAVY } else { &LIGHT };
        outline(buf, x - 1, y - 1, w + 2, h + 2, set, crate::paint::bold(ACCENT), clip);
    }

    if let Some(Drag::Rubber { start, now, .. }) = &app.drag {
        let (x0, y0) = (start.0.min(now.0) as i32, start.1.min(now.1) as i32);
        let (x1, y1) = (start.0.max(now.0) as i32, start.1.max(now.1) as i32);
        outline(buf, x0, y0, (x1 - x0 + 1).max(2), (y1 - y0 + 1).max(2), &DASHED, fg(SELECT), clip);
    }

    if app.doc.nodes.is_empty() && app.doc.groups.is_empty() {
        draw_empty_hint(buf, area);
    }
}

fn draw_grid(buf: &mut Buffer, area: Rect, (ox, oy): (i32, i32), (step_x, step_y): (i32, i32)) {
    for y in area.y..area.bottom() {
        if (y as i32 - oy).rem_euclid(step_y) != 0 {
            continue;
        }
        for x in area.x..area.right() {
            if (x as i32 - ox).rem_euclid(step_x) == 0 {
                put_sym(buf, x as i32, y as i32, "·", fg(FAINT), area);
            }
        }
    }
}

fn draw_mailbox(buf: &mut Buffer, (x, y, w): (i32, i32, i32), count: u32, clip: Rect) {
    let badge = format!(" ✉ {count} ");
    put(buf, x + w - 2 - crate::paint::text_width(&badge) as i32, y, &badge, crate::paint::bold(crate::paint::PANEL).bg(crate::paint::ATTENTION), clip);
}

/// A bright dot with a short trail `t` (0..1) of the way along `path`.
fn draw_pulse(buf: &mut Buffer, path: &[(i32, i32)], t: f32, clip: Rect) {
    if path.len() < 2 {
        return;
    }
    let at = ((path.len() - 1) as f32 * t.min(1.0)) as usize;
    for (offset, glyph, style) in [(2usize, "·", fg(crate::paint::LOOP)), (1, "•", fg(crate::paint::LOOP)), (0, "●", crate::paint::bold(crate::paint::LOOP_LIT))] {
        if let Some(&(px, py)) = at.checked_sub(offset).and_then(|i| path.get(i)) {
            put(buf, px, py, glyph, style, clip);
        }
    }
}

/// The loopback arc beside a box with its lit dot at animation frame `tick`.
fn draw_loop(buf: &mut Buffer, (x, y, w, h): (i32, i32, i32, i32), tick: usize, clip: Rect) {
    let phase = tick / 2;
    let path = loop_path(x, y, w, h);
    if path.len() == 1 {
        // One-line boxes (zoomed out): the ↻ icon pulses.
        let (px, py, glyph) = path[0];
        let color = if phase.is_multiple_of(2) { crate::paint::LOOP_LIT } else { crate::paint::LOOP };
        put(buf, px, py, glyph, crate::paint::bold(color), clip);
        return;
    }
    let lit = phase % path.len();
    let trail = (lit + path.len() - 1) % path.len();
    for (i, &(px, py, glyph)) in path.iter().enumerate() {
        let (glyph, style) = if i == lit {
            ("●", crate::paint::bold(crate::paint::LOOP_LIT))
        } else if i == trail {
            ("•", fg(crate::paint::LOOP))
        } else {
            (glyph, fg(crate::paint::LOOP))
        };
        put(buf, px, py, glyph, style, clip);
    }
}

/// Lines of the getting-started hint on an empty canvas.
pub const EMPTY_HINT: [&str; 5] = [
    "Your canvas is empty.",
    "",
    "N  new session          I  import agent sessions",
    "V  import VS Code agents   ?  help",
    "Double-click anywhere to create a session there.",
];

fn draw_empty_hint(buf: &mut Buffer, area: Rect) {
    let top = area.y as i32 + area.height as i32 / 2 - 3;
    for (i, line) in EMPTY_HINT.iter().enumerate() {
        let x = area.x as i32 + (area.width as i32 - paint::text_width(line) as i32) / 2;
        put(buf, x, top + i as i32, line, fg(if i == 0 { TEXT } else { MUTED }), area);
    }
}

/// Everything that moves with the current drag (for the Gpu visual offset):
/// dragged boxes with their subagents, or a group with its contents.
fn dragged(app: &App) -> (HashSet<&str>, HashSet<&str>) {
    let mut nodes = HashSet::new();
    let mut groups = HashSet::new();
    match &app.drag {
        Some(Drag::Nodes { ids, .. }) => {
            nodes.extend(ids.iter().map(String::as_str));
            for node in &app.doc.nodes {
                if node.kind == NodeKind::Agent && node.parent.as_ref().is_some_and(|p| ids.contains(p)) {
                    nodes.insert(node.id.as_str());
                }
            }
        }
        Some(Drag::Group { id, .. }) => {
            if let Some(group) = app.doc.group(id) {
                let rect = group.rect();
                groups.insert(group.id.as_str());
                for node in &app.doc.nodes {
                    let (cx, cy) = node.rect().center();
                    if rect.contains(cx, cy) {
                        nodes.insert(node.id.as_str());
                    }
                }
                for other in &app.doc.groups {
                    if other.id != group.id && rect.contains_rect(&other.rect()) {
                        groups.insert(other.id.as_str());
                    }
                }
            }
        }
        _ => {}
    }
    (nodes, groups)
}

/// The canvas as a model in world units (Gpu mode): the same groups,
/// connectors, boxes, styles and animations `draw_canvas` draws, culled to
/// `area`. Registers the folder chips' click targets.
pub fn canvas_model(app: &mut App, area: Rect) -> CanvasModel {
    let scale = app.scale_f();
    let origin = app.origin_f();
    let tick = app.tick;
    let mut model = CanvasModel {
        area,
        origin,
        scale,
        grid: !(matches!(app.drag, Some(Drag::Pan { moved: true, .. })) || app.pan_quiet_until.is_some()),
        groups: Vec::new(),
        edges: Vec::new(),
        boxes: Vec::new(),
        loops: Vec::new(),
        pulses: Vec::new(),
        halo: None,
        rubber: None,
        empty: app.doc.nodes.is_empty() && app.doc.groups.is_empty(),
        tick,
    };
    let visible = model.visible();
    // Loop arcs, halos and labels reach a few units past a box.
    let shows = |r: WRect| r.right() + 4.0 >= visible.x && r.x - 1.0 <= visible.right() && r.bottom() + 2.0 >= visible.y && r.y - 2.0 <= visible.bottom();
    let flash = app.flash.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_millis(1500)).map(|(id, t)| (id.clone(), t.elapsed()));
    let flashing = |id: &str| flash.as_ref().is_some_and(|(f, t)| f == id && (t.as_millis() / 250) % 2 == 0);
    let drop_target = match &app.drag {
        Some(Drag::Nodes { ids, moved: true, .. }) => ids.first().and_then(|id| app.doc.node(id)).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()),
        _ => None,
    };
    let offset = app.drag_offset();
    let (moving_nodes, moving_groups) = dragged(app);
    let node_rect = |node: &Node| {
        let rect = WRect::from(node.rect());
        if moving_nodes.contains(node.id.as_str()) { rect.offset(offset) } else { rect }
    };

    let mut member_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for node in &app.doc.nodes {
        if let Some(group) = app.doc.group_of(node) {
            *member_counts.entry(group.id.as_str()).or_default() += 1;
        }
    }
    let mut chip_hits: Vec<(Rect, crate::dialogs::Hit)> = Vec::new();
    let mut groups: Vec<&crate::doc::Group> = app.doc.groups.iter().collect();
    groups.sort_by_key(|g| std::cmp::Reverse((g.w as i64) * (g.h as i64)));
    for group in groups {
        let mut rect = WRect::from(group.rect());
        if moving_groups.contains(group.id.as_str()) {
            rect = rect.offset(offset);
        }
        if !shows(rect) {
            continue;
        }
        let color = GROUP_COLORS[group.color as usize % GROUP_COLORS.len()];
        let selected = app.selected_group.as_deref() == Some(&group.id) || flashing(&group.id);
        let target = drop_target.as_deref() == Some(&group.id);
        let style = if selected || target { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { fg(color).add_modifier(Modifier::DIM) };
        let members = member_counts.get(group.id.as_str()).copied().unwrap_or(0);
        let icon = if group.is_automations() { "◷" } else { "▣" };
        let title_w = crate::paint::text_width(&format!(" {icon} {} · {members} ", group.name)) as f32;
        let mut chips = Vec::new();
        if !group.is_automations() && group.w >= 44 {
            let total: f32 = CHIPS.iter().map(|(t, _)| crate::paint::text_width(t) as f32 + 1.0).sum();
            let mut cx = rect.right() - 2.0 - total;
            if cx > rect.x + 2.0 + title_w {
                for (label, outputs) in CHIPS {
                    let w = crate::paint::text_width(label) as f32;
                    let chip = WRect::new(cx, rect.y, w, 1.0);
                    chips.push(ChipModel { label, outputs, rect: chip });
                    // Clickable where a readable chip covers whole cells.
                    let (sx, sy) = model.to_screen(chip.x, chip.y);
                    let (ex, ey) = model.to_screen(chip.right(), chip.bottom());
                    let cell = Rect::new(sx.round().max(0.0) as u16, sy.round().max(0.0) as u16, (ex.round() - sx.round()).max(0.0) as u16, (ey - sy).round().max(1.0) as u16);
                    let inside = cell.x >= area.x && cell.right() <= area.right() && cell.y >= area.y && cell.y < area.bottom();
                    if scale >= CHIP_SCALE && cell.width > 0 && inside {
                        chip_hits.push((cell, crate::dialogs::Hit::Toolbar(crate::dialogs::Action::OpenGroupFolder(group.id.clone(), outputs))));
                    }
                    cx += w + 1.0;
                }
            }
        }
        let families = if group.is_automations() {
            let (dx, dy) = (rect.x - group.x as f32, rect.y - group.y as f32);
            app.family_labels(group).into_iter().map(|(x, y, family)| (x as f32 + dx, y as f32 + dy, family)).collect()
        } else {
            Vec::new()
        };
        model.groups.push(GroupModel {
            id: group.id.clone(),
            rect,
            name: group.name.clone(),
            icon,
            members,
            color,
            style,
            selected,
            drop_target: target,
            automations: group.is_automations(),
            chips,
            has_folder: group.folder.is_some(),
            families,
        });
    }

    let by_id: std::collections::HashMap<&str, &Node> = app.doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for node in &app.doc.nodes {
        let Some(parent) = node.parent.as_deref().and_then(|p| by_id.get(p).copied()) else { continue };
        let (a, b) = (node_rect(parent), node_rect(node));
        let bounds = WRect::new(a.x.min(b.x), a.y.min(b.y), a.right().max(b.right()) - a.x.min(b.x), a.bottom().max(b.bottom()) - a.y.min(b.y));
        if !shows(bounds) {
            continue;
        }
        let subsession = node.kind == NodeKind::Session;
        model.edges.push(EdgeModel {
            kind: if subsession { EdgeKind::Subsession } else { EdgeKind::Subagent },
            color: if subsession { ACCENT } else { MUTED },
            from: a,
            to: b,
            path: connector_path_f(a, b),
        });
    }

    let active_term = app.focused_term.clone().filter(|id| app.visible_terminals().contains(id));
    let typing = app.focus == crate::app::Focus::Terminal;
    let mut order: Vec<&Node> = app.doc.nodes.iter().collect();
    order.sort_by_key(|n| app.selection.contains(&n.id));
    for node in order {
        let rect = node_rect(node);
        if !shows(rect) {
            continue;
        }
        let status = app.node_status(node);
        let selected = app.selection.contains(&node.id) || flashing(&node.id);
        let focused_term = app.focused_term.as_deref() == Some(&node.id) && app.owned_alive(&node.id);
        let attention = box_attention(node, status, tick);
        let icon = status.icon(tick);
        let label = box_label(node, status);
        let every = app.node_loop(node);
        let status_style = fg(status.color());
        let kind = match node.kind {
            _ if node.passive.is_some() => BoxKind::Passive,
            NodeKind::Session => BoxKind::Session,
            NodeKind::Agent => BoxKind::Agent,
            NodeKind::Automation => BoxKind::Automation,
        };
        if every.is_some() {
            model.loops.push(LoopModel { rect, phase: (tick % 20) as f32 / 20.0, halo: active_term.as_deref() == Some(node.id.as_str()) });
        }
        model.boxes.push(BoxModel {
            id: node.id.clone(),
            kind,
            rect,
            prefix: box_prefix(node),
            name: node.name.clone(),
            status,
            icon,
            label,
            line: box_line(app, node, icon, label, every.as_deref()),
            line_style: if attention.is_some() { status_style.add_modifier(Modifier::BOLD) } else { status_style },
            name_style: if selected { Style::default().fg(SELECT).add_modifier(Modifier::BOLD) } else { fg(TEXT) },
            elsewhere: app.live_elsewhere(node) && status != Status::External,
            subtitle: box_subtitle(app, node),
            border: box_border(node, status, selected, attention.is_some()),
            border_style: box_border_style(node, attention, selected, focused_term),
            fill: attention.map(|(_, bg)| bg).unwrap_or(PANEL),
            attention,
            selected,
            focused_term,
            active: active_term.as_deref() == Some(node.id.as_str()),
            every,
            mailbox: app.bus.mailbox.get(&node.id).copied().unwrap_or(0),
        });
    }

    for (from, to, start) in &app.pulses {
        let (Some(a), Some(b)) = (app.doc.node(from), app.doc.node(to)) else { continue };
        if from != to {
            let (a, b) = (node_rect(a), node_rect(b));
            model.pulses.push(PulseModel { from: a, to: b, path: connector_path_f(a, b), t: (start.elapsed().as_millis() as f32 / 1_200.0).min(1.0) });
        }
    }
    if let Some(node) = active_term.as_deref().and_then(|id| app.doc.node(id)) {
        model.halo = Some((node_rect(node), typing));
    }
    if matches!(app.drag, Some(Drag::Rubber { .. })) {
        let (o, p) = (app.drag_origin, app.pointer);
        model.rubber = Some((o.0.min(p.0), o.1.min(p.1), o.0.max(p.0), o.1.max(p.1)));
    }
    app.layout.hits.extend(chip_hits);
    model
}

/// Text rendering of a canvas model (for the GUI's test hooks and the
/// parity check with `draw_canvas`): the same characters the TUI draws, at
/// the nearest discrete zoom, each shape at the cells whose centres it covers.
pub fn paint_model(model: &CanvasModel, buf: &mut Buffer) {
    let clip = model.area;
    let s = model.scale;
    let zoom = if s >= 0.75 {
        0
    } else if s >= 0.375 {
        1
    } else {
        2
    };
    let at = |wx: f32, wy: f32| {
        let (x, y) = model.to_screen(wx, wy);
        (x.round() as i32, y.round() as i32)
    };
    let cells = |r: WRect, min_w: i32, min_h: i32| {
        let (x, y) = at(r.x, r.y);
        (x, y, ((r.w * s).round() as i32).max(min_w), ((r.h * s).round() as i32).max(min_h))
    };
    if model.grid {
        draw_grid(buf, clip, at(0.0, 0.0), (((6.0 * s) as i32).max(3), ((3.0 * s) as i32).max(2)));
    }
    let off = |x: i32, y: i32, w: i32, h: i32| x + w < clip.x as i32 || x > clip.right() as i32 || y + h < clip.y as i32 || y > clip.bottom() as i32;
    for group in &model.groups {
        let (x, y, w, h) = cells(group.rect, 6, 2);
        if off(x, y, w, h) {
            continue;
        }
        outline(buf, x, y, w, h, if group.selected { &HEAVY } else { &DASHED }, group.style, clip);
        let title = format!(" {} {} · {} ", group.icon, group.name, group.members);
        put(buf, x + 2, y, &truncate(&title, (w - 4).max(0) as usize), Style::default().fg(group.color).add_modifier(Modifier::BOLD), clip);
        if zoom == 0 {
            for (fx, fy, family) in &group.families {
                let (sx, sy) = at(*fx, *fy);
                if sy > y && sy < y + h - 1 {
                    put(buf, sx, sy, &truncate(family, (x + w - 2 - sx).max(0) as usize), fg(MUTED).add_modifier(Modifier::ITALIC), clip);
                }
            }
        }
        if w >= 44 {
            let style = if group.has_folder { Style::default().fg(group.color).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
            for chip in &group.chips {
                let (cx, _) = at(chip.rect.x, chip.rect.y);
                put(buf, cx, y, chip.label, style, clip);
            }
        }
        put_sym(buf, x + w - 1, y + h - 1, "◢", fg(group.color), clip);
    }
    let screen_box = |r: WRect| cells(r, 4, 1);
    for edge in &model.edges {
        let (vertical, horizontal) = if edge.kind == EdgeKind::Subsession { ("┊", "┄") } else { ("│", "─") };
        let path = connector_path(screen_box(edge.from), screen_box(edge.to));
        draw_path(buf, &path, vertical, horizontal, fg(edge.color), clip);
    }
    for b in &model.boxes {
        let (x, y, w, h) = screen_box(b.rect);
        if off(x, y, w, h) {
            continue;
        }
        let status_style = fg(b.status.color());
        match zoom {
            0 => {
                fill(buf, x + 1, y + 1, w - 2, h - 2, Style::default().bg(b.fill), clip);
                outline(buf, x, y, w, h, border_set(b.border), b.border_style, clip);
                put(buf, x + 1, y, &truncate(&format!(" {}{} ", b.prefix, b.name), (w - 2) as usize), b.name_style, clip);
                let end = put(buf, x + 2, y + 1, &truncate(&b.line, (w - 4) as usize), b.line_style.bg(b.fill), clip);
                if b.elsewhere {
                    put(buf, end, y + 1, &truncate(" · ↗ elsewhere", (x + w - 2 - end).max(0) as usize), fg(crate::paint::PURPLE).bg(b.fill), clip);
                }
                if h >= 4 {
                    put(buf, x + 2, y + 2, &truncate(&b.subtitle, (w - 4) as usize), fg(MUTED).bg(b.fill), clip);
                }
            }
            1 => {
                let bg = b.attention.map(|(_, bg)| bg).unwrap_or(PANEL);
                fill(buf, x, y, w, h, Style::default().bg(bg), clip);
                let end = put(buf, x, y, b.icon, status_style.bg(bg), clip);
                put(buf, end + 1, y, &truncate(&b.name, (w - 2).max(0) as usize), b.name_style.bg(bg), clip);
                if h >= 2 {
                    let label_style = b.attention.map(|(c, _)| crate::paint::bold(c)).unwrap_or(fg(MUTED));
                    put(buf, x, y + 1, &truncate(b.status.label(), w as usize), label_style.bg(bg), clip);
                }
                if b.selected {
                    for xx in x..x + w {
                        paint::style_at(buf, xx, y + h - 1, Style::default().add_modifier(Modifier::UNDERLINED), clip);
                    }
                }
            }
            _ => {
                let style = if b.active {
                    crate::paint::bold(ACCENT).add_modifier(Modifier::UNDERLINED)
                } else {
                    b.attention.map(|(c, bg)| crate::paint::bold(c).bg(bg)).unwrap_or(b.name_style)
                };
                let end = put(buf, x, y, b.icon, b.attention.map(|(_, bg)| status_style.bg(bg)).unwrap_or(status_style), clip);
                put(buf, end, y, &truncate(&b.name, (w - 1).max(0) as usize), style, clip);
            }
        }
    }
    if zoom < 2 {
        for b in model.boxes.iter().filter(|b| b.mailbox > 0) {
            let (x, y, w, _) = screen_box(b.rect);
            draw_mailbox(buf, (x, y, w), b.mailbox, clip);
        }
    }
    for pulse in &model.pulses {
        draw_pulse(buf, &connector_path(screen_box(pulse.from), screen_box(pulse.to)), pulse.t, clip);
    }
    for lp in &model.loops {
        let (x, y, w, h) = screen_box(lp.rect);
        draw_loop(buf, (x, y, w + (zoom < 2 && lp.halo) as i32, h), model.tick, clip);
    }
    if zoom < 2
        && let Some((rect, heavy)) = model.halo
    {
        let (x, y, w, h) = screen_box(rect);
        outline(buf, x - 1, y - 1, w + 2, h + 2, if heavy { &HEAVY } else { &LIGHT }, crate::paint::bold(ACCENT), clip);
    }
    if let Some((x0, y0, x1, y1)) = model.rubber {
        let (x0, y0, x1, y1) = (x0.floor() as i32, y0.floor() as i32, x1.floor() as i32, y1.floor() as i32);
        outline(buf, x0, y0, (x1 - x0 + 1).max(2), (y1 - y0 + 1).max(2), &DASHED, fg(SELECT), clip);
    }
    if model.empty {
        draw_empty_hint(buf, clip);
    }
}

/// Orthogonal route (world units) from a parent box to a child box, leaving
/// and entering through the edges that face each other (`connector_path` in
/// fractional coordinates).
pub fn connector_path_f(parent: WRect, child: WRect) -> Vec<(f32, f32)> {
    let (p, c) = (parent, child);
    if c.x > p.right() || c.right() < p.x {
        // Side by side.
        let right = c.x > p.right();
        let (sx, ex) = if right { (p.right(), c.x) } else { (p.x, c.right()) };
        let (sy, ey) = (p.y + p.h / 2.0, c.y + c.h / 2.0);
        let mx = (sx + ex) / 2.0;
        vec![(sx, sy), (mx, sy), (mx, ey), (ex, ey)]
    } else {
        // Above or below.
        let below = c.y >= p.bottom();
        let (sy, ey) = if below { (p.bottom(), c.y) } else { (p.y, c.bottom()) };
        let (sx, ex) = (p.x + 2.0f32.min(p.w - 1.0) + 0.5, c.x + 2.0f32.min(c.w - 1.0) + 0.5);
        let my = (sy + ey) / 2.0;
        vec![(sx, sy), (sx, my), (ex, my), (ex, ey)]
    }
}

fn automation_label(status: crate::status::Status) -> &'static str {
    use crate::status::Status;
    match status {
        Status::Idle => "scheduled",
        Status::Stopped => "disabled",
        Status::JustFinished => "ran recently",
        Status::Working => "running",
        Status::Failed => "last run failed",
        Status::Waiting => "warning",
        other => other.label(),
    }
}

/// Cells of a loopback arc on a box's right side, in travel order: out along
/// the top row, down, and back into the box along the bottom row.
pub fn loop_path(x: i32, y: i32, w: i32, h: i32) -> Vec<(i32, i32, &'static str)> {
    let right = x + w;
    if h < 2 {
        return vec![(right, y, "↻")];
    }
    let bottom = y + h - 1;
    let mut path = vec![(right, y, "┄"), (right + 1, y, "┄"), (right + 2, y, "╮")];
    path.extend((y + 1..bottom).map(|row| (right + 2, row, "┆")));
    path.extend([(right + 2, bottom, "╯"), (right + 1, bottom, "┄"), (right, bottom, "◂")]);
    path
}

type BoxRect = (i32, i32, i32, i32);

/// Orthogonal route from a parent box to a child box (screen cells), leaving
/// and entering through the edges that face each other.
pub fn connector_path(parent: BoxRect, child: BoxRect) -> Vec<(i32, i32)> {
    let (px, py, pw, ph) = parent;
    let (cx, cy, cw, ch) = child;
    if cx > px + pw || cx + cw < px {
        // Side by side.
        let right = cx > px + pw;
        let (sx, ex) = if right { (px + pw, cx - 1) } else { (px - 1, cx + cw) };
        let (sy, ey) = (py + ph / 2, cy + ch / 2);
        let mx = (sx + ex) / 2;
        vec![(sx, sy), (mx, sy), (mx, ey), (ex, ey)]
    } else {
        // Above or below.
        let below = cy >= py + ph;
        let (sy, ey) = if below { (py + ph, cy - 1) } else { (py - 1, cy + ch) };
        let (sx, ex) = (px + 2.min(pw - 1), cx + 2.min(cw - 1));
        let my = (sy + ey) / 2;
        vec![(sx, sy), (sx, my), (ex, my), (ex, ey)]
    }
}

fn corner(from: (i32, i32), at: (i32, i32), to: (i32, i32)) -> Option<&'static str> {
    let (din, dout) = ((at.0 - from.0, at.1 - from.1), (to.0 - at.0, to.1 - at.1));
    let horizontal_in = din.0 != 0;
    let (h, v) = if horizontal_in { (din.0.signum(), dout.1.signum()) } else { (dout.0.signum(), din.1.signum()) };
    if h == 0 || v == 0 {
        return None;
    }
    // Which two arms meet here: the horizontal one points away from x, the vertical one away from y.
    let (left, up) = if horizontal_in { (h > 0, v < 0) } else { (h < 0, v > 0) };
    Some(match (left, up) {
        (true, true) => "╯",
        (true, false) => "╮",
        (false, true) => "╰",
        (false, false) => "╭",
    })
}

fn draw_path(buf: &mut Buffer, path: &[(i32, i32)], vertical: &str, horizontal: &str, style: Style, clip: Rect) {
    let (lo_x, hi_x) = (clip.x as i32 - 1, clip.right() as i32);
    let (lo_y, hi_y) = (clip.y as i32 - 1, clip.bottom() as i32);
    for pair in path.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if y0 == y1 {
            for x in x0.min(x1).max(lo_x)..=x0.max(x1).min(hi_x) {
                put_sym(buf, x, y0, horizontal, style, clip);
            }
        } else {
            for y in y0.min(y1).max(lo_y)..=y0.max(y1).min(hi_y) {
                put_sym(buf, x0, y, vertical, style, clip);
            }
        }
    }
    for i in 1..path.len().saturating_sub(1) {
        // Skip zero-length segments when picking the corner shape.
        let from = path[..i].iter().rev().find(|p| **p != path[i]).copied();
        let to = path[i + 1..].iter().find(|p| **p != path[i]).copied();
        if let (Some(from), Some(to)) = (from, to)
            && let Some(glyph) = corner(from, path[i], to)
        {
            put_sym(buf, path[i].0, path[i].1, glyph, style, clip);
        }
    }
}

#[cfg(test)]
mod connector_tests {
    use super::*;

    #[test]
    fn loop_path_is_a_closed_arc_outside_the_box() {
        let path = loop_path(10, 5, 30, 4);
        assert_eq!(path.len(), 8);
        assert!(path.iter().all(|(x, _, _)| *x >= 40 && *x <= 42), "right of the box, within the 4-column gutter");
        assert_eq!((path[0].0, path[0].1), (40, 5), "leaves at the top row");
        assert_eq!(*path.last().unwrap(), (40, 8, "◂"), "returns into the bottom row");
        for pair in path.windows(2) {
            assert_eq!((pair[0].0 - pair[1].0).abs() + (pair[0].1 - pair[1].1).abs(), 1, "contiguous");
        }
        assert_eq!(loop_path(0, 0, 10, 1), vec![(10, 0, "↻")], "one-line boxes get an icon");
    }

    #[test]
    fn connectors_reach_children_in_every_direction() {
        let parent = (40, 20, 30, 4);
        for child in [(80, 30, 30, 4), (0, 30, 30, 4), (80, 5, 30, 4), (0, 5, 30, 4), (42, 30, 30, 4), (42, 5, 30, 4)] {
            let path = connector_path(parent, child);
            let (start, end) = (path[0], *path.last().unwrap());
            let touches = |(x, y): (i32, i32), (bx, by, bw, bh): BoxRect| x >= bx - 1 && x <= bx + bw && y >= by - 1 && y <= by + bh;
            assert!(touches(start, parent) && touches(end, child), "{child:?}: {path:?}");
            assert!(path.windows(2).all(|w| w[0].0 == w[1].0 || w[0].1 == w[1].1), "orthogonal");
        }
    }

    #[test]
    fn corners_match_turn_direction() {
        assert_eq!(corner((0, 0), (5, 0), (5, 3)), Some("╮"));
        assert_eq!(corner((0, 3), (5, 3), (5, 0)), Some("╯"));
        assert_eq!(corner((5, 0), (5, 3), (9, 3)), Some("╰"));
        assert_eq!(corner((5, 3), (5, 0), (9, 0)), Some("╭"));
        assert_eq!(corner((9, 0), (5, 0), (5, 3)), Some("╭"));
        assert_eq!(corner((0, 0), (5, 0), (9, 0)), None);
    }
}
