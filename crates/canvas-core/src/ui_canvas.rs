//! Canvas rendering: groups, connectors, session and subagent boxes, stacks,
//! hidden subsessions, dust and effects. Cells mode draws characters
//! ([`draw_canvas`]); Gpu mode describes the same canvas in world units
//! ([`canvas_model`]) for the front end to draw at any scale. Both build the
//! same [`BoxModel`]s, and [`paint_box`] draws one as characters.

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
use crate::status::{Status, StatusStyle};
use crate::view::{Border, BoxKind, BoxModel, CanvasModel, ChipModel, DeckInfo, EdgeKind, EdgeModel, EffectModel, GroupModel, Hidden, LoopModel, PulseModel, WRect};
use crate::visual::{Deck, VisualFrame};

/// Gpu mode: chips are laid out in world units (scaled with the zoom) from
/// this scale up; below it they keep a readable size on screen.
pub const CHIP_SCALE: f32 = 0.7;

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

/// Marker before a box's name: canvas-wide agent, passive, subagent,
/// automation, subsession.
pub fn box_prefix(node: &Node) -> &'static str {
    match (node.kind, node.parent.is_some()) {
        _ if node.is_canvas_agent() => "◎ ",
        _ if node.passive.is_some() => "⟲ ",
        (NodeKind::Agent, _) => "◇ ",
        (NodeKind::Automation, _) => "◷ ",
        (NodeKind::Session, true) => "↳ ",
        _ => "",
    }
}

/// The icon on a group's title: the automations' ◷, the canvas agents' ◎, else ▣.
pub fn group_icon(group: &crate::doc::Group) -> &'static str {
    if group.is_automations() {
        "◷"
    } else if group.is_canvas_agents() {
        crate::canvas_wide::MARK
    } else {
        "▣"
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

/// What every box of a frame shares: the animation tick, the active
/// terminal and the flashing box.
pub struct Boxes {
    pub tick: usize,
    pub active_term: Option<String>,
    flash: Option<(String, Duration)>,
}

impl Boxes {
    pub fn new(app: &App) -> Self {
        Self {
            tick: app.tick,
            active_term: app.focused_term.clone().filter(|id| app.visible_terminals().contains(id)),
            flash: app.flash.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_millis(1500)).map(|(id, t)| (id.clone(), t.elapsed())),
        }
    }

    /// A box or group a sidebar click revealed blinks for 1.5 s.
    pub fn flashing(&self, id: &str) -> bool {
        self.flash.as_ref().is_some_and(|(f, t)| f == id && (t.as_millis() / 250) % 2 == 0)
    }
}

