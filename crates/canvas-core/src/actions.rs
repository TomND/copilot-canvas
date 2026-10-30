//! Commands triggered from keys, menus and buttons, plus background results.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agents::{AgentKind, agent_of, native_id};
use crate::app::{App, AppEvent, Focus};
use crate::dialogs::{
    Action, Confirm, FolderBrowser, GitState, Grouping, ImportCopilot, Menu, MenuItem, Modal, NewSession, NsField, Rename, Target,
    TextInput,
};
use crate::doc::{GRID_X, Node, NodeKind};
use crate::git;
use crate::paths::now_ms;
use crate::protocol::ClientMsg;
use crate::rpc::Backend;

fn item(label: &str, hint: &str, action: Action, enabled: bool) -> MenuItem {
    MenuItem { label: label.to_owned(), hint: hint.to_owned(), action, enabled }
}

impl App {
    pub fn open_menu(&mut self, x: u16, y: u16, (title, items): (String, Vec<MenuItem>)) {
        let sel = items.iter().position(|i| i.enabled).unwrap_or(0);
        self.modals.push(Modal::Menu(Menu { x, y, title, items, sel }));
    }

    /// The menu's remove item: this box alone, or (on a box of a
    /// multi-selection) the selection's eligible boxes after a confirmation.
    fn remove_item(&self, id: &str, single: &str) -> MenuItem {
        if self.selection.len() > 1 && self.selection.iter().any(|s| s == id) {
            let count = self.remove_plan(&self.selection).remove.len();
            return item(&format!("Remove {count} from canvas"), "Del", Action::ConfirmRemove(self.selection.clone()), count > 0);
        }
        item(single, "Del", Action::RemoveNode(id.to_owned()), true)
    }

