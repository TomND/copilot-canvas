//! The UI Automation tree (through AccessKit): the window's toolbar buttons,
//! activity bar, sidebar rows, canvas groups and boxes (name, status), the
//! terminal tiles (title, visible text), the status bar and the open dialog
//! or menu, built from the same `View`, canvas model and layout the renderer
//! draws. The `tree` test hook returns the same nodes as JSON.

use std::hash::{Hash, Hasher};

use accesskit::{Action, Node, NodeId, Rect as AkRect, Role, Toggled, TreeId, TreeInfo, TreeUpdate};
use canvas_core::app::{App, Focus, SideKind};
use canvas_core::dialogs::{Action as UiAction, Hit, Modal};
use canvas_core::view::View;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};

use crate::text::Grid;

pub const ROOT: u64 = 1;

/// One node before it becomes an AccessKit node (or JSON for the hooks).
#[derive(Clone, Debug)]
pub struct Item {
    pub id: u64,
    pub role: Role,
    pub name: String,
    pub description: Option<String>,
    pub value: Option<String>,
    /// Physical px: x, y, w, h.
    pub rect: [f64; 4],
    pub children: Vec<u64>,
    /// Click (and default action) at this cell (fractional cells).
    pub click: Option<(f32, f32)>,
    pub selected: Option<bool>,
    /// The first text row (px: x, y, w, h) of a node whose value is rows of
    /// text (a terminal tile), for the text runs AT-SPI reads.
    pub line: Option<[f64; 4]>,
}

fn id_of(key: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    // 0 and the root's id are never produced.
    h.finish() | 2
}

/// The text of a buffer's cells in a rectangle, rows joined by spaces.
fn text_in(buf: &Buffer, r: Rect) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in r.y..r.bottom().min(area.bottom()) {
        let row: String = (r.x..r.right().min(area.right())).filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned())).collect();
        let row = row.trim();
        if !row.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(row);
        }
    }
    out
}

struct Builder<'a> {
    items: Vec<Item>,
    index: std::collections::HashMap<u64, usize>,
    grid: &'a Grid,
}

impl Builder<'_> {
    fn cells(&self, r: Rect) -> [f64; 4] {
        let (cw, ch) = (self.grid.cell_w as f64, self.grid.cell_h as f64);
        [r.x as f64 * cw, r.y as f64 * ch, r.width as f64 * cw, r.height as f64 * ch]
    }

    fn add(&mut self, parent: u64, key: &str, role: Role, name: String, rect: [f64; 4]) -> usize {
        let mut id = id_of(key);
        let mut n = 0;
        while self.index.contains_key(&id) {
            n += 1;
            id = id_of(&format!("{key}#{n}"));
        }
        if let Some(&p) = self.index.get(&parent) {
            self.items[p].children.push(id);
        }
        self.index.insert(id, self.items.len());
        self.items.push(Item { id, role, name, description: None, value: None, rect, children: Vec::new(), click: None, selected: None, line: None });
        self.items.len() - 1
    }

    fn clickable(&mut self, index: usize, r: Rect) {
        self.items[index].click = Some((r.x as f32 + (r.width as f32 / 2.0).min(3.0), r.y as f32 + r.height as f32 / 2.0));
    }
}