/// The box of a node (or of a stack's top card) at `rect`: labels, status
/// styles, attention colours and the dust of an unused session.
pub fn box_model(app: &App, node: &Node, rect: WRect, status: Status, selected: bool, boxes: &Boxes) -> BoxModel {
    let tick = boxes.tick;
    let focused_term = app.focused_term.as_deref() == Some(&node.id) && app.owned_alive(&node.id);
    let attention = box_attention(node, status, tick);
    let dust = if attention.is_none() { app.dust_look(node, status) } else { None };
    // A canvas-wide agent shows its own state's icon and colour (paused, off, failing, …).
    let agent_state = node.is_canvas_agent().then(|| app.canvas_agent_state(node));
    let icon = agent_state.as_ref().map(|s| s.icon(tick)).unwrap_or_else(|| status.icon(tick));
    let label = box_label(node, status);
    let every = app.node_loop(node);
    let status_style = fg(agent_state.as_ref().map(|s| s.color()).unwrap_or_else(|| status.color()));
    let kind = match node.kind {
        _ if node.passive.is_some() => BoxKind::Passive,
        NodeKind::Session => BoxKind::Session,
        NodeKind::Agent => BoxKind::Agent,
        NodeKind::Automation => BoxKind::Automation,
    };
    let mut line = box_line(app, node, icon, label, every.as_deref());
    let mut line_style = if attention.is_some() { status_style.add_modifier(Modifier::BOLD) } else { status_style };
    let mut name_style = if selected { Style::default().fg(SELECT).add_modifier(Modifier::BOLD) } else { fg(TEXT) };
    let mut border_style = box_border_style(node, attention, selected, focused_term);
    let mut fill = attention.map(|(_, bg)| bg).unwrap_or(PANEL);
    if let Some(dust) = &dust {
        // Dust: drained colours and the age, never over anything important.
        line.push_str(&dust.suffix);
        line_style = fg(MUTED);
        if !selected {
            name_style = fg(MUTED);
        }
        if !selected && !focused_term {
            border_style = dust.edge;
        }
        fill = dust.fill;
    }
    BoxModel {
        id: node.id.clone(),
        kind,
        rect,
        prefix: box_prefix(node),
        name: node.name.clone(),
        status,
        icon,
        label,
        line,
        line_style,
        name_style,
        elsewhere: app.live_elsewhere(node) && status != Status::External,
        subtitle: box_subtitle(app, node),
        border: box_border(node, status, selected, attention.is_some()),
        border_style,
        fill,
        attention,
        selected,
        focused_term,
        active: boxes.active_term.as_deref() == Some(node.id.as_str()),
        every,
        mailbox: app.bus.mailbox.get(&node.id).copied().unwrap_or(0),
        dust: dust.map(|d| d.dust.level()).unwrap_or(0),
        alpha: 1.0,
        deck: None,
        hidden: None,
    }
}

/// A deck's top card. A stack: the newest stacked session with the most
/// important status of the deck (needs you, failed, done · review,
/// working). Folded subagent boxes alone: a slim "◇ 12 subagents" card with
/// what they do ("working · 2 of 12", "failed · 1 running", "just finished").
pub fn deck_model(app: &App, deck: &Deck, rect: WRect, boxes: &Boxes) -> Option<BoxModel> {
    let top = app.doc.node(&deck.top)?;
    let status = deck.status.unwrap_or_else(|| app.node_status(top));
    let selected = deck.members.iter().any(|s| app.selection.contains(s) || boxes.flashing(s));
    let mut b = box_model(app, top, rect, status, selected, boxes);
    let count = (deck.sessions.len() + deck.agents.len()) as u32;
    let agents = deck.agents.len();
    if deck.agents_only() {
        let running = deck.running;
        b.name = format!("{agents} subagent{}", if agents == 1 { "" } else { "s" });
        b.line = match running {
            n if status == Status::Working && agents > 1 => format!("{} {} · {n} of {agents}", b.icon, b.label),
            _ if status == Status::Working => format!("{} {}", b.icon, b.label),
            0 => format!("{} {}", b.icon, b.label),
            n => format!("{} {} · {n} running", b.icon, b.label),
        };
        b.subtitle = "double-click to show them".into();
    } else if agents > 0 {
        b.subtitle = format!("stack of {} · {agents} subagent{} · double-click to unfold", deck.sessions.len(), if agents == 1 { "" } else { "s" });
    } else {
        b.subtitle = format!("stack of {count} · double-click to unstack");
    }
    b.dust = 0;
    b.alpha = deck.alpha;
    b.mailbox = deck.sessions.iter().map(|s| app.bus.mailbox.get(s).copied().unwrap_or(0)).sum();
    b.active = deck.sessions.iter().any(|s| boxes.active_term.as_deref() == Some(s.as_str()));
    b.deck = Some(DeckInfo { root: deck.root.clone(), count, back: deck.back(), agents: agents as u32 });
    Some(b)
}

/// Subsessions and subagent boxes hidden into a box: how many of each and
/// the attention colour of the most important one.
fn hidden_badge(frame: &VisualFrame, id: &str) -> Option<Hidden> {
    frame.hidden_badges.get(id).copied()
}

