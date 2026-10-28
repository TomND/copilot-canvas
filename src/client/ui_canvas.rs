//! Canvas rendering: groups, connectors, session and subagent boxes.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::app::{App, Drag};
use super::doc::{Node, NodeKind};
use super::paint::{
    self, ACCENT, DASHED, FAINT, GROUP_COLORS, HEAVY, LIGHT, MUTED, PANEL, ROUND, SELECT, TEXT, fill, fg, outline, put, put_sym,
    truncate,
};

pub fn draw_canvas(app: &mut App, buf: &mut Buffer, area: Rect) {
    let clip = area;
    // Dot grid anchored to world coordinates so it moves with panning. It is
    // hidden while panning: shifting it rewrites nearly every cell each frame.
    let (n, d) = app.scale();
    let step_x = (6 * n / d).max(3);
    let step_y = (3 * n / d).max(2);
    let (ox, oy) = app.to_screen(0, 0);
    let panning = matches!(app.drag, Some(Drag::Pan { moved: true, .. })) || app.pan_quiet_until.is_some();
    for y in area.y..area.bottom() {
        if panning {
            break;
        }
        if (y as i32 - oy).rem_euclid(step_y) != 0 {
            continue;
        }
        for x in area.x..area.right() {
            if (x as i32 - ox).rem_euclid(step_x) == 0 {
                put_sym(buf, x as i32, y as i32, "·", fg(FAINT), clip);
            }
        }
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
    let mut chip_hits: Vec<(ratatui::layout::Rect, super::dialogs::Hit)> = Vec::new();
    let mut groups: Vec<&super::doc::Group> = app.doc.groups.iter().collect();
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
            let chips = [(" ◧ AI docs ", false), (" ◨ Outputs ", true)];
            let total: i32 = chips.iter().map(|(t, _)| super::paint::text_width(t) as i32 + 1).sum();
            let mut cx = x + w - 2 - total;
            if cx > title_end {
                let style = if group.folder.is_some() { Style::default().fg(color).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
                for (text, outputs) in chips {
                    let end = put(buf, cx, y, text, style, clip);
                    if cx >= clip.x as i32 && end <= clip.right() as i32 && y >= clip.y as i32 && y < clip.bottom() as i32 {
                        chip_hits.push((
                            ratatui::layout::Rect::new(cx as u16, y as u16, (end - cx) as u16, 1),
                            super::dialogs::Hit::Toolbar(super::dialogs::Action::OpenGroupFolder(group.id.clone(), outputs)),
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
    let typing = app.focus == super::app::Focus::Terminal;
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
        let attention = if node.kind == NodeKind::Agent {
            None
        } else if node.kind == NodeKind::Automation && status == super::status::Status::Failed {
            // A failed automation pulses until its next good run.
            Some((if (tick / 8).is_multiple_of(2) { super::paint::NEGATIVE } else { super::paint::NEGATIVE_DIM }, super::paint::NEGATIVE_BG))
        } else {
            status.attention()
        };
        let border = if let Some((color, _)) = attention {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(SELECT).add_modifier(Modifier::BOLD)
        } else if focused_term {
            fg(ACCENT)
        } else if node.kind == NodeKind::Agent {
            fg(MUTED)
        } else {
            fg(TEXT).add_modifier(Modifier::DIM)
        };
        let icon = status.icon(tick);
        let status_style = fg(status.color());
        let name_style = if selected { Style::default().fg(SELECT).add_modifier(Modifier::BOLD) } else { fg(TEXT) };
        match zoom {
            0 => {
                let body = attention.map(|(_, bg)| bg).unwrap_or(PANEL);
                fill(buf, x + 1, y + 1, w - 2, h - 2, Style::default().bg(body), clip);
                let set = if selected || status == super::status::Status::Waiting && attention.is_some() {
                    &HEAVY
                } else if attention.is_some() {
                    &super::paint::DOUBLE
                } else if node.passive.is_some() {
                    &DASHED
                } else {
                    match node.kind {
                        NodeKind::Agent => &LIGHT,
                        NodeKind::Automation => &super::paint::DOUBLE,
                        NodeKind::Session => &ROUND,
                    }
                };
                outline(buf, x, y, w, h, set, border, clip);
                let prefix = match (node.kind, node.parent.is_some()) {
                    _ if node.passive.is_some() => "⟲ ",
                    (NodeKind::Agent, _) => "◇ ",
                    (NodeKind::Automation, _) => "◷ ",
                    (NodeKind::Session, true) => "↳ ",
                    _ => "",
                };
                let title = format!(" {prefix}{} ", node.name);
                put(buf, x + 1, y, &truncate(&title, (w - 2) as usize), name_style, clip);
                let label = match node.kind {
                    NodeKind::Automation => automation_label(status),
                    NodeKind::Agent if status == super::status::Status::JustFinished => "just finished",
                    _ => status.label(),
                };
                let line = match app.node_loop(node) {
                    _ if node.passive.is_some() => format!("{icon} {}", app.passive_line(node)),
                    Some(every) => format!("{icon} {label} · ↻ {every}"),
                    None => format!("{icon} {label}"),
                };
                let label_style = if attention.is_some() { status_style.add_modifier(Modifier::BOLD) } else { status_style };
                let end = put(buf, x + 2, y + 1, &truncate(&line, (w - 4) as usize), label_style.bg(body), clip);
                if app.live_elsewhere(node) && status != super::status::Status::External {
                    put(buf, end, y + 1, &truncate(" · ↗ elsewhere", (x + w - 2 - end).max(0) as usize), fg(super::paint::PURPLE).bg(body), clip);
                }
                if h >= 4 {
                    let subtitle = match &node.passive {
                        Some(passive) => super::app::App::passive_trigger(passive),
                        None => app.subtitle(node),
                    };
                    put(buf, x + 2, y + 2, &truncate(&subtitle, (w - 4) as usize), fg(MUTED).bg(body), clip);
                }
            }
            1 => {
                let bg = attention.map(|(_, bg)| bg).unwrap_or(PANEL);
                fill(buf, x, y, w, h, Style::default().bg(bg), clip);
                let mut end = put(buf, x, y, icon, status_style.bg(bg), clip);
                end = put(buf, end + 1, y, &truncate(&node.name, (w - 2).max(0) as usize), name_style.bg(bg), clip);
                let _ = end;
                if h >= 2 {
                    let label_style = attention.map(|(c, _)| super::paint::bold(c)).unwrap_or(fg(MUTED));
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
                    super::paint::bold(ACCENT).add_modifier(Modifier::UNDERLINED)
                } else {
                    attention.map(|(c, bg)| super::paint::bold(c).bg(bg)).unwrap_or(name_style)
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
                let badge = format!(" ✉ {count} ");
                put(buf, x + w - 2 - super::paint::text_width(&badge) as i32, y, &badge, super::paint::bold(super::paint::PANEL).bg(super::paint::ATTENTION), clip);
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
        if path.len() < 2 {
            continue;
        }
        let t = (start.elapsed().as_millis() as f32 / 1_200.0).min(1.0);
        let at = ((path.len() - 1) as f32 * t) as usize;
        for (offset, glyph, style) in [(2usize, "·", fg(super::paint::LOOP)), (1, "•", fg(super::paint::LOOP)), (0, "●", super::paint::bold(super::paint::LOOP_LIT))] {
            if let Some(&(px, py)) = at.checked_sub(offset).and_then(|i| path.get(i)) {
                put(buf, px, py, glyph, style, clip);
            }
        }
    }

    // Looping boxes: a dotted loopback on the right side with one lit dot
    // running around it (with a short trail), like a loading bar.
    let phase = tick / 2;
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
        let path = loop_path(x, y, w + halo as i32, h);
        if path.len() == 1 {
            // One-line boxes (zoomed out): the ↻ icon pulses.
            let (px, py, glyph) = path[0];
            let color = if phase.is_multiple_of(2) { super::paint::LOOP_LIT } else { super::paint::LOOP };
            put(buf, px, py, glyph, super::paint::bold(color), clip);
            continue;
        }
        let lit = phase % path.len();
        let trail = (lit + path.len() - 1) % path.len();
        for (i, &(px, py, glyph)) in path.iter().enumerate() {
            let (glyph, style) = if i == lit {
                ("●", super::paint::bold(super::paint::LOOP_LIT))
            } else if i == trail {
                ("•", fg(super::paint::LOOP))
            } else {
                (glyph, fg(super::paint::LOOP))
            };
            put(buf, px, py, glyph, style, clip);
        }
    }

    // Halo one cell outside the active terminal's box (drawn last so neighbours
    // cannot cover it): thin while its terminal is showing, heavy while typing.
    if zoom < 2
        && let Some(node) = active_term.as_deref().and_then(|id| app.doc.node(id))
    {
        let (x, y, w, h) = app.node_screen(node);
        let set = if typing { &HEAVY } else { &LIGHT };
        outline(buf, x - 1, y - 1, w + 2, h + 2, set, super::paint::bold(ACCENT), clip);
    }

    if let Some(Drag::Rubber { start, now, .. }) = &app.drag {
        let (x0, y0) = (start.0.min(now.0) as i32, start.1.min(now.1) as i32);
        let (x1, y1) = (start.0.max(now.0) as i32, start.1.max(now.1) as i32);
        outline(buf, x0, y0, (x1 - x0 + 1).max(2), (y1 - y0 + 1).max(2), &DASHED, fg(SELECT), clip);
    }

    if app.doc.nodes.is_empty() && app.doc.groups.is_empty() {
        let lines = [
            "Your canvas is empty.",
            "",
            "N  new session          I  import Copilot sessions",
            "V  import VS Code agents   ?  help",
            "Double-click anywhere to create a session there.",
        ];
        let top = area.y as i32 + area.height as i32 / 2 - 3;
        for (i, line) in lines.iter().enumerate() {
            let x = area.x as i32 + (area.width as i32 - paint::text_width(line) as i32) / 2;
            put(buf, x, top + i as i32, line, fg(if i == 0 { TEXT } else { MUTED }), clip);
        }
    }
}

fn automation_label(status: super::status::Status) -> &'static str {
    use super::status::Status;
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
fn connector_path(parent: BoxRect, child: BoxRect) -> Vec<(i32, i32)> {
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
