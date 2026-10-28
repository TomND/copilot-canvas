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
        self.items.push(Item { id, role, name, description: None, value: None, rect, children: Vec::new(), click: None, selected: None });
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
    b.items.push(Item { id: ROOT, role: Role::Window, name: title.to_owned(), description: None, value: None, rect: root_rect, children: Vec::new(), click: None, selected: None });
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
        let name = text_in(chrome, *r);
        let i = b.add(toolbar_id, &format!("toolbar/{name}/{}", r.x), Role::Button, name, b.cells(*r));
        b.clickable(i, *r);
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
            let name = canvas_core::side::VIEWS.iter().find(|(view, _, _)| view == v).map(|(_, _, t)| t.to_string()).unwrap_or_default();
            let i = b.add(tabs_id, &format!("activity/{name}"), Role::Tab, name, b.cells(*r));
            b.items[i].selected = Some(app.side_view == *v && app.doc.ui.sidebar_visible);
            b.clickable(i, *r);
        }
    }

    // Sidebar: the session tree, or the lines of another view.
    if layout.sidebar.width > 0 {
        let sessions = app.side_view == canvas_core::side::SideView::Sessions;
        let title = canvas_core::side::VIEWS.iter().find(|(v, _, _)| *v == app.side_view).map(|(_, _, t)| t.to_string()).unwrap_or_default();
        let side = b.add(ROOT, "sidebar", if sessions { Role::Tree } else { Role::List }, title, b.cells(layout.sidebar));
        let side_id = b.items[side].id;
        if sessions {
            for &(y, index) in &layout.sidebar_rows {
                let Some(row) = app.side_rows.get(index) else { continue };
                let key = match &row.kind {
                    SideKind::Group(id) | SideKind::Session(id) | SideKind::Agent(id) => format!("side/{id}"),
                    SideKind::Section => format!("side/section/{}", row.label),
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
            b.items[i].description = Some(description);
            b.items[i].selected = Some(bx.selected);
            let s = model.rect_to_screen(bx.rect);
            b.items[i].click = Some((s.x + (s.w / 2.0).min(3.0), s.y + s.h / 2.0));
        }
    }

    // Terminal tiles: title and visible text.
    for tile in &view.tiles {
        let i = b.add(ROOT, &format!("tile/{}", tile.id), Role::Terminal, tile.title.clone(), b.cells(tile.outer));
        if let Some(term) = app.views.get(&tile.id) {
            let text: Vec<String> = (0..term.lines.len()).map(|row| term.row_cells(row).concat().trim_end().to_owned()).collect();
            let end = text.iter().rposition(|l| !l.is_empty()).map_or(0, |p| p + 1);
            b.items[i].value = Some(text[..end].join("\n"));
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

/// The nodes as an AccessKit update (a full tree).
pub fn update(items: &[Item], focus: u64) -> TreeUpdate {
    let nodes = items
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
            node.set_children(item.children.iter().map(|c| NodeId(*c)).collect::<Vec<_>>());
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
    let mut tree = TreeInfo::new(NodeId(ROOT));
    tree.toolkit_name = Some("Copilot Canvas".into());
    tree.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
    TreeUpdate { nodes, tree: Some(tree), tree_id: TreeId::ROOT, focus: NodeId(focus) }
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
