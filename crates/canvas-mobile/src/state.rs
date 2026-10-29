//! The phone's view of the service: the canvas replica, session summaries,
//! terminals and their screens, and what to send. Pure (no threads, no FFI):
//! every change returns [`Effect`]s for the client to carry out.
//!
//! The phone is a *light* client (docs/IOS-PLAN.md D5): it edits the shared
//! canvas only for what you do on it (open, review, rename, new session,
//! remove); subagent boxes, automation boxes and bus routing stay the
//! desktop clients' job.

use std::collections::{HashMap, HashSet};

use canvas_common::protocol::{ClientMsg, Input, Key, LaunchKind, MOD_CTRL, ServiceMsg, SessionSummary, TermInfo};
use canvas_model::agent::{AgentKind, agent_of, native_id};
use canvas_model::bus;
use canvas_model::doc::{Doc, GRID_X, GRID_Y, Node, NodeKind};
use canvas_model::launch::agent_launch;
use canvas_model::rpc::BackendInfo;
use canvas_model::status::{Status, agent_status, session_status};
use canvas_model::sync::{Opened, Replica};

use crate::model::{BoxKind, ConnState, CoreEvent, GroupItem, NewSession, Overview, Screen, SessionItem, StatusKind, SubagentItem};
use crate::screen::ViewState;

/// Something for the client to do after a state change.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(ClientMsg),
    Emit(CoreEvent),
}

#[derive(Default)]
pub struct State {
    pub conn: ConnState,
    pub message: String,
    pub doc: Doc,
    replica: Option<Replica>,
    /// A whole document was asked for; patches wait for it.
    doc_resync: bool,
    pub sessions: HashMap<String, SessionSummary>,
    pub terms: HashMap<String, TermInfo>,
    /// The next terminal list replaces the current one (after a reconnect).
    resync_terms: bool,
    views: HashMap<String, ViewState>,
    /// Terminal id → the agent's own session id (OpenCode names its sessions).
    agent_sessions: HashMap<String, String>,
    bus: bus::Snapshot,
    /// Terminals shown (subscribed); the focused one gets frames fastest.
    pub subs: Vec<String>,
    pub focused: Option<String>,
    /// Terminals shown at the phone's size (else mirrored at the desktop's).
    pub fit: HashMap<String, (u16, u16)>,
    /// Launches sent and not answered yet (terminal id → when).
    launching: HashMap<String, u64>,
    /// Launch errors per box.
    errors: HashMap<String, String>,
    pub info: Option<BackendInfo>,
    last_tracked: Option<Vec<String>>,
    /// Last status of each session (attention events on changes).
    seen: HashMap<String, StatusKind>,
    /// Statuses are compared once the first snapshot is in.
    have_sessions: bool,
}

impl State {
    // ---------- connection ----------

    /// A new connection is authenticated: everything is asked for again.
    pub fn on_connected(&mut self) -> Vec<Effect> {
        self.conn = ConnState::Online;
        self.message.clear();
        self.resync_terms = true;
        self.last_tracked = None;
        self.doc_resync = true;
        let mut effects = vec![Effect::Send(ClientMsg::DocOpen { path: None, demo: false, cwd: String::new() })];
        effects.extend(self.subscription_messages());
        effects.push(Effect::Emit(CoreEvent::Connection { state: ConnState::Online, message: String::new() }));
        effects.push(Effect::Emit(CoreEvent::Overview));
        effects
    }

    pub fn on_disconnected(&mut self, state: ConnState, message: String) -> Vec<Effect> {
        self.conn = state;
        self.message = message.clone();
        self.launching.clear();
        vec![Effect::Emit(CoreEvent::Connection { state, message }), Effect::Emit(CoreEvent::Overview)]
    }

    /// `Subscribe8`, and for terminals shown at the phone's size their size and claim.
    fn subscription_messages(&self) -> Vec<Effect> {
        let mut out = vec![Effect::Send(ClientMsg::Subscribe8 { ids: self.subs.clone(), focused: self.focused.clone() })];
        for id in &self.subs {
            if let Some(&(cols, rows)) = self.fit.get(id) {
                out.push(Effect::Send(ClientMsg::Resize { id: id.clone(), cols, rows }));
                out.push(Effect::Send(ClientMsg::Claim { id: id.clone() }));
            }
        }
        out
    }

    // ---------- service messages ----------