/// Builds the nodes; returns them and the focused node.
pub fn items(app: &App, view: &View, chrome: &Buffer, overlay: &Buffer, grid: &Grid, title: &str) -> (Vec<Item>, u64) {
    let screen = chrome.area;
    let mut b = Builder { items: Vec::new(), index: Default::default(), grid };
    let root_rect = b.cells(screen);
    b.items.push(Item { id: ROOT, role: Role::Window, name: title.to_owned(), description: None, value: None, rect: root_rect, children: Vec::new(), click: None, selected: None, line: None });
    b.index.insert(ROOT, 0);
    let layout = &app.layout;

    // Toolbar: its buttons, then the status counts.
    let toolbar = b.add(ROOT, "toolbar", Role::Toolbar, "Toolbar".into(), b.cells(layout.toolbar));
    let toolbar_id = b.items[toolbar].id;
    let mut last_button = 0;
    for (r, hit) in &layout.hits {
        if r.y != layout.toolbar.y || !matches!(hit, Hit::Toolbar(_)) {
            continue;
        }
        let (name, role) = match hit {
            // A space tab is named by its space (compact tabs show only a glyph).
            Hit::Toolbar(UiAction::Space(space)) => (space.name().to_owned(), Role::Tab),
            _ => (text_in(chrome, *r), Role::Button),
        };
        let i = b.add(toolbar_id, &format!("toolbar/{name}/{}", r.x), role, name, b.cells(*r));
        b.clickable(i, *r);
        if let Hit::Toolbar(UiAction::Space(space)) = hit {
            b.items[i].selected = Some(*space == app.space);
        }
        if !matches!(hit, Hit::Toolbar(UiAction::ShowAutomations)) {
            last_button = last_button.max(r.right());
        }
    }
    let counts = Rect::new(last_button, layout.toolbar.y, screen.width.saturating_sub(last_button), 1);
    let counts_text = text_in(chrome, counts);
    if !counts_text.is_empty() {
        b.add(toolbar_id, "toolbar/counts", Role::Status, counts_text, b.cells(counts));
    }

    // Activity bar.
    let bar = Rect::new(0, layout.toolbar.bottom(), canvas_core::ui_side::BAR_W, layout.status.y.saturating_sub(layout.toolbar.bottom()));
    let tabs = b.add(ROOT, "activity", Role::TabList, "Activity bar".into(), b.cells(bar));
    let tabs_id = b.items[tabs].id;
    for (r, hit) in &layout.hits {
        if let Hit::Toolbar(UiAction::View(v)) = hit {
            // Slot 1 is the space's own sidebar.
            let name = match v {
                canvas_core::side::SideView::Sessions => app.space.side().1.to_owned(),
                _ => canvas_core::side::VIEWS.iter().find(|(view, _, _)| view == v).map(|(_, _, t)| t.to_string()).unwrap_or_default(),
            };
            let i = b.add(tabs_id, &format!("activity/{name}"), Role::Tab, name, b.cells(*r));
            b.items[i].selected = Some(app.side_view == *v && app.doc.ui.sidebar_visible);
            b.clickable(i, *r);
        }
    }

    // Sidebar: the session tree, or the lines of another view (or of the space's own sidebar).
    if layout.sidebar.width > 0 {
        let slot1 = app.side_view == canvas_core::side::SideView::Sessions;
        let sessions = slot1 && app.space == canvas_core::spaces::Space::Canvas;
        let title = if slot1 { app.space.side().1.to_owned() } else { canvas_core::side::VIEWS.iter().find(|(v, _, _)| *v == app.side_view).map(|(_, _, t)| t.to_string()).unwrap_or_default() };
        let side = b.add(ROOT, "sidebar", if sessions { Role::Tree } else { Role::List }, title, b.cells(layout.sidebar));
        let side_id = b.items[side].id;
        if sessions {
            for &(y, index) in &layout.sidebar_rows {
                let Some(row) = app.side_rows.get(index) else { continue };
                let key = match &row.kind {
                    SideKind::Group(id) | SideKind::Session(id) | SideKind::Agent(id) => format!("side/{id}"),
                    SideKind::Section => format!("side/section/{}", row.label),
                    SideKind::Folder(id) => format!("side/folder/{id}"),
                    SideKind::Passives(id) => format!("side/passives/{id}"),
                    SideKind::RecentHeader => "side/recent".to_owned(),
                    SideKind::Recent(id) => format!("side/recent/{id}"),
                };
                let r = Rect::new(layout.sidebar.x, y, layout.sidebar.width, 1);
                let i = b.add(side_id, &key, Role::TreeItem, row.label.clone(), b.cells(r));
                b.items[i].description = row.status.map(|s| s.label().to_owned()).or(row.count.map(|c| format!("{c} sessions")));
                b.items[i].selected = Some(index == app.side_sel && app.focus == Focus::Sidebar);
                b.clickable(i, r);
            }
        } else {
            for y in layout.sidebar.y..layout.sidebar.bottom() {
                let r = Rect::new(layout.sidebar.x, y, layout.sidebar.width, 1);
                let text = text_in(chrome, r);
                if !text.is_empty() {
                    let i = b.add(side_id, &format!("side/line/{y}/{text}"), Role::ListItem, text, b.cells(r));
                    b.clickable(i, r);
                }
            }
        }
    }

    // Canvas: groups and boxes.
    if let Some(model) = &view.canvas {
        let canvas = b.add(ROOT, "canvas", Role::Group, "Canvas".into(), b.cells(model.area));
        let canvas_id = b.items[canvas].id;
        let px = |r: canvas_core::view::WRect| {
            let s = model.rect_to_screen(r);
            [s.x as f64 * grid.cell_w as f64, s.y as f64 * grid.cell_h as f64, s.w as f64 * grid.cell_w as f64, s.h as f64 * grid.cell_h as f64]
        };
        for g in &model.groups {
            let i = b.add(canvas_id, &format!("group/{}", g.id), Role::Group, g.name.clone(), px(g.rect));
            b.items[i].description = Some(format!("group · {} {}", g.members, if g.automations { "automations" } else { "sessions" }));
            b.items[i].selected = Some(g.selected);
        }
        for bx in &model.boxes {
            let i = b.add(canvas_id, &format!("box/{}", bx.id), Role::Button, format!("{}{}", bx.prefix.trim(), bx.name).trim().to_owned(), px(bx.rect));
            b.items[i].description = Some(box_description(bx));
            b.items[i].selected = Some(bx.selected);
            let s = model.rect_to_screen(bx.rect);
            b.items[i].click = Some((s.x + (s.w / 2.0).min(3.0), s.y + s.h / 2.0));
        }
    } else if app.space != canvas_core::spaces::Space::Canvas {
        // The Notebook's or Tasks' main area: its lines (it is drawn from cells).
        let area = layout.canvas;
        let main = b.add(ROOT, "space", Role::Group, app.space.name().to_owned(), b.cells(area));
        let main_id = b.items[main].id;
        for y in area.y..area.bottom() {
            let r = Rect::new(area.x, y, area.width, 1);
            let text = text_in(chrome, r);
            if !text.is_empty() {
                let i = b.add(main_id, &format!("space/line/{y}/{text}"), Role::ListItem, text, b.cells(r));
                b.clickable(i, r);
            }
        }
    }

    // Terminal tiles: title and visible text.
    for tile in &view.tiles {
        let i = b.add(ROOT, &format!("tile/{}", tile.id), Role::Terminal, tile.title.clone(), b.cells(tile.outer));
        if let Some(term) = app.views.get(&tile.id) {
            let text: Vec<String> = (0..term.lines.len()).map(|row| term.row_cells(row).concat().trim_end().to_owned()).collect();
            let end = text.iter().rposition(|l| !l.is_empty()).map_or(0, |p| p + 1);
            b.items[i].value = Some(text[..end].join("\n"));
            b.items[i].line = Some(b.cells(Rect::new(tile.inner.x, tile.inner.y, tile.inner.width, 1)));
        }
        b.items[i].description = Some(if tile.alive { "terminal" } else { "terminal · exited" }.into());
        b.items[i].selected = Some(tile.focused);
        b.clickable(i, tile.inner);
    }

    // Status bar and the connection banner.
    let status = text_in(chrome, layout.status);
    if !status.is_empty() {
        b.add(ROOT, "statusbar", Role::Status, status, b.cells(layout.status));
    }
    if let Some(banner) = &view.banner {
        b.add(ROOT, "banner", Role::Alert, banner.clone(), b.cells(Rect::new(0, 1, screen.width, 1)));
    }

    // The top dialog or menu with its controls.
    let mut modal_id = None;
    if let Some(r) = view.modal {
        let menu = matches!(app.modals.last(), Some(Modal::Menu(_)));
        let name = (r.y..r.bottom()).map(|y| text_in(overlay, Rect::new(r.x, y, r.width, 1))).map(|t| t.trim_matches(|c: char| "─│╭╮╰╯┌┐└┘━┃┏┓┗┛ ".contains(c)).to_owned()).find(|t| !t.is_empty()).unwrap_or_default();
        let dialog = b.add(ROOT, &format!("modal/{}", app.modals.len()), if menu { Role::Menu } else { Role::Dialog }, name, b.cells(r));
        let dialog_id = b.items[dialog].id;
        modal_id = Some(dialog_id);
        for (hr, hit) in &layout.hits {
            let inside = hr.x >= r.x && hr.y >= r.y && hr.right() <= r.right() && hr.bottom() <= r.bottom();
            if !inside {
                continue;
            }
            let role = match hit {
                Hit::MenuItem(_) => Role::MenuItem,
                Hit::Button(_) | Hit::Chip(_) => Role::Button,
                Hit::ListRow(_) => Role::ListItem,
                Hit::Field(_) => Role::TextInput,
                Hit::Toggle(_) => Role::CheckBox,
                _ => continue,
            };
            let text = text_in(overlay, *hr);
            let i = b.add(dialog_id, &format!("modal/{hit:?}/{}/{}", hr.x, hr.y), role, text.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace()).to_owned(), b.cells(*hr));
            b.clickable(i, *hr);
        }
    }

    let focus = modal_id
        .or_else(|| match app.focus {
            Focus::Terminal => app.focused_term.as_ref().map(|t| id_of(&format!("tile/{t}"))),
            Focus::Canvas => app.selection.last().map(|s| id_of(&format!("box/{s}"))),
            Focus::Sidebar => b.items.iter().find(|i| i.role == Role::TreeItem && i.selected == Some(true)).map(|i| i.id),
        })
        .filter(|id| b.items.iter().any(|i| i.id == *id))
        .unwrap_or(ROOT);
    (b.items, focus)
}

