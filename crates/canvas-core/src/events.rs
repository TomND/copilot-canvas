//! Keyboard and mouse handling for the main (non-modal) screen.

use std::time::{Duration, Instant};

use crate::app::{App, CanvasHit, Drag, Focus, SideKind};
use crate::dialogs::{Action, Hit, Target};
use crate::doc::NodeKind;
use crate::input::{self, InputEvent, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::paint::rect_contains;
use crate::protocol::{ClientMsg, Input};
use crate::view::RenderMode;

impl App {
    /// The single input dispatch for every front end.
    pub fn on_input(&mut self, batch: Vec<InputEvent>) {
        for event in batch {
            self.needs_draw = true;
            match event {
                InputEvent::Paste(text) => self.on_paste(text),
                InputEvent::Text(text) => self.on_text(&text),
                InputEvent::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
                InputEvent::Key(_) => {}
                InputEvent::Mouse(mouse) => self.on_mouse(mouse),
                // The front end resizes its surface; the next draw lays out anew.
                InputEvent::Resize(_, _) => {}
                InputEvent::Focus(focused) => {
                    if self.focus == Focus::Terminal
                        && let Some(id) = self.focused_term.clone()
                        && self.owned_alive(&id)
                    {
                        self.send(ClientMsg::Input { id, input: Input::Focus(focused) });
                    }
                }
            }
        }
    }

    fn on_paste(&mut self, text: String) {
        if !self.modals.is_empty() {
            self.modal_paste(&text);
        } else if self.focus == Focus::Terminal {
            if let Some(id) = self.focused_term.clone().filter(|id| self.owned_alive(id)) {
                if text.is_empty() {
                    // The outer terminal pasted nothing (the clipboard holds an
                    // image): let the agent read the clipboard itself.
                    self.send_ctrl_v(&id);
                } else {
                    self.paste_text(&id, &text);
                }
            }
        } else if self.side_filter_active {
            self.side_filter.insert(&text);
        }
    }

    /// Committed text (IME): typed into a terminal, inserted into text fields.
    /// Text with line breaks reaches a terminal as a paste, so they do not
    /// act as Enter.
    fn on_text(&mut self, text: &str) {
        if self.modals.is_empty()
            && self.focus == Focus::Terminal
            && text.contains(['\r', '\n'])
            && let Some(id) = self.focused_term.clone().filter(|id| self.owned_alive(id))
        {
            self.term_select = None;
            return self.paste_text(&id, text);
        }
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        if !self.modals.is_empty() {
            self.modal_paste(&text);
        } else if self.focus == Focus::Terminal {
            if let Some(id) = self.focused_term.clone().filter(|id| self.owned_alive(id)) {
                self.term_select = None;
                for c in text.chars() {
                    self.send(ClientMsg::Input { id: id.clone(), input: Input::Key { key: crate::protocol::Key::Char(c), mods: 0 } });
                }
            }
        } else if self.side_filter_active {
            self.side_filter.insert(&text);
            self.side_sel = 0;
            self.side_scroll = 0;
        } else if let [c] = text.chars().collect::<Vec<_>>()[..] {
            // One character on the canvas or sidebar is a key press (shortcuts).
            self.on_key(KeyEvent::new(KeyCode::Char(c), Modifiers::NONE));
        }
    }

    fn cycle_focus(&mut self) {
        let next = match self.focus {
            Focus::Sidebar => Focus::Canvas,
            Focus::Canvas if !self.layout.tiles.is_empty() => Focus::Terminal,
            Focus::Canvas | Focus::Terminal if self.doc.ui.sidebar_visible => Focus::Sidebar,
            _ => Focus::Canvas,
        };
        if next == Focus::Terminal {
            self.focused_term = self.focused_term.clone().or_else(|| self.layout.tiles.first().map(|t| t.0.clone()));
        }
        self.set_focus(next);
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if !self.modals.is_empty() {
            self.modal_key(key);
            return;
        }
        if self.focus == Focus::Terminal {
            self.terminal_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let shift = key.modifiers.contains(Modifiers::SHIFT);
        if key.modifiers.contains(Modifiers::ALT)
            && let KeyCode::Char(c @ '1'..='8') = key.code
        {
            let view = crate::side::VIEWS[(c as u8 - b'1') as usize].0;
            return self.set_view(view);
        }
        if self.focus == Focus::Sidebar && self.side_view != crate::side::SideView::Sessions {
            match key.code {
                KeyCode::Up => return self.list_move(-1),
                KeyCode::Down => return self.list_move(1),
                KeyCode::PageUp => return self.list_move(-10),
                KeyCode::PageDown => return self.list_move(10),
                KeyCode::Home => return self.list_move(-100_000),
                KeyCode::End => return self.list_move(100_000),
                KeyCode::Enter => return self.list_activate(self.list_sel),
                KeyCode::Char(' ') => return self.list_toggle(self.list_sel),
                KeyCode::Delete => return self.list_delete(self.list_sel),
                KeyCode::Char('a' | 'A')
                    if !ctrl
                        && matches!(
                            self.side_view,
                            crate::side::SideView::Plugins | crate::side::SideView::Mcp | crate::side::SideView::Skills | crate::side::SideView::Agents | crate::side::SideView::Settings
                        ) =>
                {
                    return self.cycle_config_agent();
                }
                KeyCode::Esc => {
                    if self.settings_group.take().is_none() {
                        self.right_detail = None;
                        self.set_focus(Focus::Canvas);
                    }
                    return;
                }
                KeyCode::Tab => return self.set_focus(Focus::Canvas),
                _ => {}
            }
        }
        // Ctrl+Q always quits outside terminals, even while typing a filter.
        if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
            return self.request_quit();
        }
        if self.side_filter_active {
            match key.code {
                KeyCode::Esc => {
                    self.side_filter_active = false;
                    self.side_filter.set("");
                }
                KeyCode::Enter => {
                    self.side_filter_active = false;
                    self.activate_side_row(self.side_sel);
                }
                KeyCode::Up | KeyCode::Down => self.side_move(if key.code == KeyCode::Up { -1 } else { 1 }),
                _ => {
                    if self.side_filter.handle(&key) {
                        self.side_sel = 0;
                        self.side_scroll = 0;
                    }
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') if ctrl || key.modifiers.is_empty() => return self.request_quit(),
            KeyCode::F(6) => return self.cycle_focus(),
            _ if input::is_leave_terminal(&key) => {
                if !self.layout.tiles.is_empty() {
                    self.focused_term = self.focused_term.clone().or_else(|| self.layout.tiles.first().map(|t| t.0.clone()));
                    self.set_focus(Focus::Terminal);
                }
                return;
            }
            KeyCode::Char('?') => return self.run(Action::Help),
            KeyCode::Char('/') => {
                self.doc.ui.sidebar_visible = true;
                self.set_focus(Focus::Sidebar);
                self.side_filter_active = true;
                return;
            }
            KeyCode::Char('b') if !ctrl => {
                self.doc.ui.sidebar_visible = !self.doc.ui.sidebar_visible;
                if !self.doc.ui.sidebar_visible && self.focus == Focus::Sidebar {
                    self.set_focus(Focus::Canvas);
                }
                return self.mark_dirty();
            }
            KeyCode::Char('n') if !ctrl => return self.run(Action::NewSession { at: None, parent: None }),
            KeyCode::Char('s') | KeyCode::Char('S') if !ctrl => {
                let parent = self.primary().and_then(|n| self.session_of(&n.id));
                return match parent {
                    Some(parent) => self.run(Action::NewSession { at: None, parent: Some(parent) }),
                    None => self.notify("Select a session first to create a subsession.", false),
                };
            }
            KeyCode::Char('i') if !ctrl => return self.run(Action::ImportCopilot),
            KeyCode::Char('v') if !ctrl => return self.run(Action::SyncVs),
            KeyCode::Char('z') if ctrl => return self.run(Action::UndoSync),
            KeyCode::Char('l') if ctrl => {
                self.repaint_requested = true;
                self.needs_draw = true;
                return;
            }
            KeyCode::Char('t') if !ctrl => return self.run(Action::ToggleTiled),
            KeyCode::Char('o') if !ctrl => return self.run(Action::OpenOtherClient),
            KeyCode::Char('f') if !ctrl => return self.run(Action::FitAll),
            KeyCode::Char('g') if !ctrl => return self.run(Action::GroupSelection),
            KeyCode::Char('r') | KeyCode::F(2) if !ctrl => {
                if let Some(group) = self.selected_group.clone() {
                    return self.run(Action::Rename(Target::Group(group)));
                }
                if let Some(node) = self.primary() {
                    let id = node.id.clone();
                    return self.run(Action::Rename(Target::Node(id)));
                }
                return;
            }
            KeyCode::Char('x') if !ctrl => {
                if let Some(id) = self.primary().and_then(|n| self.session_of(&n.id)).filter(|id| self.owned_alive(id)) {
                    self.confirm_stop(id);
                }
                return;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let c = self.layout.canvas;
                return self.zoom_at((c.x + c.width / 2) as i32, (c.y + c.height / 2) as i32, -1);
            }
            KeyCode::Char('-') => {
                let c = self.layout.canvas;
                return self.zoom_at((c.x + c.width / 2) as i32, (c.y + c.height / 2) as i32, 1);
            }
            KeyCode::Delete => {
                if let Some(group) = self.selected_group.clone() {
                    return self.run(Action::DeleteGroup(group));
                }
                let ids = self.selection.clone();
                for id in ids {
                    self.run(Action::RemoveNode(id));
                }
                return;
            }
            KeyCode::Esc => {
                self.selection.clear();
                self.selected_group = None;
                return;
            }
            _ => {}
        }
        if self.focus == Focus::Sidebar {
            match key.code {
                KeyCode::Up => self.side_move(-1),
                KeyCode::Down => self.side_move(1),
                KeyCode::PageUp => self.side_move(-10),
                KeyCode::PageDown => self.side_move(10),
                KeyCode::Home => self.side_move(-100_000),
                KeyCode::End => self.side_move(100_000),
                KeyCode::Enter => self.activate_side_row(self.side_sel),
                KeyCode::Left | KeyCode::Right => {
                    if let Some(SideKind::Group(id)) = self.side_rows.get(self.side_sel).map(|r| r.kind.clone()) {
                        let collapsed = self.doc.ui.collapsed.contains(&id);
                        if (key.code == KeyCode::Left) != collapsed {
                            self.toggle_collapse(&id);
                        }
                    }
                }
                KeyCode::Tab => self.set_focus(Focus::Canvas),
                _ => {}
            }
            return;
        }
        // Canvas.
        let step = if shift { 5 } else { 1 };
        let (n, d) = self.scale();
        let world_step = step * d / n;
        match key.code {
            KeyCode::Enter => {
                if let Some(group) = self.selected_group.clone() {
                    self.open_group(&group);
                } else if let Some(id) = self.selection.last().cloned() {
                    self.open_node(&id);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let order: Vec<String> = self
                    .side_rows
                    .iter()
                    .filter_map(|r| match &r.kind {
                        SideKind::Session(id) => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                if order.is_empty() {
                    return;
                }
                let current = self.selection.last().and_then(|id| order.iter().position(|o| o == id));
                let forward = key.code == KeyCode::Tab && !shift;
                let next = match current {
                    Some(i) if forward => (i + 1) % order.len(),
                    Some(i) => (i + order.len() - 1) % order.len(),
                    None => 0,
                };
                let id = order[next].clone();
                self.select_node(&id, false);
                self.reveal_node(&id);
            }
            KeyCode::Home | KeyCode::Char('c') => {
                if let Some(id) = self.selection.last().cloned() {
                    self.reveal_node(&id);
                } else if let Some(group) = self.selected_group.clone() {
                    self.reveal_group(&group);
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                let (dx, dy) = match key.code {
                    KeyCode::Up => (0, -world_step),
                    KeyCode::Down => (0, world_step),
                    KeyCode::Left => (-world_step * 2, 0),
                    _ => (world_step * 2, 0),
                };
                if ctrl {
                    self.doc.view.x += dx * 2;
                    self.doc.view.y += dy * 2;
                } else if let Some(group) = self.selected_group.clone() {
                    self.doc.move_group(&group, dx, dy);
                } else if !self.selection.is_empty() {
                    let ids = self.selection.clone();
                    self.doc.move_nodes(&ids, dx, dy);
                } else {
                    self.doc.view.x += dx;
                    self.doc.view.y += dy;
                }
                self.mark_dirty();
            }
            _ => {}
        }
    }

    fn terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::F(6) {
            return self.cycle_focus();
        }
        if input::is_leave_terminal(&key) {
            return self.set_focus(Focus::Canvas);
        }
        let Some(id) = self.focused_term.clone() else {
            return self.set_focus(Focus::Canvas);
        };
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let is = |c: char| matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c));
        // Like Windows Terminal: Ctrl+C copies while text is selected; otherwise it reaches the agent.
        if ctrl && is('c') && self.term_select.as_ref().is_some_and(|s| s.id == id) {
            self.copy_selection();
            self.term_select = None;
            return;
        }
        if ctrl && key.modifiers.contains(Modifiers::SHIFT) && is('c') {
            return;
        }
        if ctrl && is('v') {
            return self.paste_clipboard(&id);
        }
        self.term_select = None;
        if !self.owned_alive(&id) {
            // Keys on an exited terminal: Enter restarts it, anything else returns to the canvas.
            if key.code == KeyCode::Enter {
                self.open_node(&id);
            } else {
                self.set_focus(Focus::Canvas);
            }
            return;
        }
        if key.modifiers.contains(Modifiers::SHIFT) && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let rows = self.views.get(&id).map(|v| v.rows as i32).unwrap_or(20).max(4) - 2;
            let delta = if key.code == KeyCode::PageUp { rows } else { -rows };
            self.send(ClientMsg::Scroll { id, delta });
            return;
        }
        if let Some((wire, mods)) = input::to_wire(&key) {
            // Alt+V pastes an image in agents: a remote one needs this machine's image first.
            if mods & crate::protocol::MOD_ALT != 0 && mods & crate::protocol::MOD_CTRL == 0 && matches!(wire, crate::protocol::Key::Char('v' | 'V')) {
                return self.paste_key(&id, Input::Key { key: wire, mods });
            }
            self.send(ClientMsg::Input { id, input: Input::Key { key: wire, mods } });
        }
    }

    fn side_move(&mut self, delta: i64) {
        if self.side_rows.is_empty() {
            return;
        }
        let max = self.side_rows.len() as i64 - 1;
        self.side_sel = (self.side_sel as i64 + delta).clamp(0, max) as usize;
        if matches!(self.side_rows[self.side_sel].kind, SideKind::Section) {
            let direction = if delta < 0 { -1 } else { 1 };
            self.side_sel = (self.side_sel as i64 + direction).clamp(0, max) as usize;
        }
    }

    pub fn toggle_collapse(&mut self, id: &str) {
        if let Some(pos) = self.doc.ui.collapsed.iter().position(|c| c == id) {
            self.doc.ui.collapsed.remove(pos);
        } else {
            self.doc.ui.collapsed.push(id.to_owned());
        }
        self.mark_dirty();
    }

    pub fn activate_side_row(&mut self, index: usize) {
        let Some(row) = self.side_rows.get(index).cloned() else { return };
        self.side_sel = index;
        match row.kind {
            SideKind::Group(id) => self.open_group(&id),
            SideKind::Session(id) | SideKind::Agent(id) => {
                self.reveal_node(&id);
                self.open_node(&id);
            }
            SideKind::Section => {}
        }
    }

    fn is_double_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let double = self.last_click.is_some_and(|(t, cx, cy)| now - t < Duration::from_millis(400) && cx == x && cy == y);
        self.last_click = if double { None } else { Some((now, x, y)) };
        double
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        self.pointer = (mouse.x, mouse.y);
        if !self.modals.is_empty() {
            self.modal_mouse(mouse);
            return;
        }
        let (x, y) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(button) => {
                self.drag_origin = self.pointer;
                self.mouse_down(button, x, y, mouse.modifiers)
            }
            MouseEventKind::Drag(_) => self.mouse_drag(x, y),
            MouseEventKind::Up(_) => self.mouse_up(x, y),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                self.mouse_scroll(mouse.kind, x, y, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                // Some consoles report drags as moves; keep active drags responsive.
                if self.drag.is_some() {
                    self.mouse_drag(x, y);
                } else {
                    self.needs_draw = false;
                }
            }
        }
    }

    fn mouse_down(&mut self, button: MouseButton, x: u16, y: u16, modifiers: Modifiers) {
        let additive = modifiers.intersects(Modifiers::CONTROL | Modifiers::SHIFT);
        if button == MouseButton::Left
            && let Some(link) = self.layout.links.iter().find(|l| l.contains(x, y)).cloned()
            && (!link.terminal || modifiers.contains(Modifiers::CONTROL))
        {
            self.open_link(&link.target);
            return;
        }
        let hit = self.layout.hits.iter().find(|(r, _)| rect_contains(*r, x, y)).map(|(_, h)| h.clone());
        if button == MouseButton::Left {
            match hit {
                Some(Hit::Toolbar(action)) => return self.run(action),
                Some(Hit::TileClose(id)) => return self.run(Action::CloseTile(id)),
                Some(Hit::TileHeader(id)) => {
                    self.focused_term = Some(id.clone());
                    if let Some(node) = self.doc.node(&id).map(|n| n.id.clone()) {
                        self.select_node(&node, false);
                    }
                    return self.set_focus(Focus::Terminal);
                }
                _ => {}
            }
            if Some(x) == self.layout.sidebar_divider && y >= self.layout.sidebar.y {
                self.drag = Some(Drag::SidebarDivider);
                return;
            }
            if Some(x) == self.layout.term_divider && y >= self.layout.terms.y {
                self.drag = Some(Drag::TermDivider);
                return;
            }
        }
        if rect_contains(self.layout.sidebar, x, y) {
            return self.sidebar_click(button, x, y);
        }
        if let Some((id, _, inner)) = self.layout.tiles.iter().find(|(_, outer, _)| rect_contains(*outer, x, y)).cloned() {
            self.focused_term = Some(id.clone());
            self.mark_reviewed(&id);
            if button == MouseButton::Right {
                return self.open_menu(x, y, self.tile_menu(&id));
            }
            if button == MouseButton::Left && rect_contains(inner, x, y) {
                self.term_select = None;
                self.drag = Some(Drag::Select { id: id.clone(), anchor: (x - inner.x, y - inner.y), moved: false });
            }
            if self.doc.node(&id).is_some() && !self.selection.contains(&id) {
                self.select_node(&id, false);
            }
            return self.set_focus(Focus::Terminal);
        }
        if !rect_contains(self.layout.canvas, x, y) {
            return;
        }
        self.set_focus(Focus::Canvas);
        let double = button == MouseButton::Left && self.is_double_click(x, y);
        match (button, self.canvas_hit(x, y)) {
            (MouseButton::Left, CanvasHit::Node(id)) => {
                if additive {
                    self.select_node(&id, true);
                } else if !self.selection.contains(&id) {
                    self.select_node(&id, false);
                }
                let ids: Vec<String> = self.selection.clone();
                self.drag = Some(Drag::Nodes { ids, start: (x, y), applied: (0, 0), moved: false, click: (!additive).then_some(id) });
            }
            (MouseButton::Left, CanvasHit::GroupResize(id)) => {
                let orig = self.doc.group(&id).map(|g| (g.w, g.h)).unwrap_or((20, 6));
                self.select_group(&id);
                self.drag = Some(Drag::Resize { id, start: (x, y), orig });
            }
            (MouseButton::Left, CanvasHit::GroupTitle(id)) => {
                if double {
                    return self.run(Action::Rename(Target::Group(id)));
                }
                self.select_group(&id);
                self.drag = Some(Drag::Group { id, start: (x, y), applied: (0, 0), moved: false });
            }
            (MouseButton::Left, _) if double => {
                let (wx, wy) = self.to_world(x as i32, y as i32);
                self.run(Action::NewSession { at: Some((wx, wy)), parent: None });
            }
            (MouseButton::Left, _) if additive => {
                self.drag = Some(Drag::Rubber { start: (x, y), now: (x, y), additive: true, right: false });
            }
            (MouseButton::Left | MouseButton::Middle, _) => self.drag = Some(Drag::Pan { last: (x, y), moved: false }),
            (MouseButton::Right, CanvasHit::Node(id)) => {
                if !self.selection.contains(&id) {
                    self.select_node(&id, false);
                }
                let menu = self.node_menu(&id);
                self.open_menu(x, y, menu);
            }
            (MouseButton::Right, CanvasHit::GroupTitle(id) | CanvasHit::GroupResize(id)) => {
                self.select_group(&id);
                let menu = self.group_menu(&id);
                self.open_menu(x, y, menu);
            }
            (MouseButton::Right, _) => {
                self.drag = Some(Drag::Rubber { start: (x, y), now: (x, y), additive, right: true });
            }
        }
    }

    fn sidebar_click(&mut self, button: MouseButton, x: u16, y: u16) {
        self.set_focus(Focus::Sidebar);
        if self.side_view != crate::side::SideView::Sessions {
            if let Some(index) = self.layout.sidebar_rows.iter().find(|(row_y, _)| *row_y == y).map(|(_, i)| *i) {
                if button == MouseButton::Right {
                    self.list_sel = index;
                    return self.list_toggle(index);
                }
                self.list_activate(index);
            }
            return;
        }
        if Some(y) == self.layout.sidebar_filter_row {
            self.side_filter_active = true;
            return;
        }
        let Some(index) = self.layout.sidebar_rows.iter().find(|(row_y, _)| *row_y == y).map(|(_, i)| *i) else { return };
        let Some(row) = self.side_rows.get(index).cloned() else { return };
        self.side_sel = index;
        if button == MouseButton::Right {
            let menu = match &row.kind {
                SideKind::Group(id) => self.group_menu(id),
                SideKind::Session(id) | SideKind::Agent(id) => {
                    self.select_node(id, false);
                    self.node_menu(id)
                }
                SideKind::Section => return,
            };
            return self.open_menu(x, y, menu);
        }
        let caret_x = self.layout.sidebar.x + 1 + row.depth * 2;
        if let SideKind::Group(id) = &row.kind
            && x <= caret_x + 1
        {
            return self.toggle_collapse(id);
        }
        self.activate_side_row(index);
    }

    fn mouse_drag(&mut self, x: u16, y: u16) {
        if self.render_mode == RenderMode::Gpu && self.precise_drag(x, y) {
            return;
        }
        let (n, d) = self.scale();
        let Some(drag) = self.drag.take() else { return };
        let world = |dx: i32, dy: i32| (dx * d / n, dy * d / n);
        self.drag = Some(match drag {
            Drag::Nodes { ids, start, applied, moved, click } => {
                let (wx, wy) = world(x as i32 - start.0 as i32, y as i32 - start.1 as i32);
                let (ddx, ddy) = (wx - applied.0, wy - applied.1);
                if ddx != 0 || ddy != 0 {
                    self.doc.move_nodes(&ids, ddx, ddy);
                    self.mark_dirty();
                }
                Drag::Nodes { moved: moved || wx != 0 || wy != 0, ids, start, applied: (wx, wy), click }
            }
            Drag::Group { id, start, applied, moved } => {
                let (wx, wy) = world(x as i32 - start.0 as i32, y as i32 - start.1 as i32);
                let (ddx, ddy) = (wx - applied.0, wy - applied.1);
                if ddx != 0 || ddy != 0 {
                    self.doc.move_group(&id, ddx, ddy);
                    self.mark_dirty();
                }
                Drag::Group { moved: moved || wx != 0 || wy != 0, id, start, applied: (wx, wy) }
            }
            Drag::Resize { id, start, orig } => {
                let (wx, wy) = world(x as i32 - start.0 as i32, y as i32 - start.1 as i32);
                if let Some(group) = self.doc.group_mut(&id) {
                    group.w = (orig.0 + wx).max(crate::doc::GROUP_MIN_W);
                    group.h = (orig.1 + wy).max(crate::doc::GROUP_MIN_H);
                }
                self.mark_dirty();
                Drag::Resize { id, start, orig }
            }
            Drag::Pan { last, moved } => {
                let (wx, wy) = world(x as i32 - last.0 as i32, y as i32 - last.1 as i32);
                self.doc.view.x -= wx;
                self.doc.view.y -= wy;
                if wx != 0 || wy != 0 {
                    self.mark_dirty();
                }
                let consumed = (wx * n / d, wy * n / d);
                let next = ((last.0 as i32 + consumed.0) as u16, (last.1 as i32 + consumed.1) as u16);
                Drag::Pan { last: next, moved: moved || last != (x, y) }
            }
            Drag::Rubber { start, additive, right, .. } => Drag::Rubber { start, now: (x, y), additive, right },
            Drag::SidebarDivider => {
                self.doc.ui.sidebar_width = x.saturating_sub(self.layout.sidebar.x).clamp(18, 70);
                self.mark_dirty();
                Drag::SidebarDivider
            }
            Drag::Select { id, anchor, moved } => {
                match self.layout.tiles.iter().find(|(t, _, _)| *t == id).map(|(_, _, inner)| *inner) {
                    Some(inner) if inner.width > 0 && inner.height > 0 => {
                        let head = (x.clamp(inner.x, inner.right() - 1) - inner.x, y.clamp(inner.y, inner.bottom() - 1) - inner.y);
                        let moved = moved || head != anchor;
                        if moved {
                            self.term_select = Some(crate::app::TermSelection { id: id.clone(), anchor, head });
                        }
                        Drag::Select { id, anchor, moved }
                    }
                    _ => Drag::Select { id, anchor, moved },
                }
            }
            Drag::TermDivider => {
                let left = self.layout.canvas.x as i32;
                let right = self.layout.terms.right() as i32;
                let width = (right - left).max(1);
                let percent = ((right - x as i32) * 100 / width).clamp(15, 85);
                self.doc.ui.terminal_percent = percent as u16;
                self.mark_dirty();
                Drag::TermDivider
            }
        });
    }

    /// Gpu mode: canvas drags follow the precise pointer (boxes and groups move
    /// in whole world units; the rest shows as a visual offset). False for
    /// drags that work in cells in both modes.
    fn precise_drag(&mut self, x: u16, y: u16) -> bool {
        let s = self.scale_f();
        let (dx, dy) = ((self.pointer.0 - self.drag_origin.0) / s, (self.pointer.1 - self.drag_origin.1) / s);
        let (rx, ry) = (dx.round() as i32, dy.round() as i32);
        let next = match self.drag.take() {
            Some(Drag::Nodes { ids, start, applied, moved, click }) => {
                if (rx, ry) != applied {
                    self.doc.move_nodes(&ids, rx - applied.0, ry - applied.1);
                }
                self.mark_dirty();
                Drag::Nodes { moved: moved || (rx, ry) != (0, 0), ids, start, applied: (rx, ry), click }
            }
            Some(Drag::Group { id, start, applied, moved }) => {
                if (rx, ry) != applied {
                    self.doc.move_group(&id, rx - applied.0, ry - applied.1);
                }
                self.mark_dirty();
                Drag::Group { moved: moved || (rx, ry) != (0, 0), id, start, applied: (rx, ry) }
            }
            Some(Drag::Resize { id, start, orig }) => {
                if let Some(group) = self.doc.group_mut(&id) {
                    group.w = (orig.0 + rx).max(crate::doc::GROUP_MIN_W);
                    group.h = (orig.1 + ry).max(crate::doc::GROUP_MIN_H);
                }
                self.mark_dirty();
                Drag::Resize { id, start, orig }
            }
            Some(Drag::Pan { last, moved }) => {
                self.pan_cells(self.pointer.0 - self.drag_origin.0, self.pointer.1 - self.drag_origin.1);
                self.drag_origin = self.pointer;
                Drag::Pan { last: (x, y), moved: moved || last != (x, y) }
            }
            other => {
                self.drag = other;
                return false;
            }
        };
        self.drag = Some(next);
        true
    }

    fn mouse_up(&mut self, x: u16, y: u16) {
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Select { moved: true, .. } => self.copy_selection(),
            Drag::Select { moved: false, id, anchor } => {
                self.term_select = None;
                // Plain clicks reach apps that asked for mouse reports (Copilot's
                // clickable UI); drags stay a canvas text selection.
                self.send(ClientMsg::Click { id, col: anchor.0, row: anchor.1, button: 0, mods: 0 });
            }
            Drag::Nodes { moved: false, click: Some(id), .. } => self.open_node(&id),
            Drag::Group { moved: false, id, .. } => self.open_group(&id),
            Drag::Pan { moved: false, .. } => {
                self.selection.clear();
                self.selected_group = None;
            }
            Drag::Rubber { start, now, additive, right } => {
                let moved = start.0.abs_diff(now.0) > 1 || start.1.abs_diff(now.1) > 0;
                if !moved {
                    if right {
                        let (wx, wy) = self.to_world(x as i32, y as i32);
                        let menu = self.canvas_menu(wx, wy);
                        self.open_menu(x, y, menu);
                    }
                    return;
                }
                let area = if self.render_mode == RenderMode::Gpu {
                    // The band as drawn, from the precise pointer positions.
                    let (o, p) = (self.drag_origin, self.pointer);
                    let (ax, ay) = self.to_world_f(o.0.min(p.0), o.1.min(p.1));
                    let (bx, by) = self.to_world_f(o.0.max(p.0), o.1.max(p.1));
                    let (x0, y0) = (ax.floor() as i32, ay.floor() as i32);
                    crate::doc::Rect { x: x0, y: y0, w: (bx.ceil() as i32 - x0).max(1), h: (by.ceil() as i32 - y0).max(1) }
                } else {
                    let (ax, ay) = self.to_world(start.0.min(now.0) as i32, start.1.min(now.1) as i32);
                    let (bx, by) = self.to_world(start.0.max(now.0) as i32 + 1, start.1.max(now.1) as i32 + 1);
                    crate::doc::Rect { x: ax, y: ay, w: (bx - ax).max(1), h: (by - ay).max(1) }
                };
                let hits: Vec<String> = self
                    .doc
                    .nodes
                    .iter()
                    .filter(|n| n.kind == NodeKind::Session && n.rect().intersects(&area))
                    .map(|n| n.id.clone())
                    .collect();
                if !additive {
                    self.selection.clear();
                }
                for id in hits {
                    if !self.selection.contains(&id) {
                        self.selection.push(id);
                    }
                }
                self.selected_group = None;
                if self.selection.len() > 1 {
                    self.notify(format!("{} sessions selected — press G to group them.", self.selection.len()), false);
                }
            }
            Drag::SidebarDivider | Drag::TermDivider => self.last_sizes.clear(),
            _ => {}
        }
        self.mark_dirty();
    }

    fn mouse_scroll(&mut self, kind: MouseEventKind, x: u16, y: u16, modifiers: Modifiers) {
        let up = matches!(kind, MouseEventKind::ScrollUp);
        let horizontal = matches!(kind, MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight) || modifiers.contains(Modifiers::SHIFT);
        if rect_contains(self.layout.sidebar, x, y) {
            if self.side_view == crate::side::SideView::Sessions {
                self.side_scroll = if up { self.side_scroll.saturating_sub(3) } else { self.side_scroll + 3 };
            } else {
                self.list_move(if up { -3 } else { 3 });
            }
            return;
        }
        if let Some((id, _, inner)) = self.layout.tiles.iter().find(|(_, outer, _)| rect_contains(*outer, x, y)).cloned() {
            if self.terms.contains_key(&id) {
                let col = x.saturating_sub(inner.x).min(inner.width.saturating_sub(1));
                let row = y.saturating_sub(inner.y).min(inner.height.saturating_sub(1));
                self.send(ClientMsg::Wheel { id, up, col, row, mods: input::mods(modifiers) });
            }
            return;
        }
        if !rect_contains(self.layout.canvas, x, y) {
            return;
        }
        // Windows Terminal may keep Ctrl+wheel for font zoom, so Alt+wheel also zooms.
        if modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT) {
            if self.render_mode == RenderMode::Gpu {
                // Continuous: one notch is 1/8 of a power of two, around the pointer.
                let (px, py) = self.point_in(x, y);
                return self.zoom_by(px, py, 2f32.powf(if up { 0.125 } else { -0.125 }));
            }
            return self.zoom_at(x as i32, y as i32, if up { -1 } else { 1 });
        }
        let forward = matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft);
        if self.render_mode == RenderMode::Gpu {
            self.pan_quiet_until = Some(Instant::now() + Duration::from_millis(200));
            let step = if forward { 3.0 } else { -3.0 };
            return if horizontal { self.pan_cells(step * 2.0, 0.0) } else { self.pan_cells(0.0, step) };
        }
        let (n, d) = self.scale();
        let amount = 3 * d / n;
        self.pan_quiet_until = Some(Instant::now() + Duration::from_millis(200));
        if horizontal {
            self.doc.view.x += if forward { -amount * 2 } else { amount * 2 };
        } else {
            self.doc.view.y += if forward { -amount } else { amount };
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod paste_tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use crate::app::{App, Focus, TermSelection, TermView};
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::input::InputEvent;
    use crate::protocol::{ClientMsg, Frame8, Input, Key, MOD_CTRL, MODE_ALT_SCREEN, Run8, TermInfo};
    use crate::sync::Opened;

    /// A clipboard of the test's own.
    struct Clip(Mutex<String>);

    impl crate::platform::Platform for Clip {
        fn set_clipboard_text(&self, text: &str) -> anyhow::Result<()> {
            *self.0.lock().unwrap() = text.to_owned();
            Ok(())
        }
        fn clipboard_text(&self) -> anyhow::Result<String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn clipboard_has_text(&self) -> bool {
            !self.0.lock().unwrap().is_empty()
        }
        fn clipboard_has_image(&self) -> bool {
            false
        }
    }

    /// An app whose keyboard is in live terminal "t", with `clip` as its clipboard.
    fn app(clip: &str) -> (App, Receiver<ClientMsg>, Arc<Clip>) {
        let (tx, rx) = bounded(64);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        let clip = Arc::new(Clip(Mutex::new(clip.to_owned())));
        app.platform = clip.clone();
        app.terms.insert("t".into(), TermInfo { id: "t".into(), alive: true, ..Default::default() });
        app.focused_term = Some("t".into());
        app.focus = Focus::Terminal;
        (app, rx, clip)
    }

    fn inputs(rx: &Receiver<ClientMsg>) -> Vec<Input> {
        rx.try_iter()
            .filter_map(|m| match m {
                ClientMsg::Input { input, .. } | ClientMsg::Input8 { input, .. } => Some(input),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pastes_reach_the_terminal_as_one_paste_without_a_trailing_line_break() {
        let (mut app, rx, _) = app("line one\r\n\r\n  indented\r\n");
        app.on_input(vec![InputEvent::Key(crate::input::KeyEvent::new(crate::input::KeyCode::Char('v'), crate::input::Modifiers::CONTROL))]);
        assert_eq!(inputs(&rx), [Input::Paste("line one\r\n\r\n  indented".into())], "Ctrl+V: the clipboard, without its final line break");
        app.on_input(vec![InputEvent::Paste("a\nb\n\n".into())]);
        assert_eq!(inputs(&rx), [Input::Paste("a\nb".into())], "a console paste");
        app.on_input(vec![InputEvent::Paste("\r\n".into())]);
        assert!(inputs(&rx).is_empty(), "line breaks alone paste nothing (no Enter)");
        app.on_input(vec![InputEvent::Paste(String::new())]);
        assert_eq!(inputs(&rx), [Input::Key { key: Key::Char('v'), mods: MOD_CTRL }], "an empty paste lets the agent read an image");
    }

    #[test]
    fn ime_text_with_line_breaks_is_a_paste_and_without_is_typed() {
        let (mut app, rx, _) = app("");
        app.on_input(vec![InputEvent::Text("ab".into())]);
        assert_eq!(inputs(&rx), [Input::Key { key: Key::Char('a'), mods: 0 }, Input::Key { key: Key::Char('b'), mods: 0 }]);
        app.on_input(vec![InputEvent::Text("first\nsecond\n".into())]);
        assert_eq!(inputs(&rx), [Input::Paste("first\nsecond".into())]);
    }

    #[test]
    fn copying_from_copilot_leaves_its_scrollbar_out() {
        let (mut app, _rx, clip) = app("");
        let run = |col: u16, text: &str| Run8 { col, text: text.into(), ..Default::default() };
        let mut view = TermView::default();
        view.apply8(Frame8 {
            id: "t".into(),
            cols: 12,
            rows: 2,
            full: true,
            changed: vec![(0, vec![run(0, " ● answer"), run(11, "┃")]), (1, vec![run(0, "   more")])],
            cursor: None,
            cursor_shape: 0,
            cursor_blink: false,
            display_offset: 0,
            history: 0,
            modes: MODE_ALT_SCREEN,
            links: Vec::new(),
        });
        app.views.insert("t".into(), view);
        app.term_select = Some(TermSelection { id: "t".into(), anchor: (0, 0), head: (11, 1) });
        app.copy_selection();
        assert_eq!(clip.0.lock().unwrap().as_str(), " ● answer  ┃\r\n   more", "another program keeps it");
        app.terms.get_mut("t").unwrap().copilot = true;
        app.doc.nodes.push(serde_json::from_value(serde_json::json!({ "id": "t", "kind": "session", "name": "t", "x": 0, "y": 0, "agent": "copilot" })).unwrap());
        app.copy_selection();
        assert_eq!(clip.0.lock().unwrap().as_str(), " ● answer\r\n   more");
    }
}