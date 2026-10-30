//! Keyboard and mouse handling for the main (non-modal) screen.

use std::time::{Duration, Instant};

use crate::app::{App, CanvasHit, CanvasPress, Drag, Focus, SideKind};
use crate::dialogs::{Action, Hit, Target};
use crate::doc::NodeKind;
use crate::input::{self, InputEvent, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::paint::rect_contains;
use crate::protocol::{ClientMsg, Input};
use crate::undo::Source;
use crate::view::RenderMode;

/// An arrow key that moves the selection (not Ctrl's pan): presses in a row
/// are one undo step.
fn is_nudge(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right) && !key.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT)
}

impl App {
    /// The single input dispatch for every front end.
    pub fn on_input(&mut self, batch: Vec<InputEvent>) {
        for event in batch {
            self.needs_draw = true;
            // Anything but the walk keys ends a walk through the recent sessions.
            self.recent_input(&event);
            match event {
                InputEvent::Paste(text) => self.on_paste(text),
                InputEvent::Text(text) => self.on_text(&text),
                InputEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    // Any other key ends a burst of arrow-key moves (one undo step).
                    if !is_nudge(&key) {
                        self.end_key_burst();
                    }
                    self.on_key(key)
                }
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
        // Ctrl+F6 / Ctrl+Shift+F6 (a terminal too), `]` / `[`: view the next
        // older or newer recent session (never starts it).
        if let Some(older) = self.recent_key(&key) {
            return self.recent_step(older);
        }
        if self.focus == Focus::Terminal {
            self.terminal_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let shift = key.modifiers.contains(Modifiers::SHIFT);
        if key.modifiers.contains(Modifiers::ALT)
            && let KeyCode::Char(c @ '1'..='9') = key.code
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
            // The canvas-wide agents' panel (the top bar's ◎ Agents).
            KeyCode::Char('a') if !ctrl && !self.demo => return self.run(Action::Agents(crate::canvas_wide::AgentAction::Panel)),
            // Undo and redo on the canvas and the sidebar (terminals and text
            // fields took their keys above; the console may report Ctrl+Z and
            // Ctrl+Y as their control characters).
            KeyCode::Char('z' | 'Z') if ctrl && shift => return self.run(Action::Redo),
            KeyCode::Char('z' | 'Z') if ctrl => return self.run(Action::Undo),
            KeyCode::Char('\x1a') => return self.run(Action::Undo),
            KeyCode::Char('y' | 'Y') if ctrl => return self.run(Action::Redo),
            KeyCode::Char('\x19') => return self.run(Action::Redo),
            KeyCode::Char('l') if ctrl => {
                self.repaint_requested = true;
                self.needs_draw = true;
                return;
            }
            KeyCode::Char('t') if !ctrl => return self.run(Action::ToggleTiled),
            KeyCode::Char('o') if !ctrl => return self.run(Action::OpenOtherClient),
            KeyCode::Char('f') if !ctrl => return self.run(Action::FitAll),
            KeyCode::Char('g') if !ctrl => return self.run(Action::GroupSelection),
            KeyCode::Char('k') if !ctrl => return self.run(Action::Visual(crate::visual::VisualAction::ToggleStack)),
            KeyCode::Char('h') if !ctrl => return self.run(Action::Visual(crate::visual::VisualAction::ToggleHideSubsessions)),
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
                // A passive session's headless run stops through the service (not a failure).
                if let Some(node) = self.primary().filter(|n| n.passive.is_some() && self.passive_view(n).is_some_and(|v| v.running)).cloned() {
                    self.stop_passive_run(&node.id);
                } else if let Some(id) = self.primary().and_then(|n| self.session_of(&n.id)).filter(|id| self.owned_alive(id)) {
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
                // Several boxes: their eligible part after a confirmation.
                match self.selection.as_slice() {
                    [] => {}
                    [id] => {
                        let id = id.clone();
                        self.run(Action::RemoveNode(id));
                    }
                    _ => self.confirm_remove(self.selection.clone()),
                }
                return;
            }
            KeyCode::Esc => {
                self.clear_selection();
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
                    let id = match self.side_rows.get(self.side_sel).map(|r| r.kind.clone()) {
                        Some(SideKind::Group(id)) => Some(id),
                        Some(SideKind::RecentHeader) => Some(crate::recent::COLLAPSED.to_owned()),
                        _ => None,
                    };
                    if let Some(id) = id {
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
                // The explicit keyboard start (a group shows its running terminals).
                if let Some(group) = self.selected_group.clone() {
                    self.open_group(&group);
                } else if let Some(id) = self.selection.last().cloned() {
                    self.start_node(&id);
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
                    // Presses in a row are one undo step (`undo::BURST`).
                    self.move_group_recorded(&group, dx, dy, Source::Keys(format!("group {group}")));
                } else if !self.selection.is_empty() {
                    let ids = self.selection.clone();
                    self.move_nodes_recorded(&ids, dx, dy, Source::Keys(ids.join("\n")));
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
            // Keys on an exited terminal: Enter restarts it (an explicit
            // start), anything else returns to the canvas.
            if key.code == KeyCode::Enter {
                self.start_node(&id);
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

    /// Enter (or a double-click) on a sidebar row: a group shows its running
    /// terminals; a session or subagent starts (or resumes) and gets the keyboard.
    pub fn activate_side_row(&mut self, index: usize) {
        let Some(row) = self.side_rows.get(index).cloned() else { return };
        self.side_sel = index;
        match row.kind {
            SideKind::Group(id) => self.open_group(&id),
            SideKind::Session(id) | SideKind::Agent(id) | SideKind::Recent(id) => {
                self.reveal_node(&id);
                self.start_node(&id);
            }
            SideKind::Folder(id) => self.side_folder(&id),
            SideKind::RecentHeader => self.toggle_collapse(crate::recent::COLLAPSED),
            SideKind::Section => {}
        }
    }

    /// A click on a sidebar row: flies the canvas to it and views it (never launches).
    pub fn view_side_row(&mut self, index: usize) {
        let Some(row) = self.side_rows.get(index).cloned() else { return };
        self.side_sel = index;
        match row.kind {
            SideKind::Group(id) => self.open_group(&id),
            SideKind::Session(id) | SideKind::Agent(id) => {
                self.reveal_node(&id);
                self.view_node(&id);
            }
            SideKind::Recent(id) => {
                self.reveal_node(&id);
                self.view_node(&id);
                // The highlight stays on the RECENT row (not the tree's row of the session).
                self.side_sel = index;
            }
            SideKind::Folder(id) => self.side_folder(&id),
            SideKind::RecentHeader => self.toggle_collapse(crate::recent::COLLAPSED),
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
        // A burst of arrow-key moves (or a drag whose button came up
        // elsewhere) is one undo step from here on.
        self.commit_layout();
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
        // Shift or Ctrl: an area selection wherever it starts (on a box, a
        // stack or a group too); it never drags, starts or double-clicks
        // anything. Released without moving, it toggles the box it was on.
        if button == MouseButton::Left && additive {
            self.last_click = None;
            let click = self.press_target(x, y);
            self.drag = Some(Drag::Rubber { start: (x, y), now: (x, y), additive: true, right: false, click });
            return;
        }
        let double = button == MouseButton::Left && self.is_double_click(x, y);
        // Stacks and effects first (a click on an effect never starts a session).
        if self.visual_mouse_down(button, x, y, double, additive) {
            return;
        }
        match (button, self.canvas_hit(x, y)) {
            (MouseButton::Left, CanvasHit::Node(id)) if double => {
                // The second click of a double-click: the explicit start.
                self.drag = None;
                self.start_node(&id);
            }
            (MouseButton::Left, CanvasHit::Node(id)) => {
                if !self.selection.contains(&id) {
                    self.select_node(&id, false);
                }
                let ids: Vec<String> = self.selection.clone();
                self.drag = Some(Drag::Nodes { ids, start: (x, y), applied: (0, 0), moved: false, click: Some(id) });
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
            // Empty space pans; a click on a group's background or border selects it.
            (MouseButton::Left | MouseButton::Middle, hit) => {
                let group = match (button, hit) {
                    (MouseButton::Left, CanvasHit::GroupBody(id)) => Some(id),
                    _ => None,
                };
                self.drag = Some(Drag::Pan { last: (x, y), moved: false, group });
            }
            // Right: a drag selects by area wherever it starts; released
            // where it was pressed, the menu of what is there.
            (MouseButton::Right, hit) => {
                let click = match hit {
                    CanvasHit::Node(id) => Some(CanvasPress::Node(id)),
                    CanvasHit::GroupTitle(id) | CanvasHit::GroupResize(id) => Some(CanvasPress::Group(id)),
                    CanvasHit::GroupBody(_) | CanvasHit::Empty => None,
                };
                self.drag = Some(Drag::Rubber { start: (x, y), now: (x, y), additive, right: true, click });
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
                SideKind::Session(id) | SideKind::Agent(id) | SideKind::Recent(id) => {
                    self.select_node(id, false);
                    self.node_menu(id)
                }
                SideKind::Folder(id) => return self.side_folder(id),
                SideKind::Section | SideKind::RecentHeader => return,
            };
            return self.open_menu(x, y, menu);
        }
        let caret_x = self.layout.sidebar.x + 1 + row.depth * 2;
        if let SideKind::Group(id) = &row.kind
            && x <= caret_x + 1
        {
            return self.toggle_collapse(id);
        }
        if row.kind == SideKind::RecentHeader {
            return self.toggle_collapse(crate::recent::COLLAPSED);
        }
        // The RECENT rows hold still meanwhile, so a double-click starts the row it began on.
        self.hold_recent();
        // A second click on the same row soon after the first starts it.
        let now = Instant::now();
        let double = self.last_side_click.is_some_and(|(t, i)| i == index && now - t < Duration::from_millis(400));
        self.last_side_click = if double { None } else { Some((now, index)) };
        if double {
            self.activate_side_row(index);
        } else {
            self.view_side_row(index);
        }
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
                    self.move_nodes_recorded(&ids, ddx, ddy, Source::Mouse);
                    self.mark_dirty();
                }
                Drag::Nodes { moved: moved || wx != 0 || wy != 0, ids, start, applied: (wx, wy), click }
            }
            Drag::Group { id, start, applied, moved } => {
                let (wx, wy) = world(x as i32 - start.0 as i32, y as i32 - start.1 as i32);
                let (ddx, ddy) = (wx - applied.0, wy - applied.1);
                if ddx != 0 || ddy != 0 {
                    self.move_group_recorded(&id, ddx, ddy, Source::Mouse);
                    self.mark_dirty();
                }
                Drag::Group { moved: moved || wx != 0 || wy != 0, id, start, applied: (wx, wy) }
            }
            Drag::Resize { id, start, orig } => {
                let (wx, wy) = world(x as i32 - start.0 as i32, y as i32 - start.1 as i32);
                self.resize_group_recorded(&id, orig.0 + wx, orig.1 + wy);
                self.mark_dirty();
                Drag::Resize { id, start, orig }
            }
            Drag::Pan { last, moved, group } => {
                let (wx, wy) = world(x as i32 - last.0 as i32, y as i32 - last.1 as i32);
                self.doc.view.x -= wx;
                self.doc.view.y -= wy;
                if wx != 0 || wy != 0 {
                    self.mark_dirty();
                }
                let consumed = (wx * n / d, wy * n / d);
                let next = ((last.0 as i32 + consumed.0) as u16, (last.1 as i32 + consumed.1) as u16);
                Drag::Pan { last: next, moved: moved || last != (x, y), group }
            }
            Drag::Rubber { start, additive, right, click, .. } => Drag::Rubber { start, now: (x, y), additive, right, click },
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
                    self.move_nodes_recorded(&ids, rx - applied.0, ry - applied.1, Source::Mouse);
                }
                self.mark_dirty();
                Drag::Nodes { moved: moved || (rx, ry) != (0, 0), ids, start, applied: (rx, ry), click }
            }
            Some(Drag::Group { id, start, applied, moved }) => {
                if (rx, ry) != applied {
                    self.move_group_recorded(&id, rx - applied.0, ry - applied.1, Source::Mouse);
                }
                self.mark_dirty();
                Drag::Group { moved: moved || (rx, ry) != (0, 0), id, start, applied: (rx, ry) }
            }
            Some(Drag::Resize { id, start, orig }) => {
                self.resize_group_recorded(&id, orig.0 + rx, orig.1 + ry);
                self.mark_dirty();
                Drag::Resize { id, start, orig }
            }
            Some(Drag::Pan { last, moved, group }) => {
                self.pan_cells(self.pointer.0 - self.drag_origin.0, self.pointer.1 - self.drag_origin.1);
                self.drag_origin = self.pointer;
                Drag::Pan { last: (x, y), moved: moved || last != (x, y), group }
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
        // A completed drag of boxes, a group or its ◢ corner: one undo step.
        let layout = matches!(drag, Drag::Nodes { .. } | Drag::Group { .. } | Drag::Resize { .. });
        match drag {
            Drag::Select { moved: true, .. } => self.copy_selection(),
            Drag::Select { moved: false, id, anchor } => {
                self.term_select = None;
                // Plain clicks reach apps that asked for mouse reports (Copilot's
                // clickable UI); drags stay a canvas text selection.
                self.send(ClientMsg::Click { id, col: anchor.0, row: anchor.1, button: 0, mods: 0 });
            }
            Drag::Nodes { moved: false, click: Some(id), .. } => self.view_node(&id),
            Drag::Group { moved: false, id, .. } => self.open_group(&id),
            Drag::Pan { moved: false, group: Some(group), .. } => self.select_group(&group),
            Drag::Pan { moved: false, group: None, .. } => self.clear_selection(),
            Drag::Rubber { start, now, additive, right, click } => {
                // The GUI measures the precise pointer (a jitter across a row
                // boundary is still a click); the TUI whole cells.
                let moved = match self.render_mode {
                    RenderMode::Gpu => (self.pointer.0 - self.drag_origin.0).abs() >= 1.0 || (self.pointer.1 - self.drag_origin.1).abs() >= 0.6,
                    RenderMode::Cells => start.0.abs_diff(now.0) > 1 || start.1.abs_diff(now.1) > 0,
                };
                if !moved {
                    return self.canvas_press_up(click, right, x, y);
                }
                let hits = self.area_hits(start, now);
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
                    let agents = self.selection.iter().filter(|id| self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Agent)).count();
                    let sessions = self.selection.len() - agents;
                    let text = if agents == 0 {
                        format!("{sessions} sessions selected — press G to group them.")
                    } else {
                        let (s, a) = (if sessions == 1 { "" } else { "s" }, if agents == 1 { "" } else { "s" });
                        format!("{} boxes selected ({sessions} session{s}, {agents} subagent{a}) — G groups them, Del removes them.", self.selection.len())
                    };
                    self.notify(text, false);
                }
            }
            Drag::SidebarDivider | Drag::TermDivider => self.last_sizes.clear(),
            _ => {}
        }
        if layout {
            self.commit_layout();
        }
        self.mark_dirty();
    }

    /// What a press is on, for an area selection that may end as a click:
    /// a stack's deck, else a box.
    fn press_target(&mut self, x: u16, y: u16) -> Option<CanvasPress> {
        if let Some(root) = self.deck_root_at(x, y) {
            return Some(CanvasPress::Deck(root));
        }
        match self.canvas_hit(x, y) {
            CanvasHit::Node(id) => Some(CanvasPress::Node(id)),
            _ => None,
        }
    }

    /// An area selection released where it was pressed: a right-click opens
    /// the menu of what is there (a box, a stack, a group, the canvas); a
    /// Shift- or Ctrl-click toggles the box (a stack: its top card) in the
    /// selection and clears its effects. Nothing starts.
    fn canvas_press_up(&mut self, click: Option<CanvasPress>, right: bool, x: u16, y: u16) {
        match (right, click) {
            (true, Some(CanvasPress::Node(id))) => {
                if !self.selection.contains(&id) {
                    self.select_node(&id, false);
                }
                let menu = self.node_menu(&id);
                self.open_menu(x, y, menu);
            }
            (true, Some(CanvasPress::Group(id))) => {
                self.select_group(&id);
                let menu = self.group_menu(&id);
                self.open_menu(x, y, menu);
            }
            (true, Some(CanvasPress::Deck(root))) => {
                if let Some(deck) = self.deck_of(&root) {
                    self.select_node(&deck.top, false);
                    let menu = self.stack_menu(&deck);
                    self.open_menu(x, y, menu);
                }
            }
            (true, None) => {
                let (wx, wy) = self.to_world(x as i32, y as i32);
                let menu = self.canvas_menu(wx, wy);
                self.open_menu(x, y, menu);
            }
            (false, Some(CanvasPress::Node(id))) => {
                let acks = self.effects_on(std::slice::from_ref(&id));
                self.ack_effects(acks);
                self.select_node(&id, true);
            }
            (false, Some(CanvasPress::Deck(root))) => {
                if let Some(deck) = self.deck_of(&root) {
                    let acks = self.effects_on(&deck.sessions);
                    self.ack_effects(acks);
                    self.select_node(&deck.top, true);
                }
            }
            (false, Some(CanvasPress::Group(_)) | None) => {}
        }
    }

    /// The boxes an area selection takes: every box drawn on its own
    /// (sessions, subsessions, subagent boxes, passive sessions; automation
    /// boxes are laid out by their group) whose drawn rectangle meets the
    /// band, never boxes in a stack or hidden with every subsession. Band and
    /// boxes are compared as drawn and hit-tested at every zoom: in cells
    /// (`node_screen`) in the TUI, the boxes' world rectangles at the precise
    /// pointer positions in the GUI (`canvas_hit_f`, the canvas model). The
    /// band ends at the canvas's edges.
    pub fn area_hits(&self, start: (u16, u16), now: (u16, u16)) -> Vec<String> {
        let concealed = self.concealed_now();
        let c = self.layout.canvas;
        let selectable = self.doc.nodes.iter().filter(|n| n.kind != NodeKind::Automation && !concealed.contains_key(&n.id));
        match self.render_mode {
            RenderMode::Gpu => {
                let (o, p) = (self.drag_origin, self.pointer);
                let (sx0, sx1) = (o.0.min(p.0).max(c.x as f32), o.0.max(p.0).min(c.right() as f32));
                let (sy0, sy1) = (o.1.min(p.1).max(c.y as f32), o.1.max(p.1).min(c.bottom() as f32));
                let (ax, ay) = self.to_world_f(sx0, sy0);
                let (bx, by) = self.to_world_f(sx1, sy1);
                selectable
                    .filter(|n| {
                        let r = n.rect();
                        (r.x as f32) <= bx && ((r.x + r.w) as f32) > ax && (r.y as f32) <= by && ((r.y + r.h) as f32) > ay
                    })
                    .map(|n| n.id.clone())
                    .collect()
            }
            RenderMode::Cells => {
                let (x0, x1) = (start.0.min(now.0).max(c.x) as i32, start.0.max(now.0).min(c.right().saturating_sub(1)) as i32);
                let (y0, y1) = (start.1.min(now.1).max(c.y) as i32, start.1.max(now.1).min(c.bottom().saturating_sub(1)) as i32);
                selectable
                    .filter(|n| {
                        let (x, y, w, h) = self.node_screen(n);
                        x <= x1 && x + w > x0 && y <= y1 && y + h > y0
                    })
                    .map(|n| n.id.clone())
                    .collect()
            }
        }
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
mod selection_tests {
    use std::collections::BTreeSet;

    use crate::app::App;
    use crate::dialogs::Modal;
    use crate::input::{KeyCode, Modifiers, MouseButton};
    use crate::undo::fixture::*;
    use crate::view::RenderMode;

    /// A box's rectangle as drawn (fractional cells): the TUI's cells, the GUI's model.
    fn drawn(app: &App, id: &str) -> (f32, f32, f32, f32) {
        let node = app.doc.node(id).unwrap();
        match app.render_mode {
            RenderMode::Cells => {
                let (x, y, w, h) = app.node_screen(node);
                (x as f32, y as f32, w as f32, h as f32)
            }
            RenderMode::Gpu => {
                let r = node.rect();
                let (x, y) = app.to_screen_f(r.x as f32, r.y as f32);
                (x, y, r.w as f32 * app.scale_f(), r.h as f32 * app.scale_f())
            }
        }
    }

    /// A band from inside the first box's corner to inside the far corner of
    /// the union of `ids` as drawn (in the TUI: the cells' centres).
    fn band(app: &App, ids: &[&str]) -> ((f32, f32), (f32, f32)) {
        let rects: Vec<_> = ids.iter().map(|id| drawn(app, id)).collect();
        let x0 = rects.iter().map(|r| r.0).fold(f32::MAX, f32::min);
        let y0 = rects.iter().map(|r| r.1).fold(f32::MAX, f32::min);
        let x1 = rects.iter().map(|r| r.0 + r.2).fold(f32::MIN, f32::max);
        let y1 = rects.iter().map(|r| r.1 + r.3).fold(f32::MIN, f32::max);
        match app.render_mode {
            RenderMode::Cells => ((x0 + 0.5, y0 + 0.5), (x1 - 0.5, y1 - 0.5)),
            RenderMode::Gpu => ((x0 + 0.1, y0 + 0.1), (x1 - 0.1, y1 - 0.1)),
        }
    }

    fn selected(app: &App) -> BTreeSet<&str> {
        app.selection.iter().map(String::as_str).collect()
    }

    fn set<'a>(ids: &[&'a str]) -> BTreeSet<&'a str> {
        ids.iter().copied().collect()
    }

    /// The TUI at 100, 50 and 25 %; the GUI at 100, 50, 25 % and a continuous
    /// 70 % with a fractional origin.
    fn every_zoom() -> Vec<(App, crossbeam_channel::Receiver<crate::protocol::ClientMsg>, String)> {
        let mut out = Vec::new();
        for zoom in 0..3u8 {
            let (mut app, sent) = app(false);
            app.doc.view.zoom = zoom;
            out.push((app, sent, format!("cells zoom {zoom}")));
        }
        for scale in [1.0, 0.5, 0.25, 0.7] {
            let (mut app, sent) = app(true);
            app.view_scale = scale;
            if scale == 0.7 {
                app.view_frac = (0.25, 0.5);
            }
            out.push((app, sent, format!("gpu {scale}")));
        }
        out
    }

    #[test]
    fn area_selection_takes_subagents_and_subsessions_like_sessions_at_every_zoom() {
        for (mut app, sent, mode) in every_zoom() {
            let before = layout(&app);
            // Shift-drag from on top of the parent over its subagent box.
            let (from, to) = band(&app, &["p", "p:helper"]);
            drag(&mut app, MouseButton::Left, from, to, Modifiers::SHIFT);
            assert_eq!(selected(&app), set(&["p", "p:helper"]), "{mode}");
            assert!(notice(&app).starts_with("2 boxes selected (1 session, 1 subagent)"), "{mode}: {}", notice(&app));
            // Parents and children together: the subsession and its subagent too.
            let (from, to) = band(&app, &["p", "c", "c:x"]);
            drag(&mut app, MouseButton::Left, from, to, Modifiers::SHIFT);
            assert_eq!(selected(&app), set(&["p", "p:helper", "c", "c:x"]), "{mode}");
            // A right-drag selects the same way (replacing the selection).
            let (from, to) = band(&app, &["c:x"]);
            drag(&mut app, MouseButton::Right, (from.0, from.1), (to.0 + 0.6, to.1 + 0.6), Modifiers::NONE);
            assert_eq!(selected(&app), set(&["c:x"]), "{mode}");
            assert!(app.modals.is_empty(), "a right-drag opens no menu ({mode})");
            assert_eq!(layout(&app), before, "area selection never drags a box ({mode})");
            assert_eq!(starts_or_stops(&sent), 0, "nor starts one ({mode})");
        }
    }

    #[test]
    fn the_band_meets_boxes_where_they_are_drawn() {
        // The TUI at 25 %: "p" (world 2..32) is drawn in cells 0..6; the cells
        // right of it are another box's world, not this one's.
        let (mut app, _sent) = app(false);
        app.doc.view.zoom = 2;
        assert_eq!(drawn(&app, "p"), (0.0, 0.0, 7.0, 1.0));
        drag(&mut app, MouseButton::Left, (7.5, 0.5), (9.5, 0.5), Modifiers::SHIFT);
        assert!(app.selection.is_empty(), "{:?}", app.selection);
        drag(&mut app, MouseButton::Left, (6.5, 0.5), (9.5, 0.5), Modifiers::SHIFT);
        assert_eq!(selected(&app), set(&["p"]), "its last drawn cell");
    }

    #[test]
    fn hidden_and_stacked_subsessions_stay_out_of_area_selection() {
        for gpu in [false, true] {
            let (mut app, _sent) = app(gpu);
            let everything = ((1.5, 1.5), (105.5, 25.5));
            key(&mut app, KeyCode::Char('h'), Modifiers::NONE);
            drag(&mut app, MouseButton::Left, everything.0, everything.1, Modifiers::SHIFT);
            assert_eq!(selected(&app), set(&["p", "p:helper", "o"]), "hidden subsessions (gpu {gpu})");
            key(&mut app, KeyCode::Char('h'), Modifiers::NONE);
            app.set_stacked("p", true);
            drag(&mut app, MouseButton::Left, everything.0, everything.1, Modifiers::SHIFT);
            assert_eq!(selected(&app), set(&["p", "p:helper", "o"]), "a stack's boxes (gpu {gpu})");
            app.set_stacked("p", false);
            drag(&mut app, MouseButton::Left, everything.0, everything.1, Modifiers::SHIFT);
            assert_eq!(selected(&app), set(&["p", "p:helper", "c", "c:x", "o"]), "shown again (gpu {gpu})");
        }
    }

    #[test]
    fn shift_and_ctrl_clicks_toggle_boxes_and_never_drag_or_start() {
        for gpu in [false, true] {
            let (mut app, sent) = app(gpu);
            let before = layout(&app);
            click(&mut app, MouseButton::Left, (5.5, 3.5), Modifiers::SHIFT);
            click(&mut app, MouseButton::Left, (45.5, 3.5), Modifiers::SHIFT);
            assert_eq!(app.selection, ["p", "c"], "gpu {gpu}");
            click(&mut app, MouseButton::Left, (5.5, 3.5), Modifiers::SHIFT);
            click(&mut app, MouseButton::Left, (10.5, 9.5), Modifiers::CONTROL);
            assert_eq!(app.selection, ["c", "p:helper"], "gpu {gpu}");
            // A Shift-double-click is two toggles; a click right after a Shift-click is no double-click.
            click(&mut app, MouseButton::Left, (45.5, 3.5), Modifiers::SHIFT);
            click(&mut app, MouseButton::Left, (45.5, 3.5), Modifiers::SHIFT);
            click(&mut app, MouseButton::Left, (45.5, 3.5), Modifiers::SHIFT);
            click(&mut app, MouseButton::Left, (45.5, 3.5), Modifiers::NONE);
            assert_eq!(app.selection, ["c"], "the plain click views it");
            // Shift-dragging from a box (or a group's title) never moves anything.
            drag(&mut app, MouseButton::Left, (5.5, 3.5), (20.5, 12.5), Modifiers::SHIFT);
            drag(&mut app, MouseButton::Left, (10.5, 0.5), (30.5, 5.5), Modifiers::SHIFT);
            assert_eq!(layout(&app), before, "gpu {gpu}");
            assert!(app.undo.is_empty());
            assert_eq!(starts_or_stops(&sent), 0, "gpu {gpu}");
        }
    }

    #[test]
    fn a_right_click_opens_the_menu_of_what_is_under_it() {
        let (mut app, _sent) = app(false);
        let title = |app: &App| match app.modals.last() {
            Some(Modal::Menu(menu)) => menu.title.clone(),
            _ => String::new(),
        };
        click(&mut app, MouseButton::Right, (5.5, 3.5), Modifiers::NONE);
        assert_eq!((title(&app), app.selection.clone()), ("p".to_owned(), vec!["p".to_owned()]));
        app.modals.clear();
        click(&mut app, MouseButton::Right, (10.5, 0.5), Modifiers::NONE);
        assert_eq!((title(&app), app.selected_group.as_deref()), ("Team".to_owned(), Some("g")));
        app.modals.clear();
        click(&mut app, MouseButton::Right, (150.5, 40.5), Modifiers::NONE);
        assert_eq!(title(&app), "Canvas");
    }

    #[test]
    fn removing_a_mixed_selection_follows_the_single_box_rules_and_undoes_at_once() {
        let (mut app, _sent) = app(false);
        let (from, to) = band(&app, &["p", "c", "c:x"]);
        drag(&mut app, MouseButton::Left, from, to, Modifiers::SHIFT);
        key(&mut app, KeyCode::Delete, Modifiers::NONE);
        let Some(Modal::Confirm(confirm)) = app.modals.last().cloned() else { panic!("a confirmation") };
        assert_eq!(confirm.title, "Remove 4 from the canvas?");
        app.modals.clear();
        app.run(confirm.buttons[0].1.clone());
        let left: Vec<&str> = app.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(left, ["o", "z"], "the parent, its subagent, the subsession and its subagent");
        ctrl(&mut app, 'z');
        assert_eq!(app.doc.nodes.len(), 6);
        assert_eq!(app.doc.node("c").and_then(|n| n.parent.as_deref()), Some("p"), "the link comes back too");
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