/// A box's description: its status and line, where it runs, and the todo
/// its session claimed (the `☑ DEC-031` badge; a click on it opens the card).
fn box_description(bx: &canvas_core::view::BoxModel) -> String {
    let mut description = bx.status.label().to_owned();
    let line = bx.line.trim();
    if !line.is_empty() && !line.ends_with(bx.status.label()) {
        description = format!("{description} · {line}");
    } else if !line.is_empty() {
        description = line.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_owned();
    }
    if bx.elsewhere {
        description.push_str(" · open elsewhere");
    }
    if let Some(todo) = &bx.badge {
        description.push_str(&format!(" · working on todo {todo}"));
    }
    description
}

/// The nodes as an AccessKit update (a full tree).
pub fn update(items: &[Item], focus: u64) -> TreeUpdate {
    let mut runs = Vec::new();
    let mut nodes: Vec<(NodeId, Node)> = items
        .iter()
        .map(|item| {
            let mut node = Node::new(item.role);
            if !item.name.is_empty() {
                node.set_label(item.name.clone());
            }
            if let Some(d) = &item.description {
                // UIA's FullDescription.
                node.set_description(d.clone());
            }
            if let Some(v) = &item.value {
                node.set_value(v.clone());
            }
            let [x, y, w, h] = item.rect;
            node.set_bounds(AkRect { x0: x, y0: y, x1: x + w, y1: y + h });
            let mut children: Vec<NodeId> = item.children.iter().map(|c| NodeId(*c)).collect();
            if cfg!(target_os = "linux")
                && let (Some(text), Some(line)) = (&item.value, item.line)
            {
                let start = runs.len();
                text_runs(item.id, text, line, &mut runs);
                children.extend(runs[start..].iter().map(|(id, _)| *id));
            }
            node.set_children(children);
            if item.click.is_some() {
                node.add_action(Action::Click);
                node.add_action(Action::Focus);
            }
            if let Some(selected) = item.selected {
                if item.role == Role::CheckBox {
                    node.set_toggled(if selected { Toggled::True } else { Toggled::False });
                } else {
                    node.set_selected(selected);
                }
            }
            (NodeId(item.id), node)
        })
        .collect();
    nodes.append(&mut runs);
    let mut tree = TreeInfo::new(NodeId(ROOT));
    tree.toolkit_name = Some("Copilot Canvas".into());
    tree.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
    TreeUpdate { nodes, tree: Some(tree), tree_id: TreeId::ROOT, focus: NodeId(focus) }
}