pub fn draw_canvas(app: &mut App, buf: &mut Buffer, area: Rect) {
    let clip = area;
    let frame = app.visual_frame();
    app.prepare_effects();
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

    let boxes = Boxes::new(app);

    // Group under a dragged node (drop target).
    let drop_target = match &app.drag {
        Some(Drag::Nodes { ids, moved: true, .. }) => ids.first().and_then(|id| app.doc.node(id)).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()),
        _ => None,
    };

    let mut member_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for group in app.doc.groups_by_node().into_values() {
        *member_counts.entry(group.id.as_str()).or_default() += 1;
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
        let selected = app.selected_group.as_deref() == Some(&group.id) || boxes.flashing(&group.id);
        let target = drop_target.as_deref() == Some(&group.id);
        let style = if selected || target { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { fg(color).add_modifier(Modifier::DIM) };
        outline(buf, x, y, w, h, if selected { &HEAVY } else { &DASHED }, style, clip);
        let members = member_counts.get(group.id.as_str()).copied().unwrap_or(0);
        let icon = group_icon(group);
        let title = format!(" {icon} {} · {members} ", group.name);
        // Chips on the title row: passive sessions and folders (compact when narrow).
        let chips = app.fit_group_chips(group, paint::text_width(&title) as i32, w);
        let chips_w = crate::folders::chips_width(&chips);
        put(buf, x + 2, y, &truncate(&title, (w - 4 - chips_w).max(0) as usize), Style::default().fg(color).add_modifier(Modifier::BOLD), clip);
        // Family names above each row of automation boxes.
        if group.is_automations() && app.doc.view.zoom == 0 {
            for (fx, fy, family) in app.family_labels(group) {
                let (sx, sy) = app.to_screen(fx, fy);
                if sy > y && sy < y + h - 1 {
                    put(buf, sx, sy, &truncate(&family, (x + w - 2 - sx).max(0) as usize), fg(MUTED).add_modifier(Modifier::ITALIC), clip);
                }
            }
        }
        let mut cx = x + w - 2 - chips_w;
        for chip in chips {
            let style = if chip.lit { Style::default().fg(color).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
            let end = put(buf, cx, y, &chip.label, style, clip);
            if cx >= clip.x as i32 && end <= clip.right() as i32 && y >= clip.y as i32 && y < clip.bottom() as i32 {
                chip_hits.push((ratatui::layout::Rect::new(cx as u16, y as u16, (end - cx) as u16, 1), crate::dialogs::Hit::Toolbar(chip.action)));
            }
            cx = end + 1;
        }
        put_sym(buf, x + w - 1, y + h - 1, "◢", fg(color), clip);
    }

    app.layout.hits.extend(chip_hits);

    // Connectors: parent → subagent (thin) and parent → subsession (accent),
    // routed between facing edges in whichever direction the child lies.
    let by_id: std::collections::HashMap<&str, &Node> = app.doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let deck_screen = |deck: &Deck| {
        let (x, y) = app.to_screen(deck.rect.x as i32, deck.rect.y as i32);
        let (w, h) = app.scaled(deck.rect.w as i32, deck.rect.h as i32, 4, 1);
        (x, y, w, h)
    };
    let mut links: Vec<(BoxRect, BoxRect, bool)> = Vec::new();
    for node in &app.doc.nodes {
        let Some(parent) = node.parent.as_deref().and_then(|p| by_id.get(p).copied()) else { continue };
        if frame.skip(&node.id) || frame.skip(&parent.id) {
            continue;
        }
        links.push((app.node_screen(parent), app.node_screen(node), node.kind == NodeKind::Session));
    }
    for deck in &frame.decks {
        if let Some(root) = app.doc.node(&deck.root) {
            links.push((app.node_screen(root), deck_screen(deck), !deck.agents_only()));
        }
    }
    for ((px, py, pw, ph), (cx, cy, cw, ch), subsession) in links {
        // Skip connectors entirely off screen (their bounding box misses the canvas).
        let (lx, ly) = (px.min(cx) - 1, py.min(cy) - 1);
        let (hx, hy) = ((px + pw).max(cx + cw) + 1, (py + ph).max(cy + ch) + 1);
        if hx < clip.x as i32 || lx > clip.right() as i32 || hy < clip.y as i32 || ly > clip.bottom() as i32 {
            continue;
        }
        let style = if subsession { fg(ACCENT) } else { fg(MUTED) };
        let (vertical, horizontal) = if subsession { ("┊", "┄") } else { ("│", "─") };
        let path = connector_path((px, py, pw, ph), (cx, cy, cw, ch));
        draw_path(buf, &path, vertical, horizontal, style, clip);
    }

    // Nodes, then the decks, then the selected nodes (on top).
    let mut order: Vec<&Node> = app.doc.nodes.iter().filter(|n| !frame.skip(&n.id)).collect();
    order.sort_by_key(|n| app.selection.contains(&n.id));
    let zoom = app.doc.view.zoom;
    // The session whose terminal is the active one on the right.
    let active_term = boxes.active_term.clone();
    let typing = app.focus == crate::app::Focus::Terminal;
    let paint_decks = |buf: &mut Buffer| {
        // Decks: the top card beneath its session, the other cards behind it.
        for deck in &frame.decks {
            let (x, y, w, h) = deck_screen(deck);
            if x + w + 2 < area.x as i32 || x > area.right() as i32 || y + h + 2 < area.y as i32 || y > area.bottom() as i32 {
                continue;
            }
            if let Some(b) = deck_model(app, deck, deck.rect, &boxes) {
                paint_box(buf, &b, (x, y, w, h), zoom, clip);
            }
        }
    };
    let first_selected = order.iter().position(|n| app.selection.contains(&n.id));
    for (i, node) in order.into_iter().enumerate() {
        if Some(i) == first_selected {
            paint_decks(buf);
        }
        let (x, y, w, h) = app.node_screen(node);
        if x + w < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 {
            continue;
        }
        let status = app.node_status(node);
        let selected = app.selection.contains(&node.id) || boxes.flashing(&node.id);
        let mut b = box_model(app, node, WRect::from(node.rect()), status, selected, &boxes);
        b.hidden = hidden_badge(&frame, &node.id);
        paint_box(buf, &b, (x, y, w, h), zoom, clip);
    }
    if first_selected.is_none() {
        paint_decks(buf);
    }

    // Queued messages for a session: a badge on its top edge.
    if zoom < 2 {
        for (id, count) in app.bus.mailbox.iter().filter(|(_, c)| **c > 0) {
            if let Some(node) = app.doc.node(id).filter(|n| !frame.skip(&n.id)) {
                let (x, y, w, _) = app.node_screen(node);
                draw_mailbox(buf, (x, y, w), *count, clip);
            }
        }
    }

    // Events in flight: a bright dot runs from the sender to each receiver.
    for (from, to, start) in &app.pulses {
        let (Some(a), Some(b)) = (app.doc.node(from), app.doc.node(to)) else { continue };
        if from == to || frame.skip(from) || frame.skip(to) {
            continue;
        }
        let path = connector_path(app.node_screen(a), app.node_screen(b));
        draw_pulse(buf, &path, start.elapsed().as_millis() as f32 / 1_200.0, clip);
    }

    // Looping boxes: a dotted loopback on the right side with one lit dot
    // running around it (a two-cell trail), like a loading bar; with reduced
    // motion the arc and its arrowhead hold still.
    let (clock, _) = app.anim_clock();
    let moving = app.motion() == crate::view::Motion::On;
    for node in app.doc.nodes.iter().filter(|n| !frame.skip(&n.id)) {
        let (x, y, w, h) = app.node_screen(node);
        if x + w + 3 < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 {
            continue;
        }
        if app.node_loop(node).is_none() {
            continue;
        }
        // Outside the active terminal's halo, which runs one cell around the box.
        let halo = zoom < 2 && active_term.as_deref() == Some(node.id.as_str());
        let lap = moving.then(|| crate::loops::lap(clock, app.loop_seed(&node.id)));
        draw_loop(buf, (x, y, w + halo as i32, h), lap, clip);
    }

    // Effects: a badge on the box (or the stack that holds it), or the group.
    let effects = app.effect_models(&frame, &|n: &Node| WRect::from(n.rect()));
    for effect in &effects {
        let (x, y) = app.to_screen(effect.rect.x as i32, effect.rect.y as i32);
        let (w, h) = app.scaled(effect.rect.w as i32, effect.rect.h as i32, 4, 1);
        paint_effect(buf, effect, (x, y, w, h), clip);
    }

    // Halo one cell outside the active terminal's box (drawn last so neighbours
    // cannot cover it): thin while its terminal is showing, heavy while typing.
    if zoom < 2
        && let Some(node) = active_term.as_deref().filter(|id| !frame.skip(id)).and_then(|id| app.doc.node(id))
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

/// One box as characters at screen cells `(x, y, w, h)` for a zoom step
/// (0: full, 1: compact, 2: one line), with a deck's back cards and the
/// stack and hidden-box badges.
pub fn paint_box(buf: &mut Buffer, b: &BoxModel, (x, y, w, h): (i32, i32, i32, i32), zoom: u8, clip: Rect) {
    let status_style = fg(b.status.color());
    match zoom {
        0 => {
            if let Some(deck) = &b.deck {
                let cards = if b.kind == BoxKind::Agent { &LIGHT } else { &ROUND };
                for i in (1..=deck.back as i32).rev() {
                    fill(buf, x + i + 1, y + i + 1, w - 2, h - 2, Style::default().bg(PANEL), clip);
                    outline(buf, x + i, y + i, w, h, cards, fg(MUTED), clip);
                }
            }
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
            let bg = b.fill;
            fill(buf, x, y, w, h, Style::default().bg(bg), clip);
            let end = put(buf, x, y, b.icon, status_style.bg(bg), clip);
            put(buf, end + 1, y, &truncate(&b.name, (w - 2).max(0) as usize), b.name_style.bg(bg), clip);
            if h >= 2 {
                let label_style = b.attention.map(|(c, _)| crate::paint::bold(c)).unwrap_or(fg(MUTED));
                put(buf, x, y + 1, &truncate(b.label, w as usize), label_style.bg(bg), clip);
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
    if zoom < 2 {
        if let Some(badge) = b.deck.as_ref().and_then(DeckInfo::badge) {
            let badge = format!(" {badge} ");
            put(buf, x + w - 2 - paint::text_width(&badge) as i32, y, &badge, crate::paint::bold(PANEL).bg(ACCENT), clip);
        }
        if let Some(hidden) = b.hidden {
            let badge = format!(" {} ", hidden.text());
            let style = match hidden.color {
                Some(color) => crate::paint::bold(PANEL).bg(color),
                None => fg(TEXT).bg(crate::paint::PANEL_HI),
            };
            put(buf, x + w - 2 - paint::text_width(&badge) as i32, y + h - 1, &badge, style, clip);
        }
    }
}

/// An effect's badge on its anchor's bottom edge (a box, a stack or a group).
pub fn paint_effect(buf: &mut Buffer, effect: &EffectModel, (x, y, w, h): (i32, i32, i32, i32), clip: Rect) {
    let text = format!(" {} ", effect.badge);
    let room = (w - 4).max(4) as usize;
    put(buf, x + 2, y + h.max(1) - 1, &truncate(&text, room), crate::visual::effect_style(effect.kind), clip);
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

/// The loopback arc beside a box with its lit dot and a two-cell trail at
/// lap fraction `lap` (`crate::loops`: the dot keeps one speed along the
/// cells); `None` (reduced motion): the arc with its lit arrowhead, still.
fn draw_loop(buf: &mut Buffer, (x, y, w, h): (i32, i32, i32, i32), lap: Option<f32>, clip: Rect) {
    use crate::paint::{LOOP, LOOP_LIT, bold};
    let path = loop_path(x, y, w, h);
    if path.len() == 1 {
        // One-line boxes (zoomed out): the ↻ icon pulses (steady with reduced motion).
        let (px, py, glyph) = path[0];
        let lit = lap.is_none_or(|t| ((t * 4.0) as usize).is_multiple_of(2));
        put(buf, px, py, glyph, bold(if lit { LOOP_LIT } else { LOOP }), clip);
        return;
    }
    let n = path.len();
    let head = lap.map(|t| crate::loops::cell_at(&path, crate::loops::along(t)));
    for (i, &(px, py, glyph)) in path.iter().enumerate() {
        let (glyph, style) = match head.map(|h| (h + n - i) % n) {
            Some(0) => ("●", bold(LOOP_LIT)),
            Some(1) => ("•", fg(LOOP_LIT)),
            Some(2) => (glyph, fg(LOOP_LIT)),
            // The arrowhead back into the box.
            _ if i == n - 1 => (glyph, bold(LOOP_LIT)),
            _ => (glyph, fg(LOOP)),
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
/// what the document moves (`Doc::nodes_moving_with`, `Doc::group_moving_with`),
/// each box once.
fn dragged(app: &App) -> (HashSet<String>, HashSet<String>) {
    match &app.drag {
        Some(Drag::Nodes { ids, .. }) => (app.doc.nodes_moving_with(ids).into_iter().collect(), HashSet::new()),
        Some(Drag::Group { id, .. }) => {
            let (nodes, groups) = app.doc.group_moving_with(id);
            (nodes.into_iter().collect(), groups.into_iter().collect())
        }
        _ => (HashSet::new(), HashSet::new()),
    }
}

/// Chips of a group in Gpu mode: in world units from [`CHIP_SCALE`] up,
/// else sized in screen cells (readable at any zoom) while the title shows.
fn chip_models(app: &App, group: &crate::doc::Group, rect: WRect, title: &str, scale: f32) -> Vec<ChipModel> {
    let title_w = paint::text_width(title) as i32;
    let fixed = scale < CHIP_SCALE;
    // Cells per chip cell: 1 at a readable size, else the zoom's.
    let (row, title_cells, unit) = if fixed {
        ((rect.w * scale) as i32, (title_w as f32 * scale.max(0.6)) as i32, 1.0 / scale)
    } else {
        (group.w, title_w, 1.0)
    };
    if fixed && (row < 12 || rect.h * scale < 1.5) {
        return Vec::new();
    }
    let chips = app.fit_group_chips(group, title_cells, row);
    let total = crate::folders::chips_width(&chips) as f32 * unit;
    let mut cx = rect.right() - 2.0 * unit - total;
    let mut out = Vec::new();
    for chip in chips {
        let w = chip.width() as f32 * unit;
        out.push(ChipModel { rect: WRect::new(cx, rect.y, w, unit), label: chip.label, action: chip.action, fixed, lit: chip.lit });
        cx += w + unit;
    }
    out
}

/// The canvas as a model in world units (Gpu mode): the same groups,
/// connectors, boxes, styles and animations `draw_canvas` draws, culled to
/// `area`, plus stacks, hidden subsessions, effects and transitions.
/// Registers the chips' click targets.
pub fn canvas_model(app: &mut App, area: Rect) -> CanvasModel {
    let frame = app.visual_frame();
    app.prepare_effects();
    let scale = app.scale_f();
    let origin = app.origin_f();
    let tick = app.tick;
    // Loops that start or stop grow in or fade out (every box, in view or not).
    let (clock_ms, frozen) = app.anim_clock();
    let looping: HashSet<String> = app.doc.nodes.iter().filter(|n| app.node_loop(n).is_some()).map(|n| n.id.clone()).collect();
    app.sync_loops(looping, clock_ms, frozen);
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
        effects: Vec::new(),
        motion: app.motion(),
        clock_ms,
        frozen,
    };
    let visible = model.visible();
    // Loop arcs, halos and labels reach a few units past a box.
    let shows = |r: WRect| r.right() + 4.0 >= visible.x && r.x - 1.0 <= visible.right() && r.bottom() + 2.0 >= visible.y && r.y - 2.0 <= visible.bottom();
    let boxes = Boxes::new(app);
    let drop_target = match &app.drag {
        Some(Drag::Nodes { ids, moved: true, .. }) => ids.first().and_then(|id| app.doc.node(id)).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()),
        _ => None,
    };
    let offset = app.drag_offset();
    let (moving_nodes, moving_groups) = dragged(app);
    let home_rect = |node: &Node| {
        let rect = WRect::from(node.rect());
        if moving_nodes.contains(node.id.as_str()) { rect.offset(offset) } else { rect }
    };
    // Boxes in a stack or hide transition fly; the rest sit at home.
    let node_rect = |node: &Node| frame.moving.get(&node.id).map(|(r, _)| *r).unwrap_or_else(|| home_rect(node));
    let deck_rect = |deck: &Deck| if moving_nodes.contains(deck.root.as_str()) { deck.rect.offset(offset) } else { deck.rect };

    let mut member_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for group in app.doc.groups_by_node().into_values() {
        *member_counts.entry(group.id.as_str()).or_default() += 1;
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
        let selected = app.selected_group.as_deref() == Some(&group.id) || boxes.flashing(&group.id);
        let target = drop_target.as_deref() == Some(&group.id);
        let style = if selected || target { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { fg(color).add_modifier(Modifier::DIM) };
        let members = member_counts.get(group.id.as_str()).copied().unwrap_or(0);
        let icon = group_icon(group);
        let chips = chip_models(app, group, rect, &format!(" {icon} {} · {members} ", group.name), scale);
        for chip in &chips {
            // Clickable where the chip covers whole cells.
            let (sx, sy) = model.to_screen(chip.rect.x, chip.rect.y);
            let (ex, ey) = model.to_screen(chip.rect.right(), chip.rect.bottom());
            let cell = Rect::new(sx.round().max(0.0) as u16, sy.round().max(0.0) as u16, (ex.round() - sx.round()).max(0.0) as u16, (ey - sy).round().max(1.0) as u16);
            let inside = cell.x >= area.x && cell.right() <= area.right() && cell.y >= area.y && cell.y < area.bottom();
            if cell.width > 0 && inside {
                chip_hits.push((cell, crate::dialogs::Hit::Toolbar(chip.action.clone())));
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
    let edge = |model: &mut CanvasModel, a: WRect, b: WRect, subsession: bool| {
        let bounds = WRect::new(a.x.min(b.x), a.y.min(b.y), a.right().max(b.right()) - a.x.min(b.x), a.bottom().max(b.bottom()) - a.y.min(b.y));
        if shows(bounds) {
            model.edges.push(EdgeModel {
                kind: if subsession { EdgeKind::Subsession } else { EdgeKind::Subagent },
                color: if subsession { ACCENT } else { MUTED },
                from: a,
                to: b,
                path: connector_path_f(a, b),
            });
        }
    };
    for node in &app.doc.nodes {
        let Some(parent) = node.parent.as_deref().and_then(|p| by_id.get(p).copied()) else { continue };
        if frame.skip(&node.id) || frame.skip(&parent.id) {
            continue;
        }
        edge(&mut model, node_rect(parent), node_rect(node), node.kind == NodeKind::Session);
    }
    for deck in &frame.decks {
        if let Some(root) = app.doc.node(&deck.root) {
            edge(&mut model, node_rect(root), deck_rect(deck), !deck.agents_only());
        }
    }

    let active_term = boxes.active_term.clone().filter(|id| !frame.skip(id));
    let typing = app.focus == crate::app::Focus::Terminal;
    let mut order: Vec<&Node> = app.doc.nodes.iter().filter(|n| !frame.skip(&n.id)).collect();
    order.sort_by_key(|n| app.selection.contains(&n.id));
    for node in order {
        let rect = node_rect(node);
        if !shows(rect) {
            continue;
        }
        let status = app.node_status(node);
        let selected = app.selection.contains(&node.id) || boxes.flashing(&node.id);
        let mut b = box_model(app, node, rect, status, selected, &boxes);
        b.hidden = hidden_badge(&frame, &node.id);
        if let Some((_, alpha)) = frame.moving.get(&node.id) {
            b.alpha = alpha.clamp(0.0, 1.0);
        }
        if b.alpha >= 1.0 {
            // A looping box's arc (growing in after the loop started), or the
            // arc of a loop that just stopped, fading out.
            let halo = active_term.as_deref() == Some(node.id.as_str());
            match (b.every.is_some(), app.loop_change(&node.id)) {
                (true, change) => {
                    let changed_ms = change.filter(|(_, ended)| !ended).map(|(at, _)| at);
                    model.loops.push(LoopModel { id: node.id.clone(), rect, halo, changed_ms, ending: false, seed: app.loop_seed(&node.id) });
                }
                (false, Some((at, true))) => model.loops.push(LoopModel { id: node.id.clone(), rect, halo, changed_ms: Some(at), ending: true, seed: app.loop_seed(&node.id) }),
                (false, _) => {}
            }
        }
        model.boxes.push(b);
    }
    // The decks go above the other boxes and under the selected ones.
    let decks: Vec<BoxModel> = frame
        .decks
        .iter()
        .filter_map(|deck| {
            let rect = deck_rect(deck);
            shows(WRect::new(rect.x, rect.y, rect.w + 2.0, rect.h + 2.0)).then(|| deck_model(app, deck, rect, &boxes)).flatten()
        })
        .collect();
    let at = model.boxes.iter().position(|b| app.selection.contains(&b.id)).unwrap_or(model.boxes.len());
    model.boxes.splice(at..at, decks);

    for (from, to, start) in &app.pulses {
        let (Some(a), Some(b)) = (app.doc.node(from), app.doc.node(to)) else { continue };
        if from != to && !frame.skip(from) && !frame.skip(to) {
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
    model.effects = app.effect_models(&frame, &node_rect);
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
        let chips_w: i32 = group.chips.iter().map(|c| paint::text_width(&c.label) as i32 + 1).sum();
        put(buf, x + 2, y, &truncate(&title, (w - 4 - chips_w).max(0) as usize), Style::default().fg(group.color).add_modifier(Modifier::BOLD), clip);
        if zoom == 0 {
            for (fx, fy, family) in &group.families {
                let (sx, sy) = at(*fx, *fy);
                if sy > y && sy < y + h - 1 {
                    put(buf, sx, sy, &truncate(family, (x + w - 2 - sx).max(0) as usize), fg(MUTED).add_modifier(Modifier::ITALIC), clip);
                }
            }
        }
        for chip in &group.chips {
            let style = if chip.lit { Style::default().fg(group.color).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
            let (cx, _) = at(chip.rect.x, chip.rect.y);
            put(buf, cx, y, &chip.label, style, clip);
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
        paint_box(buf, b, (x, y, w, h), zoom, clip);
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
    // The loops that run (a fading one is the GPU's): the dot where the clock puts it.
    let clock = model.anim_clock();
    for lp in model.loops.iter().filter(|l| !l.ending) {
        let (x, y, w, h) = screen_box(lp.rect);
        let lap = (model.motion == crate::view::Motion::On).then(|| lp.lap_at(clock));
        draw_loop(buf, (x, y, w + (zoom < 2 && lp.halo) as i32, h), lap, clip);
    }
    for effect in &model.effects {
        paint_effect(buf, effect, screen_box(effect.rect), clip);
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