    pub fn node_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        let Some(node) = self.doc.node(id) else { return (String::new(), Vec::new()) };
        let session = self.session_of(id).unwrap_or_default();
        let alive = self.owned_alive(&session);
        let external = self.summary(node).is_some_and(|s| s.live) && !alive;
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
            let parent = node.parent.clone().unwrap_or_default();
            let parent_alive = self.owned_alive(&parent);
            items.push(item("Inspect transcript", "", Action::Inspect(id.to_owned()), self.transcript_request(id).is_some()));
            if self.doc.node(&parent).is_some_and(|p| p.command.is_none() && agent_of(p) == AgentKind::OpenCode) {
                let resumable = !matches!(self.child_resume(node), crate::inspect::ChildResume::No(_));
                items.push(item("Resume as subsession", "", Action::ResumeChild(id.to_owned()), resumable));
            }
            items.push(item(if parent_alive { "Open parent terminal" } else { "Start parent session" }, "", Action::Start(parent), !external));
            items.push(item("Rename box", "R", Action::Rename(Target::Node(id.to_owned())), true));
            items.push(self.remove_item(id, "Remove box"));
            return (node.name.clone(), items);
        }
        if node.passive.is_some() {
            let paused = node.passive.as_ref().is_some_and(|p| p.paused);
            let running = self.passive_view(node).is_some_and(|v| v.running);
            items.push(item("Run now", "", Action::PassiveRun(id.to_owned()), !external));
            items.push(item(if paused { "Resume" } else { "Pause" }, "", Action::PassivePause(id.to_owned()), true));
            items.push(item("Settings…", "", Action::PassiveDetail(id.to_owned()), true));
            items.push(item("Message…", "", Action::MessagePassive(id.to_owned()), true));
            items.push(item("Edit rules", "", Action::PassiveOpen(id.to_owned(), 0), true));
            items.push(item("Open run log", "", Action::PassiveOpen(id.to_owned(), 2), true));
            items.push(item(if alive { "Focus chat" } else { "Open chat" }, "", Action::PassiveChat(id.to_owned()), !external));
            items.push(item("Inspect transcript", "", Action::Inspect(id.to_owned()), self.transcript_request(id).is_some()));
            if node.is_canvas_agent() {
                items.extend(self.agent_menu_items(node));
            }
            items.push(item("MCP servers…", "", Action::McpServers(id.to_owned()), true));
            items.push(item("Rename", "R", Action::Rename(Target::Node(id.to_owned())), true));
            items.push(item("Stop run", "X", Action::Agents(crate::canvas_wide::AgentAction::Stop(id.to_owned())), alive || running));
            items.push(self.remove_item(id, "Remove from canvas"));
            return (node.name.clone(), items);
        }
        // The explicit start: Start (new), Resume (its conversation) or Open terminal (running).
        let (start, startable) = match self.start_plan(node).label() {
            Some(label) => (label, true),
            None => ("Open terminal", false),
        };
        items.push(item(start, "Enter", Action::Start(id.to_owned()), startable));
        if self.transcript_request(id).is_some() {
            items.push(item("Inspect transcript", "", Action::Inspect(id.to_owned()), true));
        }
        items.push(item("New subsession", "S", Action::NewSession { at: None, parent: Some(id.to_owned()) }, true));
        items.extend(self.visual_node_items(id));
        items.push(item("Rename", "R", Action::Rename(Target::Node(id.to_owned())), true));
        if !self.demo {
            items.push(item("MCP servers…", "", Action::McpServers(id.to_owned()), true));
            items.push(item(if node.no_messages { "Accept passive messages" } else { "Mute passive messages" }, "", Action::ToggleMessages(id.to_owned()), true));
            let queued = self.bus.mailbox.get(id).copied().unwrap_or(0);
            if queued > 0 {
                items.push(item(&format!("Deliver {queued} queued message(s)"), "", Action::Start(id.to_owned()), !external));
                items.push(item("Discard queued messages", "", Action::DiscardMail(id.to_owned()), true));
            }
        }
        let group_label = if self.selection.len() > 1 { "Group selected sessions" } else { "New group around this" };
        items.push(item(group_label, "G", Action::GroupSelection, true));
        if !self.demo {
            items.push(item("Copy session ID", "", Action::Copy(native_id(node).to_owned()), true));
        }
        items.push(item("Stop session", "X", Action::Stop { id: id.to_owned(), force: false }, alive));
        items.push(item("Force stop", "", Action::Stop { id: id.to_owned(), force: true }, alive));
        items.push(self.remove_item(id, "Remove from canvas"));
        (node.name.clone(), items)
    }

    pub fn group_menu(&self, id: &str) -> (String, Vec<MenuItem>) {
        if self.doc.group(id).is_some_and(|g| g.is_canvas_agents()) {
            return self.frame_menu(id);
        }
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
        let mut items = vec![
            item("New session here", "N", Action::NewSession { at: Some((wx, wy)), parent: None }, true),
            item("New group here", "", Action::NewGroupAt(wx, wy), true),
            item("Import sessions…", "I", Action::ImportCopilot, !self.demo),
            item("Sync VS Code sessions", "V", Action::SyncVs, !self.demo),
            item(self.undo_label(), "Ctrl+Z", Action::UndoSync, self.undo.is_some()),
            item("Migrate VS Code skills & settings…", "", Action::Migrate, !self.demo),
            item("Fit everything", "F", Action::FitAll, true),
            item(if self.doc.ui.tiled { "Single terminal view" } else { "Tile terminals" }, "T", Action::ToggleTiled, true),
            item("Open in the other client", "O", Action::OpenOtherClient, true),
            item("Help", "?", Action::Help, true),
        ];
        items.splice(items.len() - 3..items.len() - 3, self.visual_canvas_items());
        ("Canvas".into(), items)
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
    /// Copilot's scrollbar column is left out (see `CopyRules`).
    pub fn copy_selection(&mut self) {
        let Some(selection) = self.term_select.clone() else { return };
        let rules = canvas_termview::CopyRules { edge_scrollbar: self.runs_copilot(&selection.id) };
        let Some(text) = self.views.get(&selection.id).map(|v| v.selection_text_with(&selection, rules)) else { return };
        if text.is_empty() {
            return;
        }
        match self.platform.set_clipboard_text(&text) {
            Ok(()) => self.notify(format!("Copied {} characters", text.chars().count()), false),
            Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
        }
    }

    /// The terminal runs Copilot CLI (as for its user-message tint).
    pub fn runs_copilot(&self, id: &str) -> bool {
        let agent = self.doc.node(id).filter(|n| n.command.is_none()).map(crate::agents::agent_of);
        self.terms.get(id).is_some_and(|info| info.copilot) && agent == Some(crate::agents::AgentKind::Copilot)
    }

    /// Pastes the clipboard's text into a terminal (an image: Ctrl+V for the agent).
    pub fn paste_clipboard(&mut self, id: &str) {
        if !self.owned_alive(id) {
            return;
        }
        // Like Windows Terminal: text is pasted; anything else (an image, copied
        // files) sends Ctrl+V itself, and the agent (Copilot, Claude Code,
        // OpenCode) reads the clipboard to attach it.
        if !self.platform.clipboard_has_text() {
            return self.send_ctrl_v(id);
        }
        match self.platform.clipboard_text() {
            Ok(text) if !text.is_empty() => self.paste_text(id, &text),
            Ok(_) => self.send_ctrl_v(id),
            Err(error) => self.notify(format!("Paste failed: {error:#}"), true),
        }
    }

    /// Text pasted into a terminal: one paste, never ending with a line
    /// break (the service encodes it so no line break acts as Enter; see
    /// `canvas_host::keys::paste_lines`). Line breaks alone paste nothing.
    pub fn paste_text(&mut self, id: &str, text: &str) {
        let text = crate::input::paste_body(text);
        if !text.is_empty() {
            self.send(ClientMsg::Input { id: id.to_owned(), input: crate::protocol::Input::Paste(text.to_owned()) });
        }
    }

    pub fn send_ctrl_v(&mut self, id: &str) {
        let input = crate::protocol::Input::Key { key: crate::protocol::Key::Char('v'), mods: crate::protocol::MOD_CTRL };
        self.paste_key(id, input);
    }

    /// Sends a paste key (Ctrl+V, Alt+V) so the agent reads the clipboard
    /// itself. When the service runs on another machine, an image on this
    /// machine's clipboard is first put on the service's clipboard (PLAN.md §6).
    pub fn paste_key(&mut self, id: &str, input: crate::protocol::Input) {
        if !self.owned_alive(id) {
            return;
        }
        if !self.files_elsewhere() || !self.platform.clipboard_has_image() {
            return self.send(ClientMsg::Input { id: id.to_owned(), input });
        }
        if !self.connected {
            return self.notify("Offline: input not sent.", true);
        }
        self.notify("Sending the image to the service's clipboard…", false);
        let (platform, id) = (self.platform.clone(), id.to_owned());
        self.in_background(
            move |backend| {
                let png = platform.clipboard_image_png().map_err(|e| format!("{e:#}"))?;
                let size = png.len();
                backend.set_clipboard_image(png).map(|_| size).map_err(|e| format!("{e:#}"))
            },
            move |result| {
                AppEvent::Run(Box::new(move |app: &mut App| match result {
                    Ok(size) if app.owned_alive(&id) => {
                        app.send(ClientMsg::Input { id, input });
                        app.notify(format!("Image pasted ({} KB, through the service's clipboard).", size.div_ceil(1024)), false);
                    }
                    Ok(_) => {}
                    Err(error) => app.notify(format!("Image paste failed: {error}"), true),
                }))
            },
        );
    }

    // ---------- links, files and folders (local and remote) ----------

    /// The service runs on another machine: paths in terminals, automation
    /// files and folders are that machine's, not this one's.
    pub fn files_elsewhere(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.is_other_machine())
    }

    /// Opens a link from a terminal, a passive session or an events catalog.
    /// Locally: its handler on this machine. Remote: URLs on this machine;
    /// paths in VS Code through the server's VS Code tunnel, else in the
    /// viewer. Nothing is executed.
    pub fn open_link(&mut self, target: &str) {
        if self.files_elsewhere() && !crate::links::is_url(target) {
            return self.open_remote(target, false);
        }
        match self.platform.open_link(target) {
            Ok(message) => self.notify(message, false),
            Err(error) => self.notify(format!("{error:#}"), true),
        }
    }

    /// Opens an automation output, log, manifest or report link. Locally: its
    /// default program where the backend runs. Remote: URLs on this machine,
    /// files in the viewer (with "Open in VS Code" when there is a tunnel).
    pub fn open_target(&mut self, target: &str) {
        if !self.files_elsewhere() {
            if let Err(error) = self.backend.open_target(target) {
                self.notify(format!("{error:#}"), true);
            }
        } else if crate::links::is_url(target) {
            self.open_link(target);
        } else {
            self.open_remote(target, true);
        }
    }

    /// Shows a folder. Locally: File Explorer where the backend runs. Remote
    /// (any `--remote`, loopback included): VS Code through the tunnel, else
    /// the canvas's folder browser (with "Copy path").
    pub fn open_folder(&mut self, path: &str) {
        if self.service_is_remote() {
            return self.open_remote(path, false);
        }
        if let Err(error) = self.backend.open_folder(path) {
            self.notify(format!("{error:#}"), true);
        }
    }

    /// A path on the service's machine: in VS Code through its tunnel
    /// (unless `view`), else in the viewer.
    pub(crate) fn open_remote(&mut self, target: &str, view: bool) {
        let (path, line) = match crate::links::remote_path(target) {
            Ok(found) => found,
            Err(error) => return self.notify(format!("{error:#}"), true),
        };
        let info = match self.backend.path_info(&path) {
            Ok(info) => info,
            Err(error) => return self.notify(format!("Cannot check {path}: {error:#}"), true),
        };
        if !info.exists {
            return self.notify(format!("{path} does not exist"), true);
        }
        let path = if info.absolute.is_empty() { path } else { info.absolute };
        let line = line.filter(|_| !info.dir);
        let uri = self.target.as_ref().and_then(|t| t.vscode_tunnel()).map(|tunnel| crate::links::vscode_tunnel_uri(&tunnel, &path, line));
        if !view
            && let Some(uri) = &uri
            && crate::links::vscode_installed()
        {
            match self.platform.open_uri(uri) {
                Ok(()) => self.notify(format!("Opened {path} in VS Code (tunnel)"), false),
                Err(error) => self.notify(format!("{error:#}"), true),
            }
            return;
        }
        self.show_viewer(path, info.dir, line, uri);
    }

    /// Opens the read-only viewer on a path of the service's machine and
    /// loads it in the background.
    pub fn show_viewer(&mut self, path: String, folder: bool, line: Option<u32>, vscode: Option<String>) {
        self.modals.push(Modal::Viewer(Box::new(crate::viewer::Viewer::new(path.clone(), folder, line, vscode))));
        self.needs_draw = true;
        let key = path.clone();
        self.in_background(
            move |backend| match folder {
                true => crate::folders::list_entries(backend, &path).map(Err),
                false => backend.read_text(&path, crate::viewer::MAX_VIEW_BYTES).map(Ok).map_err(|e| format!("{e:#}")),
            },
            move |loaded| {
                AppEvent::Run(Box::new(move |app: &mut App| {
                    for modal in app.modals.iter_mut() {
                        if let Modal::Viewer(viewer) = modal
                            && viewer.path == key
                            && viewer.lines.is_none()
                        {
                            match &loaded {
                                Ok(Ok(text)) => viewer.set_text(text, text.len() as u64 >= crate::viewer::MAX_VIEW_BYTES),
                                Ok(Err((entries, typed))) => viewer.set_listing(entries.clone(), *typed),
                                Err(error) => viewer.error = Some(format!("Cannot read it: {error}")),
                            }
                        }
                    }
                }))
            },
        );
    }

    /// A viewer button; true closes the viewer.
    pub fn viewer_button(&mut self, viewer: &crate::viewer::Viewer, button: &str) -> bool {
        match button {
            crate::viewer::OPEN_IN_VSCODE => {
                if let Some(uri) = &viewer.vscode {
                    match self.platform.open_uri(uri) {
                        Ok(()) => self.notify(format!("Opened {} in VS Code (tunnel)", viewer.path), false),
                        Err(error) => self.notify(format!("{error:#}"), true),
                    }
                }
                false
            }
            crate::viewer::COPY_PATH => {
                match self.platform.set_clipboard_text(&viewer.path) {
                    Ok(()) => self.notify(format!("Copied {}", viewer.path), false),
                    Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
                }
                false
            }
            _ => true,
        }
    }

    pub fn confirm_stop(&mut self, id: String) {
        let name = self.doc.node(&id).map(|n| n.name.clone()).unwrap_or_else(|| id.clone());
        let agent = self.doc.node(&id).map(agent_of).unwrap_or_default();
        self.modals.push(Modal::Confirm(Confirm {
            title: format!("Stop “{name}”?"),
            body: vec![format!("{} exits gracefully; its conversation stays resumable.", agent.label())],
            buttons: vec![
                ("Stop".into(), Action::Stop { id: id.clone(), force: false }),
                ("Force stop".into(), Action::Stop { id, force: true }),
                ("Cancel".into(), Action::Nothing),
            ],
            sel: 0,
        }));
    }

    /// A session whose conversation can be forked: Copilot copies its history
    /// on disk; OpenCode and Claude Code fork natively once it has history.
    pub fn can_fork(&self, id: &str) -> bool {
        let Some(node) = self.doc.node(id).filter(|n| n.kind == NodeKind::Session && n.command.is_none()) else { return false };
        let kind = agent_of(node);
        let caps = kind.caps();
        let native = native_id(node);
        if caps.copy_fork {
            kind.is_session_id(native) && self.backend.has_history(kind, native).unwrap_or(false)
        } else {
            caps.native_fork && kind.is_session_id(native) && self.sessions.contains_key(native)
        }
    }

    /// Opens the MCP server checklist for a new session or an existing one.
    pub fn open_mcp_picker(&mut self, target: crate::dialogs::McpTarget) {
        let (agent, off) = match &target {
            crate::dialogs::McpTarget::NewSession => match self.modals.last() {
                Some(Modal::NewSession(dialog)) => (dialog.agent, dialog.mcp_off.clone()),
                _ => (self.default_agent(), self.doc.mcp.last_off.clone()),
            },
            crate::dialogs::McpTarget::Node(id) => {
                let node = self.doc.node(id);
                (node.map(agent_of).unwrap_or_default(), node.and_then(|n| n.mcp_off.clone()).unwrap_or_default())
            }
        };
        if !self.agent_configs.contains_key(&agent) {
            self.refresh_config_for(agent);
        }
        self.modals.push(Modal::Mcp(crate::dialogs::McpPicker { target, agent, off, sel: 0 }));
        self.needs_draw = true;
    }

    /// Applies a picker's choice to the new-session dialog under it or to a session.
    pub fn apply_mcp_choice(&mut self, target: &crate::dialogs::McpTarget, agent: AgentKind, off: Vec<String>) {
        let off = crate::doc::normalize_off(&off);
        match target {
            crate::dialogs::McpTarget::NewSession => {
                if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
                    dialog.mcp_off = off;
                }
            }
            crate::dialogs::McpTarget::Node(id) => {
                let alive = self.owned_alive(id);
                if let Some(node) = self.doc.node_mut(id) {
                    node.mcp_off = Some(off.clone());
                }
                self.mark_dirty();
                let summary = self.mcp_summary(agent, &off);
                let when = if alive { " Applies the next time it starts (stop it and double-click it to restart now)." } else { "" };
                self.notify(format!("MCP servers for this session: {summary}.{when}"), false);
            }
        }
    }

    pub fn confirm_update_service(&mut self) {
        let alive: Vec<&String> = self.terms.iter().filter(|(_, t)| t.alive).map(|(id, _)| id).collect();
        let working = alive.iter().filter(|id| self.doc.node(id).is_some_and(|n| self.node_status(n) == crate::status::Status::Working)).count();
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
        if self.restarting.is_some() || self.conn.remote {
            // Remote clients never restart or upgrade the service.
            return;
        }
        let resume: Vec<String> = self.terms.iter().filter(|(id, t)| t.alive && self.doc.node(id).is_some()).map(|(id, _)| id.clone()).collect();
        let demo = self.demo;
        // Only Copilot has the mouse setting an old service could switch off.
        let copilot = self.installed_agents.contains(&AgentKind::Copilot);
        self.notify(format!("Restarting the terminal service… {} session(s) will resume.", resume.len()), false);
        self.restarting = Some(resume);
        self.send(ClientMsg::Shutdown);
        let (tx, backend) = (self.app_tx.clone(), self.backend.clone());
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && crate::conn::try_connect(&instance).is_ok() {
                std::thread::sleep(Duration::from_millis(300));
            }
            let mouse = !demo && copilot && backend.restore_mouse().unwrap_or(false);
            let result = crate::conn::connect(&instance, true).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::ServiceRestarted { result, mouse });
        });
    }

    fn on_service_restarted(&mut self, result: Result<crate::conn::Conn, String>, mouse: bool) {
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
        // The new service opens the document again (local changes are kept).
        self.request_doc();
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
                self.view_node(&id);
            }
            Action::Start(id) => self.start_node(&id),
            Action::Inspect(id) => self.inspect(&id),
            Action::ResumeChild(id) => self.resume_child(&id),
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
                self.doc.groups.push(crate::doc::Group { id: id.clone(), name: "New group".into(), x, y, w: 2 * GRID_X + 4, h: 12, color, folder: None, source: None, defaults: None });
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
            Action::RemoveNode(id) => self.remove_nodes(&[id]),
            Action::RemoveNodes(ids) => self.remove_nodes(&ids),
            Action::ConfirmRemove(ids) => self.confirm_remove(ids),
            Action::Stop { id, force } => {
                if self.owned_alive(&id) {
                    self.stop_terminal(&id, force);
                    self.notify(if force { "Force stopping…" } else { "Stopping (the agent exits gracefully)…" }, false);
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
            Action::RemoveConfig(kind, name) => self.remove_config(kind, name),
            Action::ToggleAutomation(key) => self.toggle_automation(&key),
            Action::RunAutomation(key) => self.run_automation(&key),
            Action::OpenAutomationFile(key, which) => self.open_automation_file(&key, which),
            Action::OpenTarget(target) => self.open_target(&target),
            Action::AskDoctor(key) => self.ask_doctor(&key),
            Action::ShowAutomations => self.set_view(crate::side::SideView::Automations),
            Action::ToggleConfig(kind, name, enable) => self.set_config_enabled(kind, name, enable),
            Action::UpdatePlugin(name) => {
                if self.config_agent != AgentKind::Copilot {
                    return self.notify(format!("Not supported for {}.", self.config_agent.label()), true);
                }
                let label = format!("Updating plugin {name}");
                self.run_cli_op(label, move |backend| backend.plugin_update(&name));
            }
            Action::CloseDetail => {
                // A session's details close with its selection (they would show again).
                let session = matches!(self.right_detail, None | Some(crate::side::RightDetail::Session(_)));
                self.right_detail = None;
                let keep: Vec<String> = if session {
                    Vec::new()
                } else {
                    self.selection.iter().filter(|id| self.doc.node(id).is_some_and(|n| n.kind != NodeKind::Automation)).cloned().collect()
                };
                self.selection = keep;
            }
            Action::OpenPath(path) => self.open_folder(&path),
            Action::SetGroupFolder(id) => self.ask_group_folder(&id),
            Action::OpenGroupFolder(id, outputs) => self.open_group_folder(&id, outputs),
            Action::GroupDefaults(id) => {
                self.settings_group = Some(id);
                self.set_view(crate::side::SideView::Settings);
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
            Action::McpServers(id) => self.open_mcp_picker(crate::dialogs::McpTarget::Node(id)),
            Action::NewPassive(group) => self.new_passive(&group),
            Action::PassiveDetail(id) => {
                self.select_node(&id, false);
                self.right_detail = Some(crate::side::RightDetail::Passive(id));
            }
            Action::PassiveChat(id) => {
                if self.owned_alive(&id) {
                    self.show_terminal(&id);
                    self.set_focus(Focus::Terminal);
                } else if self.doc.node(&id).and_then(|n| self.summary(n)).is_some_and(|s| s.live) {
                    self.notify("This passive session is running elsewhere right now.", true);
                } else {
                    self.launch(&id);
                }
            }
            Action::PassiveRun(id) => {
                if let Some(node) = self.doc.node(&id).filter(|n| n.is_canvas_agent()).cloned() {
                    match self.canvas_agent_state(&node) {
                        crate::canvas_wide::AgentState::Off => return self.notify("It is disabled: enable it first.", true),
                        crate::canvas_wide::AgentState::Unavailable(why) => return self.notify(format!("It cannot run: {why}"), true),
                        crate::canvas_wide::AgentState::Paused => return self.notify("It is paused: resume it first (Run now waits until then).", true),
                        _ => {}
                    }
                }
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
                    match which {
                        3 => self.open_folder(&path.display().to_string()),
                        _ => self.open_service_file(&path.display().to_string()),
                    }
                }
            }
            Action::SendEvent(group) => self.modals.push(Modal::Prompt(crate::dialogs::Prompt {
                title: "Send an event to this group's passive sessions".into(),
                hint: "type [json data], for example: pr.merged {\"pr\": 164587}".into(),
                input: TextInput::default(),
                purpose: crate::dialogs::PromptPurpose::SendEvent(group),
            })),
            Action::OpenEvents(group) => match self.doc.group(&group).and_then(|g| g.folder.clone()) {
                Some(folder) => match self.backend.ensure_catalog(&folder) {
                    Ok(path) => self.open_service_file(&path),
                    Err(error) => self.notify(format!("{error:#}"), true),
                },
                None => self.notify("Set the group's folder first (group menu → Change group folder…).", true),
            },
            Action::MessagePassive(id) => self.modals.push(Modal::Prompt(crate::dialogs::Prompt {
                title: format!("Message {}", self.doc.node(&id).map(|n| n.name.clone()).unwrap_or_default()),
                hint: "It runs with your message (as a \"message\" event).".into(),
                input: TextInput::default(),
                purpose: crate::dialogs::PromptPurpose::MessagePassive(id),
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
                    self.stop_terminal(&id, false);
                }
                self.quit = true;
            }
            Action::Help => self.modals.push(Modal::Help),
            Action::Nothing => {}
            Action::NewWindow => self.new_window_requested = true,
            Action::Visual(action) => self.run_visual(action),
            Action::Agents(action) => self.run_agents(action),
            Action::Report(action) => self.report_action(action),
            Action::Prs(action) => self.run_prs(action),
            Action::Reviews(action) => self.run_reviews(action),
            Action::OpenOtherClient => {
                let args = self.other_client_args();
                match self.platform.open_other_client(&args) {
                    Ok(status) => self.notify(status, false),
                    Err(error) => self.notify(format!("Cannot open the other client: {error:#}"), true),
                }
            }
        }
    }

    // ---------- new session / subsession ----------

    pub(crate) fn open_new_session(&mut self, at: Option<(i32, i32)>, parent: Option<String>) {
        // A subsession runs its parent's agent unless you choose another.
        let agent = parent.as_deref().and_then(|p| self.doc.node(p)).filter(|n| n.command.is_none()).map(agent_of).unwrap_or_else(|| self.default_agent());
        let can_fork = !self.demo && parent.as_deref().is_some_and(|p| self.can_fork(p));
        let folder = parent
            .as_ref()
            .and_then(|p| self.doc.node(p))
            .and_then(|n| n.cwd.clone().or_else(|| self.summary(n).map(|s| s.cwd.clone())))
            .filter(|c| !c.is_empty())
            .or_else(|| self.doc.recent_folders.first().map(|f| f.path.clone()))
            .unwrap_or_else(|| self.default_cwd.clone());
        let (x, y) = self.new_session_slot(parent.as_deref(), at);
        let group = self.doc.group_at(x + crate::doc::SESSION_W / 2, y + crate::doc::SESSION_H / 2).map(|g| g.id.clone());
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
            agent,
            model: None,
            group,
        };
        if !self.agent_configs.contains_key(&agent) {
            self.refresh_config_for(agent);
        }
        self.modals.push(Modal::NewSession(Box::new(dialog)));
    }

    /// Where a new session box goes: next to its parent, at the spot given
    /// (or the nearest free one), else in free space in view.
    fn new_session_slot(&self, parent: Option<&str>, at: Option<(i32, i32)>) -> (i32, i32) {
        match (parent, at) {
            (Some(parent), _) => match self.doc.node(parent) {
                Some(p) => self.doc.free_slot(p.x + GRID_X + 4, p.y, 1, NodeKind::Session),
                None => self.doc.free_slot(self.doc.view.x + 2, self.doc.view.y + 1, 4, NodeKind::Session),
            },
            (None, Some((x, y))) => {
                let rect = crate::doc::Rect { x, y, w: crate::doc::SESSION_W, h: crate::doc::SESSION_H };
                if self.doc.is_free(&rect, "") { (x, y) } else { self.doc.free_slot(x, y, 1, NodeKind::Session) }
            }
            (None, None) => {
                let (n, d) = self.scale();
                let c = self.layout.canvas;
                let (cx, cy) = (self.doc.view.x + c.width as i32 * d / n / 3, self.doc.view.y + c.height as i32 * d / n / 3);
                self.doc.free_slot(cx, cy, 3, NodeKind::Session)
            }
        }
    }

    /// Switches the new-session dialog to the next agent (`forward`) or the previous one.
    pub fn cycle_dialog_agent(&mut self, dialog: &mut NewSession, forward: bool) {
        let all = crate::agents::ALL;
        let index = all.iter().position(|k| *k == dialog.agent).unwrap_or(0);
        dialog.agent = all[if forward { (index + 1) % all.len() } else { (index + all.len() - 1) % all.len() }];
        // Model ids are the agent's own: a choice made for another agent does not carry over.
        dialog.model = None;
        // Forking needs the parent's own agent.
        let parent_agent = dialog.parent.as_deref().and_then(|p| self.doc.node(p)).map(agent_of);
        dialog.can_fork = !self.demo && parent_agent == Some(dialog.agent) && dialog.parent.as_deref().is_some_and(|p| self.can_fork(p));
        dialog.fork = dialog.can_fork;
        if !self.agent_configs.contains_key(&dialog.agent) {
            self.refresh_config_for(dialog.agent);
        }
        dialog.error = (!self.demo && !self.installed_agents.contains(&dialog.agent)).then(|| format!("{} was not found on this machine.", dialog.agent.label()));
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
        self.git_req += 1;
        let req = self.git_req;
        dialog.git = GitState::Checking(req);
        let probed = folder.clone();
        self.in_background(
            move |backend| match backend.path_info(&folder) {
                Ok(info) if !info.dir => Err("folder not found".to_owned()),
                _ => backend.git_probe(&folder).map_err(|e| format!("{e:#}")),
            },
            move |result| AppEvent::Git { req, folder: probed, result },
        );
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
                        self.notify(format!("Could not fork the parent's conversation: {error}. Double-click the box to start it fresh."), true);
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
            AppEvent::Config(kind, snapshot) => self.on_config(kind, *snapshot),
            AppEvent::Automations(load) => self.on_automations(load),
            AppEvent::AutomationsFailed(error) => {
                self.automations_loading = false;
                self.automations_error = Some(error);
            }
            AppEvent::Tails(tails) => {
                self.tails_loading = false;
                self.tails = tails.into_iter().collect();
            }
            AppEvent::Notice { text, error } => self.notify(text, error),
            AppEvent::Run(then) => then(self),
            AppEvent::Uploaded { term, paths, errors } => self.on_uploaded(&term, paths, errors),
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
            AppEvent::Reconnected(result) => self.on_reconnected(result),
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
                // Migration changes Copilot's configuration.
                self.agent_configs.remove(&AgentKind::Copilot);
                if self.config_agent == AgentKind::Copilot {
                    self.config = None;
                }
                self.refresh_config_for(AgentKind::Copilot);
            }
        }
    }

    /// Validates the dialog and creates the session (and worktree if requested).
    pub fn submit_new_session(&mut self, dialog: &NewSession) -> Result<(), String> {
        if !self.demo && !self.installed_agents.contains(&dialog.agent) {
            return Err(format!("{} was not found on this machine; choose another agent.", dialog.agent.label()));
        }
        let folder = dialog.folder.text.trim().trim_matches('"').to_owned();
        let found = self.backend.path_info(&folder).map_err(|e| format!("{e:#}"))?;
        if !found.dir {
            return Err(format!("Folder not found: {folder}"));
        }
        let folder = found.absolute;
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
            if self.backend.path_info(&path.to_string_lossy()).map_err(|e| format!("{e:#}"))?.occupied {
                return Err(format!("{} already exists and is not empty.", path.display()));
            }
            worktree_job = Some((info.root.to_string_lossy().into_owned(), path.to_string_lossy().into_owned(), (!new_branch.is_empty()).then_some(new_branch), dialog.base.clone()));
            path.to_string_lossy().into_owned()
        } else {
            folder.clone()
        };
        let id = if self.demo { format!("demo-{}", uuid::Uuid::new_v4()) } else { uuid::Uuid::new_v4().to_string() };
        let agent = dialog.agent;
        let caps = agent.caps();
        let fork_parent = dialog
            .parent
            .clone()
            .filter(|p| dialog.fork && dialog.can_fork && self.can_fork(p) && self.doc.node(p).map(agent_of) == Some(agent))
            .and_then(|p| self.doc.node(&p).map(|n| native_id(n).to_owned()));
        // Copilot forks by copying the parent's history; the others fork it themselves on first start.
        let (copy_fork, native_fork) = if caps.copy_fork { (fork_parent, None) } else { (None, fork_parent) };
        let (x, y) = self.new_session_slot(dialog.parent.as_deref(), dialog.at);
        let mut node = Node {
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
            // A copied fork resumes its own copy of the parent's history.
            fresh: copy_fork.is_none(),
            automation: None,
            synced_name: None, reviewed_ms: None,
            mcp_off: Some(crate::doc::normalize_off(&dialog.mcp_off)),
            passive: None,
            no_messages: false,
            agent: (!self.demo).then(|| agent.id().to_owned()),
            agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        };
        // The model shown in the dialog is the one it starts with (and keeps on
        // resume); a choice made there becomes its agent's default.
        let group = self.doc.group_of(&node).cloned();
        let (model, remember) = self.new_session_model(dialog, group.as_ref());
        node.model = model;
        if let Some(choice) = remember {
            self.doc.defaults.set_model(agent, Some(&choice));
            self.notify(format!("New {} sessions now start with {}.", agent.label(), crate::models::shown(&choice)), false);
        }
        self.doc.mcp.last_off = crate::doc::normalize_off(&dialog.mcp_off);
        self.doc.nodes.push(node);
        self.doc.note_folder(&folder, now_ms());
        let prompt = dialog.prompt.text.trim();
        if !prompt.is_empty() {
            self.pending_prompts.insert(id.clone(), prompt.to_owned());
        }
        if let Some(parent) = native_fork {
            self.pending_forks.insert(id.clone(), parent);
        }
        self.select_node(&id, false);
        self.reveal_node(&id);
        self.mark_dirty();
        // Forking copies the parent's history first (off the UI thread: it can be large).
        let fork_job = copy_fork.map(|parent| {
            let node = self.doc.node(&id).cloned();
            (parent, node.as_ref().map(|n| n.name.clone()).unwrap_or_default(), node.and_then(|n| n.cwd))
        });
        let forking = fork_job.is_some();
        let fork = move |backend: &dyn Backend, child: &str| -> Result<(), String> {
            match &fork_job {
                Some((parent, name, cwd)) => backend.fork(parent, child, name, cwd.as_deref()).map_err(|e| format!("{e:#}")),
                None => Ok(()),
            }
        };
        match worktree_job {
            Some((root, path, new_branch, base)) => {
                self.preparing.insert(id.clone());
                self.notify("Creating the worktree… (large repositories can take a while)", false);
                let node = id.clone();
                self.in_background(
                    move |backend| backend.worktree_add(&root, &path, new_branch.as_deref(), &base).map_err(|e| format!("{e:#}")).and_then(|_| fork(backend, &id)),
                    move |result| AppEvent::Worktree { node, result },
                );
            }
            None if forking => {
                self.preparing.insert(id.clone());
                let node = id.clone();
                self.in_background(move |backend| fork(backend, &id), move |result| AppEvent::Forked { node, result });
            }
            None => self.launch(&id),
        }
        Ok(())
    }

    // ---------- imports ----------

    pub fn copilot_import_candidates(&self, filter: &str) -> Vec<String> {
        let needle = filter.to_lowercase();
        let present = self.native_ids();
        let mut list: Vec<&crate::protocol::SessionSummary> = self
            .sessions
            .values()
            .filter(|s| !present.contains(s.id.as_str()))
            .filter(|s| {
                needle.is_empty()
                    || s.name.to_lowercase().contains(&needle)
                    || s.cwd.to_lowercase().contains(&needle)
                    || s.branch.to_lowercase().contains(&needle)
                    || crate::app::client_label(s).to_lowercase().contains(&needle)
                    || s.id.starts_with(&needle)
            })
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        list.into_iter().map(|s| s.id.clone()).collect()
    }

    /// Folders in `dir` (the drives when `None`), on the backend's machine.
    pub fn list_dir(backend: &dyn Backend, dir: &Option<PathBuf>) -> Result<Vec<String>, String> {
        backend.list_dir(dir.as_ref().map(|d| d.display().to_string())).map_err(|e| format!("{e:#}"))
    }

    pub fn open_folder_browser(&mut self, start: &str) {
        let start = self.backend.path_info(start.trim()).ok().filter(|info| info.dir);
        let dir = match start {
            Some(info) => Some(PathBuf::from(info.absolute)),
            None => (!self.info.user_home.is_empty()).then(|| PathBuf::from(&self.info.user_home)),
        };
        let (entries, error) = match Self::list_dir(&*self.backend, &dir) {
            Ok(entries) => (entries, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        self.modals.push(Modal::Folder(FolderBrowser { dir, entries, filter: String::new(), sel: 0, scroll: 0, error }));
    }

    pub fn folder_navigate(backend: &dyn Backend, browser: &mut FolderBrowser, target: Option<PathBuf>) {
        browser.dir = target;
        browser.filter.clear();
        browser.sel = 0;
        browser.scroll = 0;
        match Self::list_dir(backend, &browser.dir) {
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

#[cfg(test)]
mod remote_tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::{Conn, ConnTarget};
    use crate::protocol::{Input, Key, MOD_ALT, MOD_CTRL, TermInfo};
    use crate::rpc::{PathInfo, Request, Response};
    use crate::sync::Opened;

    /// The service's machine: one file, one folder; records clipboard images.
    #[derive(Default)]
    struct Service {
        images: Mutex<Vec<Vec<u8>>>,
    }

    impl Backend for Service {
        fn call(&self, request: Request) -> anyhow::Result<Response> {
            Ok(match request {
                Request::PathInfo { path } if path.ends_with("notes.md") => Response::Path(PathInfo { exists: true, file: true, absolute: path, ..Default::default() }),
                Request::PathInfo { path } if path.ends_with("src") => Response::Path(PathInfo { exists: true, dir: true, absolute: path, ..Default::default() }),
                Request::PathInfo { .. } => Response::Path(PathInfo::default()),
                Request::ReadText { .. } => Response::Text("# Notes\nREMOTE".into()),
                Request::ListDir { .. } => Response::Texts(vec!["..".into(), "lib".into()]),
                Request::SetClipboardImage { png } => {
                    self.images.lock().unwrap().push(png);
                    Response::Done
                }
                _ => Response::Error("unexpected".into()),
            })
        }
    }

    /// This machine: an image on the clipboard; records what it opens.
    #[derive(Default)]
    struct Home {
        opened: Mutex<Vec<String>>,
        copied: Mutex<Vec<String>>,
    }

    impl crate::platform::Platform for Home {
        fn clipboard_has_image(&self) -> bool {
            true
        }
        fn clipboard_has_text(&self) -> bool {
            false
        }
        fn clipboard_image_png(&self) -> anyhow::Result<Vec<u8>> {
            Ok(b"\x89PNG fake".to_vec())
        }
        fn set_clipboard_text(&self, text: &str) -> anyhow::Result<()> {
            self.copied.lock().unwrap().push(text.to_owned());
            Ok(())
        }
        fn open_link(&self, target: &str) -> anyhow::Result<String> {
            self.opened.lock().unwrap().push(target.to_owned());
            Ok(format!("Opened {target}"))
        }
        fn open_uri(&self, uri: &str) -> anyhow::Result<()> {
            self.opened.lock().unwrap().push(uri.to_owned());
            Ok(())
        }
    }

    struct Fixture {
        app: App,
        sent: Receiver<ClientMsg>,
        events: Receiver<AppEvent>,
        service: Arc<Service>,
        home: Arc<Home>,
    }

    fn fixture(target: Option<ConnTarget>) -> Fixture {
        let (tx, sent) = bounded(64);
        let (_keep, service_rx) = bounded(1);
        let (app_tx, events) = unbounded();
        let service = Arc::new(Service::default());
        let mut app = App::new(Conn::detached(tx, service_rx), service.clone(), Opened::detached(crate::doc::Doc::default()), false, ".".into(), app_tx, None);
        let home = Arc::new(Home::default());
        app.platform = home.clone();
        app.target = target;
        app.terms.insert("t".into(), TermInfo { id: "t".into(), alive: true, ..Default::default() });
        Fixture { app, sent, events, service, home }
    }

    fn remote(vscode: Option<&str>) -> ConnTarget {
        let record = canvas_common::remote::ServerRecord {
            name: "cloud".into(),
            addr: Some("127.0.0.1:1".into()),
            vscode_tunnel: vscode.map(str::to_owned),
            client_id: "pc-1".into(),
            ..Default::default()
        };
        ConnTarget::Server(crate::tunnel::Server::new(record, vec![1; 32]))
    }

    /// Runs background continuations until `done`.
    fn settle(f: &mut Fixture, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(&f.app) {
            assert!(std::time::Instant::now() < deadline, "background work did not finish");
            if let Ok(event) = f.events.recv_timeout(std::time::Duration::from_millis(50)) {
                f.app.on_app_event(event);
            }
        }
    }

    #[test]
    fn image_paste_goes_through_the_service_clipboard_only_in_remote_mode() {
        let ctrl_v = Input::Key { key: Key::Char('v'), mods: MOD_CTRL };
        let mut local = fixture(Some(ConnTarget::Tcp { addr: "x:1".into(), remote_id: None, secret: vec![] }));
        local.app.target = None;
        local.app.send_ctrl_v("t");
        assert_eq!(local.sent.try_recv().ok(), Some(ClientMsg::Input { id: "t".into(), input: ctrl_v.clone() }), "locally the agent reads this clipboard");
        assert!(local.service.images.lock().unwrap().is_empty());

        let mut f = fixture(Some(remote(None)));
        f.app.paste_key("t", Input::Key { key: Key::Char('v'), mods: MOD_ALT });
        assert!(f.sent.try_recv().is_err(), "the key waits for the image");
        settle(&mut f, |app| app.notice.as_ref().is_some_and(|n| n.text.starts_with("Image pasted")));
        assert_eq!(*f.service.images.lock().unwrap(), vec![b"\x89PNG fake".to_vec()]);
        assert_eq!(f.sent.try_recv().ok(), Some(ClientMsg::Input { id: "t".into(), input: Input::Key { key: Key::Char('v'), mods: MOD_ALT } }));
        // Offline: nothing is sent, nothing is queued.
        f.app.connected = false;
        f.app.send_ctrl_v("t");
        assert!(f.sent.try_recv().is_err() && f.service.images.lock().unwrap().len() == 1);
    }

    #[test]
    fn remote_paths_open_in_vscode_or_the_viewer_and_urls_open_here() {
        let mut f = fixture(Some(remote(None)));
        f.app.open_link("https://example.com/x");
        assert_eq!(f.home.opened.lock().unwrap().last().map(String::as_str), Some("https://example.com/x"), "URLs open on this machine");
        f.app.open_link(r"C:\work\missing.rs:3");
        assert_eq!(f.app.notice.as_ref().map(|n| n.text.as_str()), Some(r"C:\work\missing.rs does not exist"), "checked on the service's machine");
        f.app.open_link(r"C:\work\notes.md:2");
        settle(&mut f, |app| matches!(app.modals.last(), Some(Modal::Viewer(v)) if v.lines.is_some()));
        let Some(Modal::Viewer(viewer)) = f.app.modals.last().cloned() else { panic!("viewer") };
        assert_eq!((viewer.path.as_str(), viewer.lines.as_deref()), (r"C:\work\notes.md", Some(&["# Notes".to_owned(), "REMOTE".to_owned()][..])));
        assert!(viewer.vscode.is_none());
        assert!(!f.app.viewer_button(&viewer, crate::viewer::COPY_PATH));
        assert_eq!(*f.home.copied.lock().unwrap(), vec![r"C:\work\notes.md".to_owned()]);
        f.app.modals.clear();
        f.app.open_folder(r"C:\work\src");
        settle(&mut f, |app| matches!(app.modals.last(), Some(Modal::Viewer(v)) if v.lines.is_some()));
        let Some(Modal::Viewer(folder)) = f.app.modals.last() else { panic!("folder viewer") };
        assert!(folder.folder && folder.lines.as_deref() == Some(&["▸ lib".to_owned()][..]));

        // With a VS Code tunnel, files and folders open in VS Code (where it is installed);
        // automation files always open in the viewer, with the button.
        let mut f = fixture(Some(remote(Some("cpc"))));
        f.app.open_target(r"C:\work\notes.md");
        let Some(Modal::Viewer(viewer)) = f.app.modals.last().cloned() else { panic!("automation files use the viewer") };
        assert_eq!(viewer.vscode.as_deref(), Some("vscode://vscode-remote/tunnel+cpc/C:/work/notes.md"));
        assert_eq!(viewer.buttons()[0], crate::viewer::OPEN_IN_VSCODE);
        f.app.modals.clear();
        f.app.open_link(r"C:\work\notes.md:7");
        if crate::links::vscode_installed() {
            assert_eq!(f.home.opened.lock().unwrap().last().map(String::as_str), Some("vscode://vscode-remote/tunnel+cpc/C:/work/notes.md:7"));
            assert!(f.app.modals.is_empty());
        } else {
            assert!(matches!(f.app.modals.last(), Some(Modal::Viewer(_))), "no VS Code here: the viewer");
        }
    }
}