/// Rows of text as AccessKit text runs, one per row (a line each, its line
/// break included). AT-SPI reads a node's text only through runs (its Value
/// interface is numeric), so on Linux a terminal tile's visible text is what
/// Orca reads and reviews this way; UIA reads the value (Windows keeps the
/// tree without runs). Runs are not nodes of the platform tree themselves.
fn text_runs(parent: u64, text: &str, line: [f64; 4], out: &mut Vec<(NodeId, Node)>) {
    if text.is_empty() {
        return;
    }
    let rows: Vec<&str> = text.split('\n').collect();
    let [x, y, w, h] = line;
    for (n, row) in rows.iter().enumerate() {
        let mut value = (*row).to_owned();
        if n + 1 < rows.len() {
            value.push('\n');
        }
        let mut node = Node::new(Role::TextRun);
        node.set_character_lengths(value.chars().map(|c| c.len_utf8() as u8).collect::<Vec<_>>());
        node.set_word_starts(word_starts(&value));
        let top = y + n as f64 * h;
        node.set_bounds(AkRect { x0: x, y0: top, x1: x + w, y1: top + h });
        node.set_value(value);
        out.push((NodeId(id_of(&format!("{parent}/run/{n}"))), node));
    }
}

/// Where a text run's words start (characters; a word keeps the spaces
/// after it, leading spaces are a word). AccessKit counts them in a byte, so
/// a row's words past its 255th character have no start of their own.
fn word_starts(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut after_space = false;
    for (i, c) in text.chars().enumerate().take(256) {
        let space = c.is_whitespace();
        if i == 0 || (after_space && !space) {
            out.push(i as u8);
        }
        after_space = space;
    }
    out
}

