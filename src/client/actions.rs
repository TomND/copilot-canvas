//! Commands triggered from keys, menus and buttons, plus background results.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::app::{App, AppEvent, Focus};
use super::dialogs::{
    Action, Confirm, FolderBrowser, GitState, Grouping, ImportCopilot, Menu, MenuItem, Modal, NewSession, NsField, Rename, Target,
    TextInput,
};
use super::doc::{GRID_X, Node, NodeKind};
use crate::git;
use crate::paths::now_ms;
use crate::protocol::ClientMsg;

fn item(label: &str, hint: &str, action: Action, enabled: bool) -> MenuItem {
    MenuItem { label: label.to_owned(), hint: hint.to_owned(), action, enabled }
}

impl App {
    pub fn open_menu(&mut self, x: u16, y: u16, (title, items): (String, Vec<MenuItem>)) {
        let sel = items.iter().position(|i| i.enabled).unwrap_or(0);
        self.modals.push(Modal::Menu(Menu { x, y, title, items, sel }));
    }

    pub fn node_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        let Some(node) = self.doc.node(id) else { return (String::new(), Vec::new()) };
        let session = self.session_of(id).unwrap_or_default();
        let alive = self.owned_alive(&session);
        let external = self.sessions.get(&session).is_some_and(|s| s.live) && !alive;
        let mut items = Vec::new();
        if node.kind == NodeKind::Automation {
            let key = node.automation.clone().unwrap_or_default();
            let toggleable = self.automation(&key).is_some_and(|a| a.toggleable);
            let enabled = self.automation(&key).is_some_and(|a| a.enabled);
            let task = self.automation(&key).is_some_and(|a| a.factory.is_some());
            items.push(item("Show details", "Enter", Action::Open(id.to_owned()), true));
            if task {
                items.push(item("Run now", "", Action::RunAutomation(key.clone()), toggleable));
                items.push(item("Ask doctor to check", "", Action::AskDoctor(key.clone()), true));
                items.push(item("Open manifest", "", Action::OpenAutomationFile(key.clone(), 0), true));
                items.push(item("Open launcher log", "", Action::OpenAutomationFile(key.clone(), 1), true));
            }
            items.push(item(if enabled { "Disable automation" } else { "Enable automation" }, "Space", Action::ToggleAutomation(key), toggleable));
            items.push(item("Rename box", "R", Action::Rename(Target::Node(id.to_owned())), true));
            return (node.name.clone(), items);
        }
        if node.kind == NodeKind::Agent {
            items.push(item("Open parent terminal", "Enter", Action::Open(id.to_owned()), !external));
            items.push(item("Rename box", "R", Action::Rename(Target::Node(id.to_owned())), true));
            items.push(item("Remove box", "Del", Action::RemoveNode(id.to_owned()), true));
            return (node.name.clone(), items);
        }
        if node.passive.is_some() {
            let paused = node.passive.as_ref().is_some_and(|p| p.paused);
            items.push(item("Run now", "", Action::PassiveRun(id.to_owned()), !external));
            items.push(item(if paused { "Resume" } else { "Pause" }, "", Action::PassivePause(id.to_owned()), true));
            items.push(item("Settings…", "", Action::PassiveDetail(id.to_owned()), true));
            items.push(item("Message…", "", Action::MessagePassive(id.to_owned()), true));
            items.push(item("Edit rules", "", Action::PassiveOpen(id.to_owned(), 0), true));
            items.push(item("Open run log", "", Action::PassiveOpen(id.to_owned(), 2), true));
            items.push(item(if alive { "Focus chat" } else { "Open chat" }, "", Action::PassiveChat(id.to_owned()), !external));
            items.push(item("MCP servers…", "", Action::McpServers(id.to_owned()), true));
            items.push(item("Rename", "R", Action::Rename(Target::Node(id.to_owned())), true));
            items.push(item("Stop run", "X", Action::Stop { id: id.to_owned(), force: false }, alive));
            items.push(item("Remove from canvas", "Del", Action::RemoveNode(id.to_owned()), true));
            return (node.name.clone(), items);
        }
        let open_label = if alive { "Focus terminal" } else { "Open terminal" };
        items.push(item(open_label, "Enter", Action::Open(id.to_owned()), !external));
        items.push(item("New subsession", "S", Action::NewSession { at: None, parent: Some(id.to_owned()) }, true));
        items.push(item("Rename", "R", Action::Rename(Target::Node(id.to_owned())), true));
        if !self.demo {
            items.push(item("MCP servers…", "", Action::McpServers(id.to_owned()), true));
            items.push(item(if node.no_messages { "Accept passive messages" } else { "Mute passive messages" }, "", Action::ToggleMessages(id.to_owned()), true));
            let queued = self.bus.mailbox.get(id).copied().unwrap_or(0);
            if queued > 0 {
                items.push(item(&format!("Deliver {queued} queued message(s)"), "", Action::Open(id.to_owned()), !external));
                items.push(item("Discard queued messages", "", Action::DiscardMail(id.to_owned()), true));
            }
        }
        let group_label = if self.selection.len() > 1 { "Group selected sessions" } else { "New group around this" };
        items.push(item(group_label, "G", Action::GroupSelection, true));
        if !self.demo {
            items.push(item("Copy session ID", "", Action::Copy(id.to_owned()), true));
        }
        items.push(item("Stop session", "X", Action::Stop { id: id.to_owned(), force: false }, alive));
        items.push(item("Force stop", "", Action::Stop { id: id.to_owned(), force: true }, alive));
        items.push(item("Remove from canvas", "Del", Action::RemoveNode(id.to_owned()), true));
        (node.name.clone(), items)
    }

    pub fn group_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        let name = self.doc.group(id).map(|g| g.name.clone()).unwrap_or_default();
        (
            name,
            vec![
                item("Show running terminals", "Enter", Action::OpenGroup(id.to_owned()), true),
                item("New session in group", "", Action::NewSession { at: self.group_slot(id), parent: None }, true),
                item("Open AI docs folder", "", Action::OpenGroupFolder(id.to_owned(), false), true),
                item("Open human outputs folder", "", Action::OpenGroupFolder(id.to_owned(), true), true),
                item("Change group folder…", "", Action::SetGroupFolder(id.to_owned()), true),
                item("Session defaults for group…", "", Action::GroupDefaults(id.to_owned()), true),
                item("New passive session…", "", Action::NewPassive(id.to_owned()), !self.demo),
                item("Send event…", "", Action::SendEvent(id.to_owned()), !self.demo),
                item("Edit events…", "", Action::OpenEvents(id.to_owned()), !self.demo),
                item("Rename group", "R", Action::Rename(Target::Group(id.to_owned())), true),
                item("Fit to members", "", Action::FitGroup(id.to_owned()), true),
                item("Change color", "", Action::CycleGroupColor(id.to_owned()), true),
                item("Delete group (keep sessions)", "Del", Action::DeleteGroup(id.to_owned()), true),
            ],
        )
    }

    fn group_slot(&self, id: &str) -> Option<(i32, i32)> {
        let group = self.doc.group(id)?;
        let columns = ((group.w - 2) / GRID_X).max(1);
        Some(self.doc.free_slot(group.x + 2, group.y + 2, columns, NodeKind::Session))
    }

    pub fn canvas_menu(&self, wx: i32, wy: i32) -> (String, Vec<MenuItem>) {
        (
            "Canvas".into(),
            vec![
                item("New session here", "N", Action::NewSession { at: Some((wx, wy)), parent: None }, true),
                item("New group here", "", Action::NewGroupAt(wx, wy), true),
                item("Import Copilot sessions…", "I", Action::ImportCopilot, !self.demo),
                item("Sync VS Code sessions", "V", Action::SyncVs, !self.demo),
                item("Undo last sync", "Ctrl+Z", Action::UndoSync, self.undo.is_some()),
                item("Migrate VS Code skills & settings…", "", Action::Migrate, !self.demo),
                item("Fit everything", "F", Action::FitAll, true),
                item(if self.doc.ui.tiled { "Single terminal view" } else { "Tile terminals" }, "T", Action::ToggleTiled, true),
                item("Help", "?", Action::Help, true),
            ],
        )
    }

    pub fn tile_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        let alive = self.owned_alive(id);
        let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_owned());
        (
            name,
            vec![
                item("Copy selection", "Ctrl+C", Action::CopySelection, self.term_select.as_ref().is_some_and(|s| s.id == id)),
                item("Paste", "Ctrl+V", Action::Paste(id.to_owned()), alive),
                item("Show on canvas", "", Action::Open(id.to_owned()), true),
                item("Close tile (keeps running)", "", Action::CloseTile(id.to_owned()), true),
                item("Stop session", "", Action::Stop { id: id.to_owned(), force: false }, alive),
                item("Force stop", "", Action::Stop { id: id.to_owned(), force: true }, alive),
            ],
        )
    }

    /// Copies the terminal selection to the clipboard (the highlight stays).
    pub fn copy_selection(&mut self) {
        let Some(selection) = self.term_select.clone() else { return };
        let Some(text) = self.views.get(&selection.id).map(|v| v.selection_text(&selection)) else { return };
        if text.is_empty() {
            return;
        }
        match super::clipboard::set_text(&text) {
            Ok(()) => self.notify(format!("Copied {} characters", text.chars().count()), false),
            Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
        }
    }

    pub fn paste_clipboard(&mut self, id: &str) {
        if !self.owned_alive(id) {
            return;
        }
        // Like Windows Terminal: text is pasted; anything else (an image, copied
        // files) sends Ctrl+V itself, and Copilot reads the clipboard to attach it.
        if !super::clipboard::has_text() {
            return self.send_ctrl_v(id);
        }
        match super::clipboard::get_text() {
            Ok(text) if !text.is_empty() => self.send(ClientMsg::Input { id: id.to_owned(), input: crate::protocol::Input::Paste(text) }),
            Ok(_) => self.send_ctrl_v(id),
            Err(error) => self.notify(format!("Paste failed: {error:#}"), true),
        }
    }

    pub fn send_ctrl_v(&mut self, id: &str) {
        if self.owned_alive(id) {
            let input = crate::protocol::Input::Key { key: crate::protocol::Key::Char('v'), mods: crate::protocol::MOD_CTRL };
            self.send(ClientMsg::Input { id: id.to_owned(), input });
        }
    }

    pub fn confirm_stop(&mut self, id: String) {
        let name = self.doc.node(&id).map(|n| n.name.clone()).unwrap_or_else(|| id.clone());
        self.modals.push(Modal::Confirm(Confirm {
            title: format!("Stop “{name}”?"),
            body: vec!["Copilot exits gracefully; its conversation stays resumable.".into()],
            buttons: vec![
                ("Stop".into(), Action::Stop { id: id.clone(), force: false }),
                ("Force stop".into(), Action::Stop { id, force: true }),
                ("Cancel".into(), Action::Nothing),
            ],
            sel: 0,
        }));
    }

    /// A Copilot session with history on disk (its conversation can be forked).
    pub fn can_fork(&self, id: &str) -> bool {
        crate::index::is_session_id(id)
            && self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Session && n.command.is_none())
            && self.instance.as_ref().is_some_and(|i| i.sessions_dir.join(id).join("events.jsonl").is_file())
    }

    /// Opens the MCP server checklist for a new session or an existing one.
    pub fn open_mcp_picker(&mut self, target: super::dialogs::McpTarget) {
        if self.config.is_none() {
            self.refresh_config();
        }
        let off = match &target {
            super::dialogs::McpTarget::NewSession => match self.modals.last() {
                Some(Modal::NewSession(dialog)) => dialog.mcp_off.clone(),
                _ => self.doc.mcp.last_off.clone(),
            },
            super::dialogs::McpTarget::Node(id) => self.doc.node(id).and_then(|n| n.mcp_off.clone()).unwrap_or_default(),
        };
        self.modals.push(Modal::Mcp(super::dialogs::McpPicker { target, off, sel: 0 }));
        self.needs_draw = true;
    }

    /// Applies a picker's choice to the new-session dialog under it or to a session.
    pub fn apply_mcp_choice(&mut self, target: &super::dialogs::McpTarget, off: Vec<String>) {
        let off = super::doc::normalize_off(&off);
        match target {
            super::dialogs::McpTarget::NewSession => {
                if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
                    dialog.mcp_off = off;
                }
            }
            super::dialogs::McpTarget::Node(id) => {
                let alive = self.owned_alive(id);
                if let Some(node) = self.doc.node_mut(id) {
                    node.mcp_off = Some(off.clone());
                }
                self.mark_dirty();
                let summary = self.mcp_summary(&off);
                let when = if alive { " Applies the next time it starts (stop it and click it to restart now)." } else { "" };
                self.notify(format!("MCP servers for this session: {summary}.{when}"), false);
            }
        }
    }

    pub fn confirm_update_service(&mut self) {
        let alive: Vec<&String> = self.terms.iter().filter(|(_, t)| t.alive).map(|(id, _)| id).collect();
        let working = alive.iter().filter(|id| self.doc.node(id).is_some_and(|n| self.node_status(n) == super::status::Status::Working)).count();
        let mut body = vec![
            "Gets the new build's terminal fixes (smooth wheel scrolling, keyboard priority).".to_owned(),
            format!("{} running session(s) stop and resume automatically right after.", alive.len()),
        ];
        if working > 0 {
            body.push(format!("{working} of them are working right now; their current turn is interrupted."));
        }
        self.modals.push(Modal::Confirm(Confirm {
            title: "Update the terminal service?".into(),
            body,
            buttons: vec![("Restart and resume".into(), Action::RestartService), ("Later".into(), Action::Nothing)],
            sel: if working > 0 { 1 } else { 0 },
        }));
    }

    /// Restarts the service on this build, then restarts the terminals that were
    /// running (Copilot resumes its conversation; panes and focus stay as they are).
    pub fn restart_service(&mut self) {
        let Some(instance) = self.instance.clone() else { return };
        if self.restarting.is_some() {
            return;
        }
        let resume: Vec<String> = self.terms.iter().filter(|(id, t)| t.alive && self.doc.node(id).is_some()).map(|(id, _)| id.clone()).collect();
        let demo = self.demo;
        self.notify(format!("Restarting the terminal service… {} session(s) will resume.", resume.len()), false);
        self.restarting = Some(resume);
        self.send(ClientMsg::Shutdown);
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && super::conn::try_connect(&instance).is_ok() {
                std::thread::sleep(Duration::from_millis(300));
            }
            let mouse = !demo && crate::copilot_cfg::restore_mouse().unwrap_or(false);
            let result = super::conn::connect(&instance, true).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::ServiceRestarted { result, mouse });
        });
    }

    fn on_service_restarted(&mut self, result: Result<super::conn::Conn, String>, mouse: bool) {
        let resume = self.restarting.take().unwrap_or_default();
        let conn = match result {
            Ok(conn) => conn,
            Err(error) => {
                self.connected = false;
                self.notify(format!("The terminal service did not come back: {error}. Restart Copilot Canvas."), true);
                return;
            }
        };
        self.build_mismatch = conn.service_build != crate::paths::build_id();
        self.routing_sent.clear();
        self.new_service_rx = Some(conn.rx.clone());
        self.conn = conn;
        self.connected = true;
        self.terms.clear();
        self.views.clear();
        self.launching.clear();
        self.terms_rev += 1;
        let count = self.resume_terminals(&resume);
        let mouse = if mouse { " Copilot's mouse setting was off (forced by the old service); it is on again, so the wheel scrolls smoothly." } else { "" };
        self.notify(format!("Terminal service updated; resumed {count} session(s).{mouse}"), false);
    }

    /// Restarts terminals that a replaced service was running (Copilot resumes
    /// its conversation) without moving tiles or focus. Returns how many.
    pub fn resume_terminals(&mut self, ids: &[String]) -> usize {
        let (slots, focused, focus) = (self.tile_slots.clone(), self.focused_term.clone(), self.focus);
        let ids: Vec<String> = ids.iter().filter(|id| self.doc.node(id).is_some()).cloned().collect();
        for id in &ids {
            self.launch(id);
        }
        self.tile_slots = slots;
        self.focused_term = focused;
        self.focus = focus;
        ids.len()
    }

    pub fn request_quit(&mut self) {
        let alive = self.terms.values().filter(|t| t.alive).count();
        if alive == 0 {
            self.quit = true;
            return;
        }
        self.modals.push(Modal::Confirm(Confirm {
            title: "Close Copilot Canvas".into(),
            body: vec![
                format!("{alive} session(s) are running in the terminal service."),
                "Detaching keeps them running; reopen Copilot Canvas to return to them.".into(),
            ],
            buttons: vec![
                ("Detach (keep running)".into(), Action::Detach),
                ("Stop all and quit".into(), Action::StopAllAndQuit),
                ("Cancel".into(), Action::Nothing),
            ],
            sel: 0,
        }));
    }

    pub fn run(&mut self, action: Action) {
        self.needs_draw = true;
        match action {
            Action::Open(id) => {
                self.reveal_node(&id);
                self.open_node(&id);
            }
            Action::OpenGroup(id) => self.open_group(&id),
            Action::NewSession { at, parent } => self.open_new_session(at, parent),
            Action::Rename(target) => {
                let current = match &target {
                    Target::Node(id) => self.doc.node(id).map(|n| n.name.clone()),
                    Target::Group(id) => self.doc.group(id).map(|g| g.name.clone()),
                };
                if let Some(current) = current {
                    self.modals.push(Modal::Rename(Rename { target, input: TextInput::new(&current) }));
                }
            }
            Action::GroupSelection => {
                let mut ids: Vec<String> = self.selection.iter().filter_map(|id| self.session_of(id)).collect();
                ids.dedup();
                if ids.is_empty() {
                    return self.notify("Select sessions first (Shift-drag or right-drag to select several).", false);
                }
                if let Some(id) = self.doc.group_around(&ids, "New group") {
                    self.mark_dirty();
                    self.select_group(&id);
                    // Name first (top), then choose the group's folder.
                    self.ask_group_folder(&id);
                    self.modals.push(Modal::Rename(Rename { target: Target::Group(id), input: TextInput::new("New group") }));
                }
            }
            Action::NewGroupAt(x, y) => {
                let id = format!("g-{}", uuid::Uuid::new_v4());
                let color = (self.doc.groups.len() % 6) as u8;
                self.doc.groups.push(super::doc::Group { id: id.clone(), name: "New group".into(), x, y, w: 2 * GRID_X + 4, h: 12, color, folder: None, source: None, defaults: None });
                self.mark_dirty();
                self.select_group(&id);
                self.ask_group_folder(&id);
                self.modals.push(Modal::Rename(Rename { target: Target::Group(id), input: TextInput::new("New group") }));
            }
            Action::DeleteGroup(id) => {
                self.doc.groups.retain(|g| g.id != id);
                self.doc.ui.collapsed.retain(|c| *c != id);
                self.selected_group = None;
                self.mark_dirty();
            }
            Action::FitGroup(id) => {
                self.doc.fit_group(&id);
                self.mark_dirty();
            }
            Action::CycleGroupColor(id) => {
                if let Some(group) = self.doc.group_mut(&id) {
                    group.color = (group.color + 1) % 6;
                }
                self.mark_dirty();
            }
            Action::RemoveNode(id) => {
                let session = self.doc.node(&id).filter(|n| n.kind == NodeKind::Session).map(|n| n.id.clone());
                if session.as_ref().is_some_and(|s| self.owned_alive(s)) {
                    return self.notify("Stop this session (X) before removing its box. History is never deleted.", true);
                }
                if let Some(session) = &session {
                    self.send(ClientMsg::Forget { id: session.clone() });
                    self.tile_slots.retain(|t| t != session);
                }
                self.doc.remove_node(&id);
                self.selection.retain(|s| *s != id);
                self.mark_dirty();
            }
            Action::Stop { id, force } => {
                if self.owned_alive(&id) {
                    self.send(ClientMsg::Stop { id, force });
                    self.notify(if force { "Force stopping…" } else { "Stopping (Copilot exits gracefully)…" }, false);
                }
            }
            Action::Copy(text) => self.copy_to_clipboard(&text),
            Action::ImportCopilot => {
                if self.demo {
                    return self.notify("Import is unavailable in the demo.", false);
                }
                self.send(ClientMsg::RefreshSessions);
                self.modals.push(Modal::ImportCopilot(ImportCopilot {
                    filter: TextInput::default(),
                    sel: 0,
                    scroll: 0,
                    chosen: Default::default(),
                    grouping: Grouping::Folder,
                }));
            }
            Action::SyncVs => self.start_vs_sync(),
            Action::Migrate => self.start_migration(),
            Action::UndoSync => self.undo_last(),
            Action::Quit => self.request_quit(),
            Action::CopySelection => self.copy_selection(),
            Action::Paste(id) => self.paste_clipboard(&id),
            Action::View(view) => self.set_view(view),
            Action::RemoveConfig(kind, name) => {
                let label = format!("Removing {name}");
                self.run_cli_op(label, move || crate::copilot_cfg::remove(kind, &name));
            }
            Action::ToggleAutomation(key) => self.toggle_automation(&key),
            Action::RunAutomation(key) => self.run_automation(&key),
            Action::OpenAutomationFile(key, which) => self.open_automation_file(&key, which),
            Action::OpenTarget(target) => {
                if let Err(error) = super::factory::open_target(&target) {
                    self.notify(error, true);
                }
            }
            Action::AskDoctor(key) => self.ask_doctor(&key),
            Action::ShowAutomations => self.set_view(super::side::SideView::Automations),
            Action::ToggleConfig(kind, name, enable) => {
                let label = format!("{} {name}", if enable { "Enabling" } else { "Disabling" });
                self.run_cli_op(label, move || crate::copilot_cfg::set_enabled(kind, &name, enable));
            }
            Action::UpdatePlugin(name) => {
                let label = format!("Updating plugin {name}");
                self.run_cli_op(label, move || crate::copilot_cfg::update_plugin(&name));
            }
            Action::CloseDetail => {
                self.right_detail = None;
                let keep: Vec<String> = self.selection.iter().filter(|id| self.doc.node(id).is_some_and(|n| n.kind != NodeKind::Automation)).cloned().collect();
                self.selection = keep;
            }
            Action::OpenPath(path) => {
                if let Err(error) = crate::copilot_cfg::open_in_explorer(std::path::Path::new(&path)) {
                    self.notify(format!("{error:#}"), true);
                }
            }
            Action::SetGroupFolder(id) => self.ask_group_folder(&id),
            Action::OpenGroupFolder(id, outputs) => self.open_group_folder(&id, outputs),
            Action::GroupDefaults(id) => {
                self.settings_group = Some(id);
                self.set_view(super::side::SideView::Settings);
                if self.config.is_none() {
                    self.refresh_config();
                }
            }
            Action::FitAll => self.fit_all(),
            Action::Zoom(delta) => {
                let c = self.layout.canvas;
                self.zoom_at((c.x + c.width / 2) as i32, (c.y + c.height / 2) as i32, delta);
            }
            Action::ToggleTiled => {
                self.doc.ui.tiled = !self.doc.ui.tiled;
                if self.doc.ui.tiled && self.tile_slots.is_empty() {
                    let recent: Vec<String> = self.doc.tiles.iter().filter(|t| self.terms.contains_key(*t)).take(4).cloned().collect();
                    for id in recent.into_iter().rev() {
                        self.show_terminal(&id);
                    }
                }
                self.last_sizes.clear();
                self.mark_dirty();
            }
            Action::CloseTile(id) => {
                self.tile_slots.retain(|t| *t != id);
                if self.focused_term.as_deref() == Some(&id) {
                    self.focused_term = self.tile_slots.first().cloned();
                }
                if !self.doc.ui.tiled {
                    let keep: Vec<String> = self.selection.iter().filter(|s| self.session_of(s).as_deref() != Some(&id)).cloned().collect();
                    self.selection = keep;
                }
                if self.focus == Focus::Terminal {
                    self.set_focus(Focus::Canvas);
                }
            }
            Action::Detach => self.quit = true,
            Action::UpdateService => self.confirm_update_service(),
            Action::McpServers(id) => self.open_mcp_picker(super::dialogs::McpTarget::Node(id)),
            Action::NewPassive(group) => self.new_passive(&group),
            Action::PassiveDetail(id) => {
                self.select_node(&id, false);
                self.right_detail = Some(super::side::RightDetail::Passive(id));
            }
            Action::PassiveChat(id) => {
                if self.owned_alive(&id) {
                    self.show_terminal(&id);
                    self.set_focus(Focus::Terminal);
                } else if self.sessions.get(&id).is_some_and(|s| s.live) {
                    self.notify("This passive session is running elsewhere right now.", true);
                } else {
                    self.launch(&id);
                }
            }
            Action::PassiveRun(id) => {
                if let Some(key) = self.doc.node(&id).and_then(|n| n.passive.as_ref()).map(|p| p.key.clone()) {
                    self.sync_routing();
                    self.bus_action(serde_json::json!({ "action": "run", "key": key }));
                    self.notify("Run requested; it starts within a few seconds.", false);
                }
            }
            Action::PassivePause(id) => {
                let key = self.doc.node_mut(&id).and_then(|n| n.passive.as_mut()).map(|p| {
                    p.paused = !p.paused;
                    (p.key.clone(), p.paused)
                });
                if let Some((key, paused)) = key {
                    self.mark_dirty();
                    if !paused {
                        self.bus_action(serde_json::json!({ "action": "resume", "key": key }));
                    }
                    self.sync_routing();
                }
            }
            Action::PassiveEdit(id, field) => self.edit_passive(&id, field),
            Action::PassiveOpen(id, which) => {
                if let Some(paths) = self.doc.node(&id).and_then(|n| self.passive_paths(n)) {
                    let path = match which {
                        0 => paths.rules,
                        1 => paths.state,
                        2 => paths.log,
                        _ => paths.dir,
                    };
                    match super::links::open(&path.display().to_string()) {
                        Ok(message) => self.notify(message, false),
                        Err(error) => self.notify(format!("{error:#}"), true),
                    }
                }
            }
            Action::SendEvent(group) => self.modals.push(Modal::Prompt(super::dialogs::Prompt {
                title: "Send an event to this group's passive sessions".into(),
                hint: "type [json data], for example: pr.merged {\"pr\": 164587}".into(),
                input: TextInput::default(),
                purpose: super::dialogs::PromptPurpose::SendEvent(group),
            })),
            Action::OpenEvents(group) => match self.doc.group(&group).and_then(|g| g.folder.clone()) {
                Some(folder) => match crate::bus::ensure_catalog(std::path::Path::new(&folder)) {
                    Ok(path) => match super::links::open(&path.display().to_string()) {
                        Ok(message) => self.notify(message, false),
                        Err(error) => self.notify(format!("{error:#}"), true),
                    },
                    Err(error) => self.notify(format!("{error:#}"), true),
                },
                None => self.notify("Set the group's folder first (group menu → Change group folder…).", true),
            },
            Action::MessagePassive(id) => self.modals.push(Modal::Prompt(super::dialogs::Prompt {
                title: format!("Message {}", self.doc.node(&id).map(|n| n.name.clone()).unwrap_or_default()),
                hint: "It runs with your message (as a \"message\" event).".into(),
                input: TextInput::default(),
                purpose: super::dialogs::PromptPurpose::MessagePassive(id),
            })),
            Action::ToggleMessages(id) => {
                if let Some(node) = self.doc.node_mut(&id) {
                    node.no_messages = !node.no_messages;
                }
                self.mark_dirty();
                self.sync_routing();
            }
            Action::DiscardMail(id) => self.bus_action(serde_json::json!({ "action": "discard_mail", "to": id })),
            Action::RestartService => self.restart_service(),
            Action::StopAllAndQuit => {
                let alive: Vec<String> = self.terms.values().filter(|t| t.alive).map(|t| t.id.clone()).collect();
                for id in alive {
                    self.send(ClientMsg::Stop { id, force: false });
                }
                self.quit = true;
            }
            Action::Help => self.modals.push(Modal::Help),
            Action::Nothing => {}
        }
    }

    // ---------- new session / subsession ----------

    fn open_new_session(&mut self, at: Option<(i32, i32)>, parent: Option<String>) {
        let can_fork = !self.demo && parent.as_deref().is_some_and(|p| self.can_fork(p));
        let folder = parent
            .as_ref()
            .and_then(|p| self.doc.node(p))
            .and_then(|n| n.cwd.clone().or_else(|| self.sessions.get(&n.id).map(|s| s.cwd.clone())))
            .filter(|c| !c.is_empty())
            .or_else(|| self.doc.recent_folders.first().map(|f| f.path.clone()))
            .unwrap_or_else(|| self.default_cwd.clone());
        let dialog = NewSession {
            parent,
            at,
            name: TextInput::default(),
            folder: TextInput::new(&folder),
            prompt: TextInput::default(),
            git: GitState::Unknown,
            probed: String::new(),
            folder_changed_at: Some(Instant::now() - Duration::from_secs(1)),
            worktree: false,
            base: String::new(),
            new_branch: TextInput::default(),
            branch_edited: false,
            wt_path: TextInput::default(),
            path_edited: false,
            focus: NsField::Name,
            error: None,
            mcp_off: self.doc.mcp.last_off.clone(),
            can_fork,
            fork: can_fork,
        };
        if self.config.is_none() {
            self.refresh_config();
        }
        self.modals.push(Modal::NewSession(Box::new(dialog)));
    }

    /// Starts a Git probe once the folder field has been stable for a moment.
    pub fn poll_git_debounce(&mut self) {
        let Some(Modal::NewSession(dialog)) = self.modals.iter_mut().rev().find(|m| matches!(m, Modal::NewSession(_))) else { return };
        let Some(changed) = dialog.folder_changed_at else { return };
        if changed.elapsed() < Duration::from_millis(250) {
            return;
        }
        dialog.folder_changed_at = None;
        let folder = dialog.folder.text.trim().to_owned();
        if folder == dialog.probed && !matches!(dialog.git, GitState::Unknown) {
            return;
        }
        dialog.probed = folder.clone();
        if !Path::new(&folder).is_dir() {
            dialog.git = GitState::Error("folder not found".into());
            return;
        }
        self.git_req += 1;
        let req = self.git_req;
        dialog.git = GitState::Checking(req);
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = git::probe(Path::new(&folder)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Git { req, folder, result });
        });
    }

    pub fn refresh_worktree_defaults(dialog: &mut NewSession) {
        if !dialog.branch_edited {
            let name = dialog.name.text.trim();
            dialog.new_branch.set(&if name.is_empty() { String::new() } else { git::slug(name) });
        }
        if !dialog.path_edited
            && let GitState::Repo(info) = &dialog.git
        {
            let branch = if dialog.new_branch.text.trim().is_empty() { dialog.base.clone() } else { dialog.new_branch.text.clone() };
            let path = if branch.is_empty() { String::new() } else { git::default_worktree_path(&info.root, &branch).to_string_lossy().into_owned() };
            dialog.wt_path.set(&path);
        }
    }

    pub fn on_app_event(&mut self, event: AppEvent) {
        self.needs_draw = true;
        match event {
            AppEvent::Git { req, folder, result } => {
                for modal in self.modals.iter_mut() {
                    if let Modal::NewSession(dialog) = modal
                        && dialog.git == GitState::Checking(req)
                        && dialog.probed == folder
                    {
                        dialog.git = match result {
                            Ok(Some(info)) => {
                                if dialog.base.is_empty() || !info.branches.contains(&dialog.base) {
                                    dialog.base = info.branch.clone().or_else(|| info.branches.first().cloned()).unwrap_or_default();
                                }
                                GitState::Repo(info)
                            }
                            Ok(None) => {
                                dialog.worktree = false;
                                GitState::NotRepo
                            }
                            Err(error) => GitState::Error(error),
                        };
                        Self::refresh_worktree_defaults(dialog);
                        break;
                    }
                }
            }
            AppEvent::Forked { node, result } => {
                self.preparing.remove(&node);
                match result {
                    Ok(()) => {
                        let parent = self.doc.node(&node).and_then(|n| n.parent.clone()).and_then(|p| self.doc.node(&p)).map(|p| p.name.clone()).unwrap_or_default();
                        self.notify(format!("Forked “{parent}”'s conversation; starting the subsession."), false);
                        self.launch(&node);
                    }
                    Err(error) => {
                        // Never leave a box that would start a fresh session under the fork's id.
                        if let Some(n) = self.doc.node_mut(&node) {
                            n.fresh = true;
                        }
                        self.node_errors.insert(node, "fork failed".into());
                        self.notify(format!("Could not fork the parent's conversation: {error}. Click the box to start it fresh."), true);
                    }
                }
            }
            AppEvent::Worktree { node, result } => {
                self.preparing.remove(&node);
                match result {
                    Ok(()) => {
                        self.notify("Worktree ready; starting the session.", false);
                        self.launch(&node);
                    }
                    Err(error) => {
                        self.node_errors.insert(node, "worktree failed".into());
                        self.notify(format!("Worktree creation failed: {error}"), true);
                    }
                }
            }
            AppEvent::Config(snapshot) => {
                self.config_loading = false;
                self.config = Some(*snapshot);
            }
            AppEvent::Automations(load) => self.on_automations(load),
            AppEvent::CliDone { label, result } => {
                self.cli_busy = None;
                match result {
                    Ok(_) => self.notify(format!("{label}: done."), false),
                    Err(error) => self.notify(format!("{label}: {error}"), true),
                }
                self.refresh_config();
            }
            AppEvent::VsSync(candidates, warnings) => self.apply_vs_sync(candidates, warnings),
            AppEvent::MigratePlan(items) => {
                for modal in self.modals.iter_mut() {
                    if let Modal::Migrate(dialog) = modal {
                        dialog.chosen = items.iter().map(|i| i.status == crate::migrate::Status::New).collect();
                        dialog.items = Some(items.clone());
                    }
                }
            }
            AppEvent::ServiceRestarted { result, mouse } => self.on_service_restarted(result, mouse),
            AppEvent::MigrateDone(result) => {
                for modal in self.modals.iter_mut() {
                    if let Modal::Migrate(dialog) = modal {
                        dialog.applying = false;
                        dialog.result = Some(match &result {
                            Ok(lines) => lines.clone(),
                            Err(error) => vec![format!("Migration failed: {error}")],
                        });
                    }
                }
                self.config = None;
                self.refresh_config();
            }
        }
    }

    /// Validates the dialog and creates the session (and worktree if requested).
    pub fn submit_new_session(&mut self, dialog: &NewSession) -> Result<(), String> {
        let folder = dialog.folder.text.trim().trim_matches('"').to_owned();
        if !Path::new(&folder).is_dir() {
            return Err(format!("Folder not found: {folder}"));
        }
        let folder = crate::paths::absolute(Path::new(&folder)).to_string_lossy().into_owned();
        let mut name = dialog.name.text.trim().to_owned();
        if name.is_empty() {
            name = Path::new(&folder).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Session".into());
        }
        let mut worktree_job = None;
        let cwd = if dialog.worktree {
            let GitState::Repo(info) = &dialog.git else { return Err("The folder is not a Git repository.".into()) };
            let new_branch = dialog.new_branch.text.trim().to_owned();
            if !new_branch.is_empty() && !git::valid_branch_name(&new_branch) {
                return Err(format!("Invalid branch name: {new_branch}"));
            }
            if new_branch.is_empty() && dialog.base.is_empty() {
                return Err("Choose a base branch or enter a new branch name.".into());
            }
            if !new_branch.is_empty() && info.branches.iter().take(info.local_count).any(|b| *b == new_branch) {
                return Err(format!("Branch {new_branch} already exists; leave the new branch empty to check it out."));
            }
            let path = dialog.wt_path.text.trim().to_owned();
            if path.is_empty() {
                return Err("Enter a worktree folder.".into());
            }
            let path = PathBuf::from(&path);
            if path.exists() && path.read_dir().map(|mut d| d.next().is_some()).unwrap_or(true) {
                return Err(format!("{} already exists and is not empty.", path.display()));
            }
            worktree_job = Some((info.root.clone(), path.clone(), (!new_branch.is_empty()).then_some(new_branch), dialog.base.clone()));
            path.to_string_lossy().into_owned()
        } else {
            folder.clone()
        };
        let id = if self.demo { format!("demo-{}", uuid::Uuid::new_v4()) } else { uuid::Uuid::new_v4().to_string() };
        let fork_from = dialog.parent.clone().filter(|p| dialog.fork && dialog.can_fork && self.can_fork(p));
        let sessions_dir = self.instance.as_ref().map(|i| i.sessions_dir.clone());
        let (x, y) = match (&dialog.parent, dialog.at) {
            (Some(parent), _) => match self.doc.node(parent) {
                Some(p) => self.doc.free_slot(p.x + GRID_X + 4, p.y, 1, NodeKind::Session),
                None => self.doc.free_slot(self.doc.view.x + 2, self.doc.view.y + 1, 4, NodeKind::Session),
            },
            (None, Some((x, y))) => {
                let rect = super::doc::Rect { x, y, w: super::doc::SESSION_W, h: super::doc::SESSION_H };
                if self.doc.is_free(&rect, "") { (x, y) } else { self.doc.free_slot(x, y, 1, NodeKind::Session) }
            }
            (None, None) => {
                let (n, d) = self.scale();
                let c = self.layout.canvas;
                let (cx, cy) = (self.doc.view.x + c.width as i32 * d / n / 3, self.doc.view.y + c.height as i32 * d / n / 3);
                self.doc.free_slot(cx, cy, 3, NodeKind::Session)
            }
        };
        let node = Node {
            id: id.clone(),
            kind: NodeKind::Session,
            name,
            x,
            y,
            cwd: Some(cwd),
            parent: dialog.parent.clone(),
            agent_key: None,
            source: Some("canvas".into()),
            command: self.demo.then(|| vec!["powershell.exe".into(), "-NoLogo".into(), "-NoProfile".into()]),
            // A fork resumes its own copy of the parent's history.
            fresh: !fork_from.is_some(),
            automation: None,
            synced_name: None, reviewed_ms: None,
            mcp_off: Some(super::doc::normalize_off(&dialog.mcp_off)),
            passive: None,
            no_messages: false,
        };
        self.doc.mcp.last_off = super::doc::normalize_off(&dialog.mcp_off);
        self.doc.nodes.push(node);
        self.doc.note_folder(&folder, now_ms());
        let prompt = dialog.prompt.text.trim();
        if !prompt.is_empty() {
            self.pending_prompts.insert(id.clone(), prompt.to_owned());
        }
        self.select_node(&id, false);
        self.reveal_node(&id);
        self.mark_dirty();
        // Forking copies the parent's history first (off the UI thread: it can be large).
        let fork_job = match (fork_from, sessions_dir) {
            (Some(parent), Some(dir)) => {
                let node = self.doc.node(&id).cloned();
                Some((dir, parent, node.as_ref().map(|n| n.name.clone()).unwrap_or_default(), node.and_then(|n| n.cwd)))
            }
            _ => None,
        };
        let fork = move |child: &str| -> Result<(), String> {
            match &fork_job {
                Some((dir, parent, name, cwd)) => crate::fork::fork_session(dir, parent, child, name, cwd.as_deref()).map_err(|e| format!("{e:#}")),
                None => Ok(()),
            }
        };
        let forking = dialog.fork && dialog.can_fork;
        match worktree_job {
            Some((root, path, new_branch, base)) => {
                self.preparing.insert(id.clone());
                self.notify("Creating the worktree… (large repositories can take a while)", false);
                let tx = self.app_tx.clone();
                std::thread::spawn(move || {
                    let result = git::add_worktree(&root, &path, new_branch.as_deref(), &base).map_err(|e| format!("{e:#}")).and_then(|_| fork(&id));
                    let _ = tx.send(AppEvent::Worktree { node: id, result });
                });
            }
            None if forking => {
                self.preparing.insert(id.clone());
                let tx = self.app_tx.clone();
                std::thread::spawn(move || {
                    let result = fork(&id);
                    let _ = tx.send(AppEvent::Forked { node: id, result });
                });
            }
            None => self.launch(&id),
        }
        Ok(())
    }

    // ---------- imports ----------

    pub fn copilot_import_candidates(&self, filter: &str) -> Vec<String> {
        let needle = filter.to_lowercase();
        let mut list: Vec<&crate::protocol::SessionSummary> = self
            .sessions
            .values()
            .filter(|s| self.doc.node(&s.id).is_none())
            .filter(|s| {
                needle.is_empty()
                    || s.name.to_lowercase().contains(&needle)
                    || s.cwd.to_lowercase().contains(&needle)
                    || s.branch.to_lowercase().contains(&needle)
                    || s.id.starts_with(&needle)
            })
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        list.into_iter().map(|s| s.id.clone()).collect()
    }

    pub fn list_dir(dir: &Option<PathBuf>) -> Result<Vec<String>, String> {
        match dir {
            None => {
                let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
                Ok((0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i) as char)).collect())
            }
            Some(dir) => {
                let mut names: Vec<String> = std::fs::read_dir(dir)
                    .map_err(|e| e.to_string())?
                    .filter_map(|e| e.ok())
                    .filter(|e| std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('$') && n != "System Volume Information")
                    .collect();
                names.sort_by_key(|n| n.to_lowercase());
                names.insert(0, "..".into());
                Ok(names)
            }
        }
    }

    pub fn open_folder_browser(&mut self, start: &str) {
        let start = PathBuf::from(start.trim());
        let dir = if start.is_dir() { Some(crate::paths::absolute(&start)) } else { dirs::home_dir() };
        let (entries, error) = match Self::list_dir(&dir) {
            Ok(entries) => (entries, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        self.modals.push(Modal::Folder(FolderBrowser { dir, entries, filter: String::new(), sel: 0, scroll: 0, error }));
    }

    pub fn folder_navigate(browser: &mut FolderBrowser, target: Option<PathBuf>) {
        browser.dir = target;
        browser.filter.clear();
        browser.sel = 0;
        browser.scroll = 0;
        match Self::list_dir(&browser.dir) {
            Ok(entries) => {
                browser.entries = entries;
                browser.error = None;
            }
            Err(error) => {
                browser.entries = vec!["..".into()];
                browser.error = Some(error);
            }
        }
    }
}