    pub fn on_service(&mut self, message: ServiceMsg, now: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut overview = false;
        match message {
            ServiceMsg::Terminals(list) => {
                if std::mem::take(&mut self.resync_terms) {
                    self.terms.clear();
                }
                for info in list {
                    self.launching.remove(&info.id);
                    if info.alive {
                        self.errors.remove(&info.id);
                    }
                    if self.subs.contains(&info.id) {
                        effects.push(Effect::Emit(CoreEvent::Screen { id: info.id.clone() }));
                    }
                    self.terms.insert(info.id.clone(), info);
                }
                overview = true;
            }
            ServiceMsg::TerminalGone(id) => {
                self.terms.remove(&id);
                self.views.remove(&id);
                overview = true;
            }
            ServiceMsg::Frame8(frame) => {
                let id = frame.id.clone();
                self.views.entry(id.clone()).or_default().apply(frame);
                effects.push(Effect::Emit(CoreEvent::Screen { id }));
            }
            ServiceMsg::Sessions { full, upsert, remove } => {
                if full {
                    self.sessions.clear();
                }
                for summary in upsert {
                    self.sessions.insert(summary.id.clone(), summary);
                }
                for id in remove {
                    self.sessions.remove(&id);
                }
                self.have_sessions = true;
                overview = true;
            }
            ServiceMsg::Clipboard(text) => effects.push(Effect::Emit(CoreEvent::Clipboard { text })),
            ServiceMsg::Error { id, message } => {
                if let Some(id) = id {
                    self.launching.remove(&id);
                    self.errors.insert(id, message.clone());
                    overview = true;
                }
                effects.push(Effect::Emit(CoreEvent::Notice { text: message, error: true }));
            }
            ServiceMsg::Bus { json } => {
                if let Ok(snapshot) = serde_json::from_str::<bus::Snapshot>(&json) {
                    self.bus = snapshot;
                    overview = true;
                }
            }
            ServiceMsg::AgentSession { id, native } => {
                self.agent_sessions.insert(id, native);
                overview = true;
            }
            ServiceMsg::Doc { key, version, json } => {
                self.doc_resync = false;
                match Opened::from_message(key, version, &json) {
                    Ok(opened) => {
                        match &mut self.replica {
                            Some(replica) => replica.resync(&mut self.doc, opened),
                            None => {
                                self.doc = opened.doc.clone();
                                self.replica = Some(Replica::new(&opened));
                            }
                        }
                        // Local changes kept on top go out now.
                        effects.extend(self.edit());
                    }
                    Err(error) => effects.push(Effect::Emit(CoreEvent::Notice { text: format!("The canvas could not be loaded: {error:#}"), error: true })),
                }
                overview = true;
            }
            ServiceMsg::DocPatch { key, version, origin, ops_json } => {
                let Some(replica) = self.replica.as_mut().filter(|r| r.key == key && !self.doc_resync) else { return effects };
                let resync = match serde_json::from_str::<Vec<canvas_model::sync::Op>>(&ops_json) {
                    Ok(ops) => {
                        let patched = replica.apply_patch(&mut self.doc, version, origin, &ops);
                        overview |= patched.changed;
                        patched.resync
                    }
                    Err(_) => true,
                };
                if resync {
                    self.doc_resync = true;
                    effects.push(Effect::Send(ClientMsg::DocOpen { path: None, demo: false, cwd: String::new() }));
                }
            }
            ServiceMsg::DocSaved { error: Some(error), .. } => effects.push(Effect::Emit(CoreEvent::Notice { text: format!("The canvas could not be saved on the service: {error}"), error: true })),
            ServiceMsg::Notification { term, title, body } => effects.push(Effect::Emit(CoreEvent::Notification { term, title, body })),
            ServiceMsg::TunnelAccess { url, token, expires_ms } => effects.push(Effect::Emit(CoreEvent::TunnelAccess { url, token, expires_ms })),
            ServiceMsg::Image { term, image_id, width, height, rgba_or_png, format } => {
                self.views.entry(term).or_default().view.set_image(image_id, width, height, format, rgba_or_png);
            }
            ServiceMsg::Placements { term, placements } => self.views.entry(term).or_default().view.set_placements(placements),
            // Connection-level messages are the client's; subsessions and
            // the rest are the desktop clients' to put on the canvas.
            _ => {}
        }
        if overview {
            effects.extend(self.after_change(now));
            effects.push(Effect::Emit(CoreEvent::Overview));
        }
        effects
    }