/// The nodes as JSON (the `tree` and `access` hooks), parents before children.
pub fn json(items: &[Item], focus: u64) -> Value {
    let nodes: Vec<Value> = items
        .iter()
        .map(|i| {
            json!({
                "id": i.id, "role": format!("{:?}", i.role), "name": i.name, "description": i.description,
                "value": i.value.as_ref().map(|v| v.chars().take(4000).collect::<String>()), "rect": i.rect,
                "children": i.children, "selected": i.selected, "focused": i.id == focus,
            })
        })
        .collect();
    json!({ "focus": focus, "nodes": nodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::view::{Border, BoxKind, BoxModel, WRect};
    use ratatui::style::{Color, Style};


    #[test]
    fn text_runs_are_rows_with_their_line_breaks() {
        let mut runs = Vec::new();
        text_runs(7, "PS> ls\n\nfoo  中文 bar", [10.0, 20.0, 300.0, 16.0], &mut runs);
        let values: Vec<&str> = runs.iter().map(|(_, n)| n.value().unwrap_or_default()).collect();
        assert_eq!(values, ["PS> ls\n", "\n", "foo  中文 bar"]);
        for (_, node) in &runs {
            let value = node.value().unwrap_or_default();
            assert_eq!(node.character_lengths().iter().map(|&l| l as usize).sum::<usize>(), value.len());
        }
        assert_eq!(runs[0].1.word_starts(), [0, 4]);
        assert_eq!(runs[2].1.word_starts(), [0, 5, 8]);
        assert_eq!(word_starts("  ab"), [0, 2]);
        assert_eq!(runs[1].1.bounds().map(|b| (b.y0, b.y1)), Some((36.0, 52.0)));
        let mut none = Vec::new();
        text_runs(7, "", [0.0; 4], &mut none);
        assert!(none.is_empty());
    }


    #[test]
    fn a_boxs_description_names_the_todo_its_session_claimed() {
        let mut bx = BoxModel {
            id: "s".into(),
            kind: BoxKind::Session,
            rect: WRect::new(0.0, 0.0, 30.0, 4.0),
            prefix: "",
            name: "Fix flaky test".into(),
            status: canvas_core::status::Status::Idle,
            icon: "",
            label: "",
            line: String::new(),
            line_style: Style::default(),
            name_style: Style::default(),
            elsewhere: false,
            subtitle: String::new(),
            border: Border::Round,
            border_style: Style::default(),
            fill: Color::Reset,
            attention: None,
            selected: false,
            focused_term: false,
            active: false,
            every: None,
            mailbox: 0,
            dust: 0,
            alpha: 1.0,
            deck: None,
            hidden: None,
            badge: None,
        };
        let idle = canvas_core::status::Status::Idle.label().to_owned();
        assert_eq!(super::box_description(&bx), idle);
        bx.badge = Some("DEC-031".into());
        bx.elsewhere = true;
        assert_eq!(super::box_description(&bx), format!("{idle} · open elsewhere · working on todo DEC-031"));
    }
}