    /// Tracking, attention events and the focused session's review.
    fn after_change(&mut self, now: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        // History is indexed for the sessions on the canvas (so statuses are live
        // even when no desktop client is connected).
        let mut tracked: Vec<String> = self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && n.command.is_none()).map(|n| native_id(n).to_owned()).collect();
        tracked.sort();
        tracked.dedup();
        if self.conn == ConnState::Online && self.replica.is_some() && self.last_tracked.as_ref() != Some(&tracked) {
            effects.push(Effect::Send(ClientMsg::Track { ids: tracked.clone() }));
            self.last_tracked = Some(tracked);
        }
        // A session that starts waiting for you or finishes a turn.
        let mut seen = HashMap::new();
        let mut review = None;
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
            let status: StatusKind = self.node_status(node, now).into();
            let before = self.seen.get(&node.id).copied();
            if self.have_sessions && before.is_some() && before != Some(status) {
                let attention = status == StatusKind::NeedsYou || (status == StatusKind::Done && before == Some(StatusKind::Working));
                if attention {
                    effects.push(Effect::Emit(CoreEvent::Attention { id: node.id.clone(), name: node.name.clone(), status }));
                }
            }
            // A turn that ends while you watch it is already reviewed.
            if status == StatusKind::Done && self.focused.as_deref() == Some(node.id.as_str()) {
                review = Some(node.id.clone());
            }
            seen.insert(node.id.clone(), status);
        }
        self.seen = seen;
        if let Some(id) = review {
            effects.extend(self.mark_reviewed(&id, now));
        }
        effects
    }

    // ---------- document edits ----------

    /// The local changes not sent yet, as a `DocEdit`.
    fn edit(&mut self) -> Vec<Effect> {
        let Some(replica) = self.replica.as_mut() else { return Vec::new() };
        if self.conn != ConnState::Online {
            return Vec::new();
        }
        match replica.take_edit(&self.doc) {
            Some((base, ops)) => {
                let ops_json = serde_json::to_string(&ops).unwrap_or_else(|_| "[]".into());
                vec![Effect::Send(ClientMsg::DocEdit { key: replica.key.clone(), base, ops_json })]
            }
            None => Vec::new(),
        }
    }

    /// You looked at a session: a finished turn no longer waits for review.
    pub fn mark_reviewed(&mut self, id: &str, now: u64) -> Vec<Effect> {
        let Some(node) = self.doc.node_mut(id) else { return Vec::new() };
        if node.reviewed_ms.is_some_and(|r| r >= now) {
            return Vec::new();
        }
        node.reviewed_ms = Some(now);
        self.seen.insert(id.to_owned(), StatusKind::Idle);
        let mut effects = self.edit();
        effects.push(Effect::Emit(CoreEvent::Overview));
        effects
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<Vec<Effect>, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("A name cannot be empty.".into());
        }
        let node = self.doc.node_mut(id).ok_or("That box is no longer on the canvas.")?;
        node.name = name.to_owned();
        let mut effects = self.edit();
        effects.push(Effect::Emit(CoreEvent::Overview));
        Ok(effects)
    }

    /// Removes a box whose terminal is not running (its history stays).
    pub fn remove(&mut self, id: &str) -> Result<Vec<Effect>, String> {
        if self.terms.get(id).is_some_and(|t| t.alive) {
            return Err("Stop the session first.".into());
        }
        if self.doc.node(id).is_none() {
            return Err("That box is no longer on the canvas.".into());
        }
        self.doc.remove_node(id);
        let mut effects = vec![Effect::Send(ClientMsg::Forget { id: id.to_owned() })];
        effects.extend(self.edit());
        effects.push(Effect::Emit(CoreEvent::Overview));
        Ok(effects)
    }

    // ---------- terminals ----------

    /// Shows terminal `id` (the only one subscribed); `fit`: at the phone's size.
    pub fn open_terminal(&mut self, id: &str, fit: Option<(u16, u16)>, now: u64) -> Vec<Effect> {
        self.subs = vec![id.to_owned()];
        self.focused = Some(id.to_owned());
        match fit {
            Some(size) => {
                self.fit.insert(id.to_owned(), size);
            }
            None => {
                self.fit.remove(id);
            }
        }
        // A new subscription starts with a full frame.
        if let Some(view) = self.views.get_mut(id) {
            view.force_full();
        }
        let mut effects = if self.conn == ConnState::Online { self.subscription_messages() } else { Vec::new() };
        effects.extend(self.mark_reviewed(id, now));
        effects
    }

    /// The phone's terminal area changed size (rotation, keyboard).
    pub fn resize_terminal(&mut self, id: &str, cols: u16, rows: u16) -> Vec<Effect> {
        match self.fit.get_mut(id) {
            Some(size) if *size != (cols, rows) => {
                *size = (cols, rows);
                vec![Effect::Send(ClientMsg::Resize { id: id.to_owned(), cols, rows })]
            }
            _ => Vec::new(),
        }
    }

    /// Switches between the phone's size and mirroring the desktop's. To
    /// mirror, the phone stops showing the terminal for a moment: the service
    /// forgets its size, so the desktop's applies again.
    pub fn set_fit(&mut self, id: &str, fit: Option<(u16, u16)>) -> Vec<Effect> {
        let mut effects = Vec::new();
        match fit {
            Some((cols, rows)) => {
                self.fit.insert(id.to_owned(), (cols, rows));
                effects.push(Effect::Send(ClientMsg::Resize { id: id.to_owned(), cols, rows }));
                effects.push(Effect::Send(ClientMsg::Claim { id: id.to_owned() }));
            }
            None => {
                self.fit.remove(id);
                let others: Vec<String> = self.subs.iter().filter(|s| *s != id).cloned().collect();
                effects.push(Effect::Send(ClientMsg::Subscribe8 { ids: others, focused: None }));
                effects.push(Effect::Send(ClientMsg::Subscribe8 { ids: self.subs.clone(), focused: self.focused.clone() }));
            }
        }
        if let Some(view) = self.views.get_mut(id) {
            view.force_full();
        }
        effects
    }

    pub fn close_terminal(&mut self) -> Vec<Effect> {
        self.subs.clear();
        self.focused = None;
        vec![Effect::Send(ClientMsg::Subscribe8 { ids: Vec::new(), focused: None })]
    }

    /// The screen of a terminal: what changed since it was last taken, or all of it.
    pub fn take_screen(&mut self, id: &str, full: bool) -> Option<Screen> {
        let term = self.terms.get(id).cloned();
        let copilot = self.doc.node(id).is_some_and(|n| n.command.is_none() && agent_of(n) == AgentKind::Copilot);
        let view = self.views.get_mut(id)?;
        Some(view.take(id, term.as_ref(), copilot, full))
    }

    /// Selected text of a terminal's viewport (cells, inclusive).
    pub fn selection_text(&self, id: &str, anchor: (u16, u16), head: (u16, u16)) -> String {
        let Some(view) = self.views.get(id) else { return String::new() };
        let selection = canvas_termview::TermSelection { id: id.to_owned(), anchor, head };
        view.view.selection_text(&selection)
    }

    // ---------- sessions ----------

    pub fn summary(&self, node: &Node) -> Option<&SessionSummary> {
        match node.kind {
            NodeKind::Session => {
                let native = self.agent_sessions.get(&node.id).map(String::as_str).unwrap_or_else(|| native_id(node));
                self.sessions.get(native).or_else(|| self.sessions.get(native_id(node)))
            }
            NodeKind::Agent => {
                let parent = node.parent.as_deref()?;
                self.sessions.get(parent).or_else(|| self.doc.node(parent).map(native_id).filter(|n| *n != parent).and_then(|n| self.sessions.get(n)))
            }
            NodeKind::Automation => None,
        }
    }

    fn alive(&self, id: &str) -> bool {
        self.terms.get(id).is_some_and(|t| t.alive)
    }

    /// The status the desktop canvas shows for a box.
    pub fn node_status(&self, node: &Node, now: u64) -> Status {
        match node.kind {
            NodeKind::Session if node.passive.is_some() => {
                let view = node.passive.as_ref().and_then(|p| self.bus.passive.get(&p.key));
                if view.is_some_and(|v| v.running) || self.alive(&node.id) && session_status(self.summary(node), self.terms.get(&node.id), node.reviewed_ms) == Status::Working {
                    return Status::Working;
                }
                match view.map(|v| (v.status.as_str(), v.status_ms)) {
                    Some(("alert", _)) => Status::Waiting,
                    Some(("done", at)) if node.reviewed_ms.is_some_and(|r| at > r) => Status::JustFinished,
                    _ if node.passive.as_ref().is_some_and(|p| p.paused) => Status::Stopped,
                    _ => Status::Idle,
                }
            }
            NodeKind::Session => {
                if self.launching.contains_key(&node.id) {
                    return Status::Preparing;
                }
                if self.errors.contains_key(&node.id) && !self.alive(&node.id) {
                    return Status::Failed;
                }
                session_status(self.summary(node), self.terms.get(&node.id), node.reviewed_ms)
            }
            NodeKind::Automation => Status::Unknown,
            NodeKind::Agent => {
                let parent = node.parent.as_deref().unwrap_or_default();
                let summary = self.summary(node);
                let running = self.alive(parent) || summary.is_some_and(|s| s.live);
                match summary.and_then(|s| s.agents.iter().find(|a| Some(&a.key) == node.agent_key.as_ref())) {
                    Some(agent) => agent_status(agent.status, agent.finished_ms, running, now),
                    None => Status::Unknown,
                }
            }
        }
    }

    fn item(&self, node: &Node, now: u64) -> SessionItem {
        let summary = self.summary(node);
        let term = self.terms.get(&node.id);
        let status = self.node_status(node, now);
        let alive = term.is_some_and(|t| t.alive);
        let kind = match node.kind {
            NodeKind::Session if node.passive.is_some() => BoxKind::Passive,
            NodeKind::Session => BoxKind::Session,
            NodeKind::Agent => BoxKind::Subagent,
            NodeKind::Automation => BoxKind::Automation,
        };
        let cwd = node.cwd.clone().filter(|c| !c.is_empty()).or_else(|| summary.map(|s| s.cwd.clone())).unwrap_or_default();
        let subtitle = if let Some(error) = self.errors.get(&node.id).filter(|_| !alive) {
            error.clone()
        } else if let Some(title) = term.filter(|t| t.alive && !t.title.trim().is_empty()).map(|t| t.title.trim().to_owned()) {
            title
        } else if node.kind == NodeKind::Agent {
            "subagent".into()
        } else {
            folder_name(&cwd)
        };
        let running = alive || summary.is_some_and(|s| s.live);
        let subagents = if node.kind == NodeKind::Session {
            summary
                .map(|s| {
                    s.agents
                        .iter()
                        .map(|a| SubagentItem { key: a.key.clone(), name: a.name.clone(), description: a.description.clone(), status: agent_status(a.status, a.finished_ms, running, now).into() })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let rect = node.rect();
        let open_elsewhere = !alive && summary.is_some_and(|s| s.live);
        SessionItem {
            id: node.id.clone(),
            kind,
            name: node.name.clone(),
            agent: agent_of(node).id().to_owned(),
            status: status.into(),
            status_label: status.label().to_owned(),
            subtitle,
            cwd,
            branch: summary.map(|s| s.branch.clone()).unwrap_or_default(),
            parent: node.parent.clone(),
            group: self.doc.group_of(node).map(|g| g.id.clone()),
            alive,
            can_open: node.kind == NodeKind::Session && node.passive.is_none() && !open_elsewhere,
            last_activity_ms: summary.map_or(0, |s| s.last_event_ms.max(s.updated_ms)).max(term.map_or(0, |t| t.last_output_ms)),
            loop_label: summary.filter(|s| s.loop_ms > 0).map(|s| format!("every {}", every(s.loop_ms))).unwrap_or_default(),
            mailbox: self.bus.mailbox.get(&node.id).copied().unwrap_or(0),
            subagents,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
        }
    }

    pub fn overview(&self, now: u64) -> Overview {
        let sessions = self.doc.nodes.iter().map(|n| self.item(n, now)).collect();
        let groups = self
            .doc
            .groups
            .iter()
            .map(|g| GroupItem { id: g.id.clone(), name: g.name.clone(), color: g.color, folder: g.folder.clone().unwrap_or_default(), x: g.x, y: g.y, w: g.w, h: g.h })
            .collect();
        let mut agents = Vec::new();
        if let Some(info) = &self.info {
            let default = self.doc.default_agent.as_deref().and_then(AgentKind::parse).filter(|a| info.installed.contains(a)).unwrap_or(info.default_agent);
            agents.push(default.id().to_owned());
            agents.extend(info.installed.iter().filter(|a| **a != default).map(|a| a.id().to_owned()));
        }
        Overview {
            state: self.conn,
            message: self.message.clone(),
            sessions,
            groups,
            canvas_path: self.replica.as_ref().map(|r| r.path.clone()).unwrap_or_default(),
            agents,
            home_folder: self.info.as_ref().map(|i| i.user_home.clone()).unwrap_or_default(),
            recent_folders: self.doc.recent_folders.iter().map(|f| f.path.clone()).collect(),
        }
    }

    // ---------- launching ----------

    /// The folder a session starts in.
    pub fn launch_folder(&self, id: &str) -> Option<String> {
        let node = self.doc.node(id)?;
        node.cwd
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| self.summary(node).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()))
            .or_else(|| self.info.as_ref().map(|i| i.user_home.clone()))
    }

    /// Starts (or resumes) a session's terminal at `cols`×`rows`. Errors are
    /// for the user. Nothing to do when it already runs.
    pub fn launch(&mut self, id: &str, cols: u16, rows: u16, prompt: Option<String>, now: u64) -> Result<Vec<Effect>, String> {
        if self.alive(id) {
            return Ok(Vec::new());
        }
        if self.conn != ConnState::Online {
            return Err("Offline: the session cannot be started now.".into());
        }
        let node = self.doc.node(id).cloned().ok_or("That box is no longer on the canvas.")?;
        if node.kind != NodeKind::Session {
            return Err("Only sessions have a terminal.".into());
        }
        if node.passive.is_some() {
            return Err("Passive sessions run on their own schedule; open them on the desktop.".into());
        }
        if self.summary(&node).is_some_and(|s| s.live) {
            return Err("This session is open in another terminal (CLI or VS Code); Copilot Canvas never starts a second copy.".into());
        }
        let cwd = self.launch_folder(id).ok_or("No folder for this session.")?;
        let kind = match &node.command {
            Some(command) if !command.is_empty() => LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() },
            _ => {
                let group = self.doc.group_of(&node).cloned();
                LaunchKind::Agent(Box::new(agent_launch(&node, &self.doc.defaults, group.as_ref(), prompt, None, !node.fresh)))
            }
        };
        self.errors.remove(id);
        self.launching.insert(id.to_owned(), now);
        let mut effects = vec![Effect::Send(ClientMsg::Launch { id: id.to_owned(), kind, cwd: cwd.clone(), cols, rows })];
        if let Some(node) = self.doc.node_mut(id) {
            node.fresh = false;
            node.cwd = Some(cwd.clone());
        }
        self.doc.note_folder(&cwd, now);
        effects.extend(self.edit());
        effects.push(Effect::Emit(CoreEvent::Overview));
        Ok(effects)
    }

    /// A new session box (placed in its group, or below the others); returns its id.
    pub fn add_session(&mut self, request: &NewSession, cwd: &str) -> Result<String, String> {
        let agent = match request.agent.trim() {
            "" => self.info.as_ref().map(|i| i.default_agent).unwrap_or_default(),
            name => AgentKind::parse(name).ok_or_else(|| format!("Unknown agent {name:?}"))?,
        };
        if let Some(info) = &self.info
            && !info.installed.is_empty()
            && !info.installed.contains(&agent)
        {
            return Err(format!("{} is not installed on the service's machine.", agent.label()));
        }
        let mut name = request.name.trim().to_owned();
        if name.is_empty() {
            name = folder_name(cwd);
        }
        if name.is_empty() {
            name = "Session".into();
        }
        let (x, y) = match self.doc.group(&request.group) {
            Some(group) => self.doc.free_slot(group.x + 2, group.y + 2, ((group.w - 4) / GRID_X).max(1), NodeKind::Session),
            None => {
                let left = self.doc.nodes.iter().map(|n| n.x).min().unwrap_or(0);
                let bottom = self.doc.nodes.iter().map(|n| n.y + n.rect().h).max().map_or(0, |b| b + GRID_Y - 4);
                self.doc.free_slot(left, bottom, 4, NodeKind::Session)
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let node: Node = serde_json::from_value(serde_json::json!({
            "id": id, "kind": "session", "name": name, "x": x, "y": y, "cwd": cwd, "fresh": true,
            "agent": (agent != AgentKind::Copilot).then(|| agent.id()),
        }))
        .map_err(|e| e.to_string())?;
        self.doc.nodes.push(node);
        Ok(id)
    }

    /// Stops a terminal gracefully (Claude Code also wants Ctrl+D).
    pub fn stop(&self, id: &str) -> Vec<Effect> {
        let mut effects = vec![Effect::Send(ClientMsg::Stop { id: id.to_owned(), force: false })];
        if self.doc.node(id).is_some_and(|n| n.command.is_none() && agent_of(n) == AgentKind::Claude) {
            effects.push(Effect::Send(ClientMsg::Input { id: id.to_owned(), input: Input::Key { key: Key::Char('d'), mods: MOD_CTRL } }));
        }
        effects
    }

    /// Ids of every box whose session runs in a terminal of the service.
    pub fn running(&self) -> HashSet<String> {
        self.terms.values().filter(|t| t.alive).map(|t| t.id.clone()).collect()
    }
}

/// The last component of a Windows or POSIX path.
pub fn folder_name(path: &str) -> String {
    path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or_default().to_owned()
}

/// `15m`, `2h`, `1d`.
fn every(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => format!("{}s", ms / 1000),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_common::protocol::{Activity, Frame8, Run8};
    use canvas_model::sync::DocReply;

    fn doc_message(doc: &Doc, version: u64) -> ServiceMsg {
        let reply = DocReply { path: r"C:\Users\me\.copilot-canvas\canvas.json".into(), client: 7, doc: Some(doc.clone()), error: None };
        ServiceMsg::Doc { key: "canvas-1".into(), version, json: serde_json::to_string(&reply).unwrap() }
    }

    fn node(id: &str, name: &str, x: i32) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": x, "y": 0, "cwd": r"C:\work\app" })).unwrap()
    }

    fn online() -> State {
        let mut state = State::default();
        state.on_connected();
        let mut doc = Doc::default();
        doc.nodes.push(node("a", "Fix flaky test", 0));
        doc.nodes.push(node("b", "Review PR", 40));
        state.on_service(doc_message(&doc, 1), 1_000);
        state
    }

    fn sent(effects: &[Effect]) -> Vec<&ClientMsg> {
        effects.iter().filter_map(|e| if let Effect::Send(m) = e { Some(m) } else { None }).collect()
    }

    #[test]
    fn connecting_asks_for_the_canvas_and_tracks_its_sessions() {
        let mut state = State::default();
        let effects = state.on_connected();
        assert!(matches!(sent(&effects)[0], ClientMsg::DocOpen { path: None, .. }));
        let mut doc = Doc::default();
        doc.nodes.push(node("a", "A", 0));
        let effects = state.on_service(doc_message(&doc, 1), 0);
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::Track { ids } if ids == &["a".to_owned()])));
        assert_eq!(state.overview(0).sessions.len(), 1);
        assert_eq!(state.overview(0).canvas_path, r"C:\Users\me\.copilot-canvas\canvas.json");
    }

    #[test]
    fn statuses_follow_history_terminals_and_reviews() {
        let mut state = online();
        state.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: true, title: "Fixing the test".into(), ..Default::default() }]), 1_000);
        let summary = SessionSummary { id: "a".into(), live: true, activity: Activity::Working, last_event_ms: 900, ..Default::default() };
        state.on_service(ServiceMsg::Sessions { full: true, upsert: vec![summary.clone()], remove: Vec::new() }, 1_000);
        let item = |state: &State| state.overview(2_000).sessions.into_iter().find(|s| s.id == "a").unwrap();
        assert_eq!(item(&state).status, StatusKind::Working);
        assert_eq!(item(&state).subtitle, "Fixing the test", "the terminal's title");
        // Waiting for a permission: an attention event.
        let waiting = SessionSummary { pending: 1, ..summary.clone() };
        let effects = state.on_service(ServiceMsg::Sessions { full: false, upsert: vec![waiting], remove: Vec::new() }, 2_000);
        assert!(effects.contains(&Effect::Emit(CoreEvent::Attention { id: "a".into(), name: "Fix flaky test".into(), status: StatusKind::NeedsYou })));
        // The turn ends after you last looked: done · review, until you open it.
        state.doc.node_mut("a").unwrap().reviewed_ms = Some(500);
        let done = SessionSummary { activity: Activity::Idle, finished_ms: Some(3_000), ..summary };
        state.on_service(ServiceMsg::Sessions { full: false, upsert: vec![done], remove: Vec::new() }, 3_000);
        assert_eq!(item(&state).status, StatusKind::Done);
        let effects = state.open_terminal("a", Some((50, 30)), 4_000);
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::DocEdit { ops_json, .. } if ops_json.contains("reviewed_ms"))), "opening reviews it");
        assert_eq!(item(&state).status, StatusKind::Idle);
    }

    #[test]
    fn opening_a_terminal_at_the_phones_size_resizes_and_claims_it() {
        let mut state = online();
        let effects = state.open_terminal("a", Some((48, 30)), 1);
        let messages = sent(&effects);
        assert!(matches!(messages[0], ClientMsg::Subscribe8 { ids, focused: Some(f) } if ids == &["a".to_owned()] && f == "a"));
        assert!(messages.contains(&&ClientMsg::Resize { id: "a".into(), cols: 48, rows: 30 }));
        assert!(messages.contains(&&ClientMsg::Claim { id: "a".into() }));
        assert_eq!(sent(&state.resize_terminal("a", 90, 20)), [&ClientMsg::Resize { id: "a".into(), cols: 90, rows: 20 }]);
        assert!(state.resize_terminal("a", 90, 20).is_empty(), "same size: nothing");
        // Mirroring: no size, and the service forgets this phone's.
        let effects = state.set_fit("a", None);
        assert!(!sent(&effects).iter().any(|m| matches!(m, ClientMsg::Resize { .. } | ClientMsg::Claim { .. })));
        assert!(state.resize_terminal("a", 40, 20).is_empty(), "mirrored terminals keep the desktop's size");
        // Reconnecting shows the same terminal again.
        let effects = state.on_connected();
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::Subscribe8 { ids, .. } if ids == &["a".to_owned()])));
    }

    #[test]
    fn screens_report_changed_rows_then_everything_on_request() {
        let mut state = online();
        let frame = |full: bool, rows: Vec<(u16, &str)>| Frame8 {
            id: "a".into(),
            cols: 10,
            rows: 3,
            full,
            changed: rows
                .into_iter()
                .map(|(y, t)| {
                    // The engine marks cells of several code points.
                    let clusters = if t.chars().count() > 1 && t.contains('\u{301}') { vec![(0, 1)] } else { Vec::new() };
                    (y, vec![Run8 { col: 0, text: t.into(), clusters, ..Default::default() }])
                })
                .collect(),
            cursor: Some((2, 1)),
            cursor_shape: 0,
            cursor_blink: false,
            display_offset: 0,
            history: 0,
            modes: 0,
            links: Vec::new(),
        };
        let effects = state.on_service(ServiceMsg::Frame8(frame(true, vec![(0, "one"), (1, "two"), (2, "e\u{301}")])), 0);
        assert_eq!(effects, [Effect::Emit(CoreEvent::Screen { id: "a".into() })]);
        let screen = state.take_screen("a", false).unwrap();
        assert!(screen.full && screen.changed.len() == 3);
        assert_eq!(screen.changed[2].runs[0].cells, ["e\u{301}"], "grapheme clusters stay together");
        state.on_service(ServiceMsg::Frame8(frame(false, vec![(1, "TWO")])), 0);
        let screen = state.take_screen("a", false).unwrap();
        assert!(!screen.full);
        assert_eq!(screen.changed.iter().map(|r| r.y).collect::<Vec<_>>(), [1]);
        assert_eq!((screen.cursor_visible, screen.cursor_col, screen.cursor_row), (true, 2, 1));
        assert!(state.take_screen("a", true).unwrap().changed.len() == 3);
    }

    #[test]
    fn launching_resumes_with_the_agent_and_marks_the_box_started() {
        let mut state = online();
        let effects = state.launch("a", 50, 30, None, 10).unwrap();
        let ClientMsg::Launch { id, kind: LaunchKind::Agent(launch), cwd, cols, rows } = sent(&effects)[0] else { panic!("launch") };
        assert_eq!((id.as_str(), cwd.as_str(), *cols, *rows), ("a", r"C:\work\app", 50, 30));
        assert!(launch.resume, "a box that is not fresh resumes");
        assert_eq!(state.overview(10).sessions[0].status, StatusKind::Preparing);
        state.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: true, ..Default::default() }]), 11);
        assert!(state.launch("a", 50, 30, None, 12).unwrap().is_empty(), "already running");
        // Running elsewhere: never a second copy.
        state.on_service(ServiceMsg::Sessions { full: true, upsert: vec![SessionSummary { id: "b".into(), live: true, ..Default::default() }], remove: Vec::new() }, 12);
        assert!(state.launch("b", 50, 30, None, 12).unwrap_err().contains("another terminal"));
    }

    #[test]
    fn new_sessions_are_placed_and_started_fresh() {
        let mut state = online();
        state.info = Some(BackendInfo { default_agent: AgentKind::OpenCode, installed: vec![AgentKind::OpenCode, AgentKind::Copilot], user_home: r"C:\Users\me".into(), ..Default::default() });
        let request = NewSession { name: String::new(), agent: String::new(), cwd: r"C:\src\repo".into(), prompt: "hi".into(), group: String::new() };
        let id = state.add_session(&request, r"C:\src\repo").unwrap();
        let node = state.doc.node(&id).unwrap().clone();
        assert_eq!((node.name.as_str(), node.agent.as_deref(), node.fresh), ("repo", Some("opencode"), true));
        assert!(state.doc.nodes.iter().filter(|n| n.id != id).all(|n| !n.rect().intersects(&node.rect())), "placed where nothing is");
        let effects = state.launch(&id, 50, 30, Some("hi".into()), 5).unwrap();
        let ClientMsg::Launch { kind: LaunchKind::Agent(launch), .. } = sent(&effects)[0] else { panic!("launch") };
        assert_eq!((launch.agent.as_str(), launch.resume, launch.prompt.as_deref()), ("opencode", false, Some("hi")));
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::DocEdit { .. })), "the new box goes to the service");
        let claude = NewSession { agent: "claude".into(), ..request };
        assert!(state.add_session(&claude, r"C:\x").unwrap_err().contains("not installed"));
    }

    #[test]
    fn launch_errors_and_patches_from_other_windows() {
        let mut state = online();
        state.on_service(ServiceMsg::Error { id: Some("a".into()), message: "folder not found".into() }, 0);
        let a = state.overview(0).sessions.into_iter().find(|s| s.id == "a").unwrap();
        assert_eq!((a.status, a.subtitle.as_str()), (StatusKind::Failed, "folder not found"));
        // Another window renames a box.
        let mut renamed = state.doc.node("b").unwrap().clone();
        renamed.name = "Review PR #42".into();
        let ops = vec![canvas_model::sync::Op::NodeFields(Box::new(renamed), vec!["name".into()])];
        let effects = state.on_service(ServiceMsg::DocPatch { key: "canvas-1".into(), version: 2, origin: 99, ops_json: serde_json::to_string(&ops).unwrap() }, 0);
        assert!(effects.contains(&Effect::Emit(CoreEvent::Overview)));
        assert_eq!(state.doc.node("b").unwrap().name, "Review PR #42");
        // A lost patch asks for the whole canvas again.
        let effects = state.on_service(ServiceMsg::DocPatch { key: "canvas-1".into(), version: 9, origin: 99, ops_json: "[]".into() }, 0);
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::DocOpen { .. })));
    }

    #[test]
    fn removing_needs_a_stopped_terminal_and_renames_are_shared() {
        let mut state = online();
        state.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: true, ..Default::default() }]), 0);
        assert!(state.remove("a").is_err());
        let effects = state.remove("b").unwrap();
        assert!(sent(&effects).contains(&&ClientMsg::Forget { id: "b".into() }));
        assert!(state.doc.node("b").is_none());
        let effects = state.rename("a", "  Better name ").unwrap();
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::DocEdit { ops_json, .. } if ops_json.contains("Better name"))));
        assert!(state.rename("a", " ").is_err());
    }

    #[test]
    fn folder_names_and_loop_labels() {
        assert_eq!(folder_name(r"C:\src\repo\"), "repo");
        assert_eq!(folder_name("/home/me/app"), "app");
        assert_eq!(every(15 * 60_000), "15m");
        assert_eq!(every(2 * 3_600_000), "2h");
        assert_eq!(every(86_400_000), "1d");
    }
}
