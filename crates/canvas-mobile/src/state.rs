//! The phone's view of the service: the canvas replica, session summaries,
//! terminals and their screens, the todo boards, and what to send. Pure (no
//! threads, no FFI): every change returns [`Effect`]s for the client to carry out.
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
use canvas_model::todos::{Actor, TodoOp};

use crate::model::{
    BoxKind, ConnState, CoreEvent, GroupItem, NewSession, NotebookOverview, NotebookRoot, Overview, ReportsOverview, Screen, SessionItem, StatusKind, SubagentItem, TodoChange, TodoDetail, TodosOverview,
};
use crate::screen::ViewState;

/// Something for the client to do after a state change.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(ClientMsg),
    Emit(CoreEvent),
}

/// How a subsession forks its parent's conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Fork {
    /// Copilot: the service copies the parent's history (`Request::Fork`), then the copy resumes.
    Copy { agent: AgentKind, parent: String },
    /// OpenCode, Claude Code: the agent forks it on the first start (`AgentLaunch::fork_from`).
    Native { parent: String },
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
    /// Your todo changes the service has not sent back yet: kept on top of its books.
    todos_sent: crate::todos::InFlight,
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
    /// The reports inbox (`CAP_REPORTS`).
    reports: crate::reports::Inbox,
    /// The notebook's folders as the service last said (asked for again after a reconnect).
    pub notebook: Option<canvas_model::notebook::Info>,
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
        self.notebook = None;
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
                } else if message.starts_with("Todo change ")
                    && let Some(served) = self.todos_sent.refused()
                {
                    // A todo change of this phone refused (the book as it is follows): undone now.
                    self.bus.todos = Some(served);
                    overview = true;
                }
                effects.push(Effect::Emit(CoreEvent::Notice { text: message, error: true }));
            }
            ServiceMsg::Bus { json } => {
                if let Ok(mut snapshot) = serde_json::from_str::<bus::Snapshot>(&json) {
                    // Your changes the service does not have yet stay on top of its book.
                    let ctx = self.todo_canvas(now).ctx(now);
                    snapshot.todos = self.todos_sent.receive(snapshot.todos.take(), &ctx, now);
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
            ServiceMsg::Reports { json } => {
                if let Ok(snapshot) = serde_json::from_str(&json) {
                    let arrived = self.reports.update(snapshot);
                    effects.push(Effect::Emit(CoreEvent::Reports));
                    if let Some(arrived) = arrived {
                        let item = crate::reports::item(&arrived.newest, &self.doc);
                        effects.push(Effect::Emit(CoreEvent::ReportArrived { id: item.id, title: item.title, source: item.source_name, more: arrived.more }));
                    }
                }
            }
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
        let passive = node.passive.as_ref();
        let view = passive.and_then(|p| self.bus.passive.get(&p.key));
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
            passive_key: passive.map(|p| p.key.clone()).unwrap_or_default(),
            paused: passive.is_some_and(|p| p.paused),
            passive_headline: view.map(|v| v.headline.clone()).unwrap_or_default(),
            passive_running: view.is_some_and(|v| v.running),
            next_run_ms: view.map_or(0, |v| v.next_run_ms),
            forkable: self.fork_kind(&node.id).is_some(),
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

    // ---------- reports ----------

    pub fn reports_overview(&self) -> ReportsOverview {
        self.reports.overview(&self.doc)
    }

    /// Marks reports read or unread, deletes them, or marks everything read
    /// (`op`: "read", "unread", "delete", "read_all"): here at once, and at
    /// the service (every window and phone follows).
    pub fn report_request(&mut self, op: &str, ids: &[String]) -> Result<Vec<Effect>, String> {
        if self.conn != ConnState::Online {
            return Err("Offline: the inbox cannot be changed now.".into());
        }
        let json = self.reports.request(op, ids);
        Ok(vec![Effect::Send(ClientMsg::Bus { json }), Effect::Emit(CoreEvent::Reports)])
    }

    // ---------- todos ----------

    /// The canvas's sessions as the todo boards see them.
    fn todo_canvas(&self, now: u64) -> crate::todos::Canvas<'_> {
        let (mut stopped, mut working) = (HashSet::new(), HashSet::new());
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
            let status = self.node_status(node, now);
            if self.have_sessions && matches!(status, Status::Stopped | Status::Failed) {
                stopped.insert(node.id.clone());
            }
            if status.animated() {
                working.insert(node.id.clone());
            }
        }
        crate::todos::Canvas { doc: &self.doc, stopped, working }
    }

    /// The todo boards (the service's book with this phone's changes it does not have yet on top).
    pub fn todos_overview(&self, now: u64) -> TodosOverview {
        crate::todos::overview(self.bus.todos.as_ref(), &self.todo_canvas(now))
    }

    pub fn todo_detail(&self, id: &str, now: u64) -> Option<TodoDetail> {
        crate::todos::detail(self.bus.todos.as_ref()?, id, &self.todo_canvas(now))
    }

    /// Applies a todo operation as you: to this phone's copy of the book at
    /// once, then sent as the window's bus action. Refused here: nothing is
    /// sent. Books the service sends before it has it keep it on top
    /// (`todos::InFlight`); refused by the service: a notice, and it is undone.
    pub fn todo_op(&mut self, op: TodoOp, now: u64) -> Result<(TodoChange, Vec<Effect>), String> {
        if self.conn != ConnState::Online {
            return Err("Offline: the todos cannot be changed now.".into());
        }
        let ctx = self.todo_canvas(now).ctx(now);
        let book = self.bus.todos.as_mut().ok_or("The todo boards are not loaded yet (the service has not sent them).")?;
        // The service's book, kept while changes are in flight.
        let before = self.todos_sent.is_empty().then(|| book.clone());
        let applied = book.apply(op.clone(), &Actor::human(), &ctx)?;
        self.todos_sent.sent(op.clone(), before, book.rev, now);
        let mut message = applied.message.clone();
        for warning in &applied.warnings {
            message.push_str(&format!(" ⚠ {warning}."));
        }
        let id = applied.created.or_else(|| op.todo_id().map(str::to_owned)).unwrap_or_default();
        let effects = vec![Effect::Send(ClientMsg::Bus { json: crate::todos::action_json(&op) }), Effect::Emit(CoreEvent::Overview)];
        Ok((TodoChange { id, message }, effects))
    }

    /// Now and then (every heartbeat): todo changes the service never sent
    /// back go after `todos::PENDING_MS`, and its last book shows with the rest.
    pub fn tick(&mut self, now: u64) -> Vec<Effect> {
        if self.todos_sent.is_empty() {
            return Vec::new();
        }
        let ctx = self.todo_canvas(now).ctx(now);
        match self.todos_sent.tick(&ctx, now) {
            Some(book) => {
                self.bus.todos = Some(book);
                vec![Effect::Emit(CoreEvent::Overview)]
            }
            None => Vec::new(),
        }
    }

    // ---------- notebook ----------

    /// The notebook root setting and the canvas id (`Request::NotebookInfo`).
    pub fn notebook_request(&self) -> (Option<String>, Option<String>) {
        (self.doc.notebook_root.clone().filter(|r| !r.trim().is_empty()), self.doc.canvas_id.clone())
    }

    /// The explorer's roots (none before the service said where the notebook is).
    pub fn note_roots(&self) -> Vec<NotebookRoot> {
        self.notebook.as_ref().map(|info| crate::notes::roots(info, &self.doc)).unwrap_or_default()
    }

    pub fn notebook_overview(&self) -> Option<NotebookOverview> {
        let info = self.notebook.as_ref()?;
        Some(NotebookOverview { root: info.root.clone(), canvas_dir: info.canvas_dir.clone().unwrap_or_default(), roots: self.note_roots() })
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
        self.launch_fork(id, cols, rows, prompt, None, now)
    }

    /// [`State::launch`], forking `fork_from` (an agent's native fork) on the first start.
    pub fn launch_fork(&mut self, id: &str, cols: u16, rows: u16, prompt: Option<String>, fork_from: Option<String>, now: u64) -> Result<Vec<Effect>, String> {
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
            return Err("This session is open in another terminal (CLI or VS Code); Atelier never starts a second copy.".into());
        }
        let cwd = self.launch_folder(id).ok_or("No folder for this session.")?;
        let kind = match &node.command {
            Some(command) if !command.is_empty() => LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() },
            _ => {
                let group = self.doc.group_of(&node).cloned();
                let fork_from = fork_from.filter(|_| node.fresh);
                LaunchKind::Agent(Box::new(agent_launch(&node, &self.doc.defaults, group.as_ref(), prompt, fork_from, !node.fresh)))
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
        let parent = self.doc.node(&request.parent).filter(|n| n.kind == NodeKind::Session).cloned();
        // A fork runs the parent's agent.
        let agent = match (&parent, request.fork) {
            (Some(p), true) => agent_of(p),
            _ => agent,
        };
        let (x, y) = match (&parent, self.doc.group(&request.group)) {
            (Some(p), _) => self.doc.free_slot(p.x + GRID_X + 4, p.y, 1, NodeKind::Session),
            (None, Some(group)) => self.doc.free_slot(group.x + 2, group.y + 2, ((group.w - 4) / GRID_X).max(1), NodeKind::Session),
            (None, None) => {
                let left = self.doc.nodes.iter().map(|n| n.x).min().unwrap_or(0);
                let bottom = self.doc.nodes.iter().map(|n| n.y + n.rect().h).max().map_or(0, |b| b + GRID_Y - 4);
                self.doc.free_slot(left, bottom, 4, NodeKind::Session)
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let node: Node = serde_json::from_value(serde_json::json!({
            "id": id, "kind": "session", "name": name, "x": x, "y": y, "cwd": cwd, "fresh": true,
            "agent": (agent != AgentKind::Copilot).then(|| agent.id()),
            "parent": parent.as_ref().map(|p| p.id.clone()),
            "source": "iphone",
        }))
        .map_err(|e| e.to_string())?;
        self.doc.nodes.push(node);
        Ok(id)
    }

    /// How a subsession of `id` can fork its conversation: Copilot copies the
    /// history on the service (if it has one: `HasHistory`), OpenCode and
    /// Claude Code fork it themselves (a session the service indexed).
    pub fn fork_kind(&self, id: &str) -> Option<Fork> {
        let node = self.doc.node(id).filter(|n| n.kind == NodeKind::Session && n.passive.is_none() && n.command.is_none())?;
        let kind = agent_of(node);
        let native = self.agent_sessions.get(id).cloned().unwrap_or_else(|| native_id(node).to_owned());
        if kind.caps().copy_fork {
            Some(Fork::Copy { agent: kind, parent: native })
        } else if kind.caps().native_fork && self.sessions.contains_key(&native) {
            Some(Fork::Native { parent: native })
        } else {
            None
        }
    }

    /// A passive session's key.
    fn passive_key(&self, id: &str) -> Result<String, String> {
        self.doc.node(id).and_then(|n| n.passive.as_ref()).map(|p| p.key.clone()).ok_or_else(|| "Not a passive session.".to_owned())
    }

    /// Runs a passive session now.
    pub fn run_passive(&self, id: &str) -> Result<Vec<Effect>, String> {
        let key = self.passive_key(id)?;
        Ok(vec![Effect::Send(ClientMsg::Bus { json: serde_json::json!({ "action": "run", "key": key }).to_string() })])
    }

    /// Pauses or resumes a passive session: the canvas field (the desktop's
    /// routing follows it) and the service at once.
    pub fn set_paused(&mut self, id: &str, paused: bool) -> Result<Vec<Effect>, String> {
        let key = self.passive_key(id)?;
        if let Some(passive) = self.doc.node_mut(id).and_then(|n| n.passive.as_mut()) {
            passive.paused = paused;
        }
        let mut effects = vec![Effect::Send(ClientMsg::Bus { json: serde_json::json!({ "action": "pause", "key": key, "paused": paused }).to_string() })];
        if !paused {
            effects.push(Effect::Send(ClientMsg::Bus { json: serde_json::json!({ "action": "resume", "key": key }).to_string() }));
        }
        effects.extend(self.edit());
        effects.push(Effect::Emit(CoreEvent::Overview));
        Ok(effects)
    }

    /// Drops the messages waiting to be typed into a session.
    pub fn discard_mail(&self, id: &str) -> Vec<Effect> {
        vec![Effect::Send(ClientMsg::Bus { json: serde_json::json!({ "action": "discard_mail", "to": id }).to_string() })]
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
        let reply = DocReply { path: r"C:\Users\me\.atelier\canvas.json".into(), client: 7, doc: Some(doc.clone()), error: None };
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
        assert_eq!(state.overview(0).canvas_path, r"C:\Users\me\.atelier\canvas.json");
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
        let request = NewSession { name: String::new(), agent: String::new(), cwd: r"C:\src\repo".into(), prompt: "hi".into(), group: String::new(), parent: String::new(), fork: false };
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

    #[test]
    fn subsessions_sit_next_to_their_parent_and_fork_it() {
        let mut state = online();
        state.info = Some(BackendInfo { default_agent: AgentKind::Copilot, installed: canvas_model::agent::ALL.to_vec(), ..Default::default() });
        // A Claude Code parent the service indexed: a native fork.
        state.doc.node_mut("b").unwrap().agent = Some("claude".into());
        state.on_service(ServiceMsg::Sessions { full: true, upsert: vec![SessionSummary { id: "b".into(), ..Default::default() }], remove: Vec::new() }, 0);
        assert_eq!(state.fork_kind("b"), Some(Fork::Native { parent: "b".into() }));
        assert_eq!(state.fork_kind("a"), Some(Fork::Copy { agent: AgentKind::Copilot, parent: "a".into() }), "Copilot copies the history");
        let request = NewSession { name: "Try another fix".into(), agent: "copilot".into(), cwd: String::new(), prompt: String::new(), group: String::new(), parent: "b".into(), fork: true };
        let id = state.add_session(&request, r"C:\work\app").unwrap();
        let (child, parent) = (state.doc.node(&id).unwrap().clone(), state.doc.node("b").unwrap().clone());
        assert_eq!((child.parent.as_deref(), child.agent.as_deref()), (Some("b"), Some("claude")), "linked, and a fork runs the parent's agent");
        assert!(child.x > parent.x && child.y == parent.y, "placed to the right of its parent");
        let effects = state.launch_fork(&id, 50, 30, None, Some("b".into()), 1).unwrap();
        let ClientMsg::Launch { kind: LaunchKind::Agent(launch), .. } = sent(&effects)[0] else { panic!("launch") };
        assert_eq!((launch.fork_from.as_deref(), launch.resume), (Some("b"), false));
        assert_eq!(state.overview(1).sessions.iter().find(|s| s.id == "b").map(|s| s.forkable), Some(true));
    }

    #[test]
    fn the_inbox_follows_the_service_and_changes_go_out_as_bus_requests() {
        use canvas_model::reports::{Report, Snapshot};
        let mut state = online();
        let report = |id: &str, title: &str| Report { id: id.into(), title: title.into(), kind: "info".into(), source: "a".into(), source_name: "old name".into(), ..Default::default() };
        let inbox = |reports: Vec<Report>| ServiceMsg::Reports { json: serde_json::to_string(&Snapshot { version: 1, total: reports.len() as u32, unread: 0, reports, rejected: Vec::new() }).unwrap() };
        let effects = state.on_service(inbox(vec![report("1", "Brief")]), 0);
        assert_eq!(effects, [Effect::Emit(CoreEvent::Reports)], "the first inbox is not news");
        assert_eq!(state.reports_overview().threads[0].latest.source_name, "Fix flaky test", "named by its box on this canvas");
        let effects = state.on_service(inbox(vec![report("2", "Deploy failed"), report("1", "Brief")]), 0);
        assert!(effects.contains(&Effect::Emit(CoreEvent::ReportArrived { id: "2".into(), title: "Deploy failed".into(), source: "Fix flaky test".into(), more: 0 })));
        let effects = state.report_request("read", &["2".into()]).unwrap();
        assert!(matches!(sent(&effects)[0], ClientMsg::Bus { json } if json.contains("\"op\":\"read\"") && json.contains("\"2\"")));
        assert_eq!(state.reports_overview().unread, 1);
        state.on_disconnected(ConnState::Offline, String::new());
        assert!(state.report_request("delete", &["1".into()]).unwrap_err().contains("Offline"));
    }

    #[test]
    fn passive_sessions_pause_run_and_drop_their_mail() {
        let mut state = online();
        let mut passive = state.doc.node("a").unwrap().clone();
        passive.passive = Some(serde_json::from_value(serde_json::json!({ "key": "drift", "group": "g", "slug": "drift-guard" })).unwrap());
        *state.doc.node_mut("a").unwrap() = passive;
        let bus = |effects: &[Effect]| sent(effects).iter().filter_map(|m| if let ClientMsg::Bus { json } = m { Some(json.clone()) } else { None }).collect::<Vec<_>>();
        assert!(bus(&state.run_passive("a").unwrap())[0].contains("\"run\""));
        let effects = state.set_paused("a", true).unwrap();
        assert!(bus(&effects)[0].contains("\"pause\"") && bus(&effects)[0].contains("true"));
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::DocEdit { ops_json, .. } if ops_json.contains("paused"))), "the desktop's routing follows the canvas");
        assert!(state.overview(0).sessions.iter().find(|s| s.id == "a").unwrap().paused);
        let effects = state.set_paused("a", false).unwrap();
        assert!(bus(&effects).iter().any(|j| j.contains("\"resume\"")));
        assert!(bus(&state.discard_mail("a"))[0].contains("discard_mail"));
        assert!(state.run_passive("b").is_err(), "not a passive session");
    }

    #[test]
    fn todos_come_with_the_bus_and_changes_apply_here_then_go_out_as_the_windows_action() {
        use crate::model::{TodoDraft, TodoKind, TodoNeed, TodoPriority, TodoStatus};
        let mut state = online();
        assert!(!state.todos_overview(0).loaded, "no book yet");
        assert!(state.todo_op(TodoOp::Done { id: "WI-004".into(), note: None }, 0).unwrap_err().contains("not loaded"));
        // The book comes in the bus snapshot; the session working on WI-002 is on this canvas.
        state.doc.nodes.push(node("demo-main", "Momentum work", 80));
        state.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "demo-main".into(), alive: true, ..Default::default() }]), 0);
        state.on_service(ServiceMsg::Sessions { full: true, upsert: vec![SessionSummary { id: "demo-main".into(), live: true, activity: Activity::Working, ..Default::default() }], remove: Vec::new() }, 0);
        let snapshot = serde_json::to_string(&bus::Snapshot { todos: Some(canvas_model::todos::demo_book()), ..Default::default() }).unwrap();
        let effects = state.on_service(ServiceMsg::Bus { json: snapshot.clone() }, 0);
        assert!(effects.contains(&Effect::Emit(CoreEvent::Overview)), "the app reads todos() on Overview");
        let overview = state.todos_overview(0);
        assert!(overview.loaded && overview.boards.len() == 2);
        let momentum = overview.todos.iter().find(|t| t.id == "WI-002").unwrap();
        assert_eq!((momentum.claimed_by.as_str(), momentum.claim_working), ("Momentum work", true));
        let needs: Vec<(TodoNeed, String)> = overview.needs_you.iter().map(|n| (n.need, n.todo.id.clone())).collect();
        assert!(needs.contains(&(TodoNeed::StaleClaim, "WI-003".into())) && !needs.contains(&(TodoNeed::StaleClaim, "WI-002".into())), "{needs:?}");

        // Accepting a proposal: here at once, and the window's bus action for the service.
        let proposal = needs.iter().find(|(n, _)| *n == TodoNeed::Proposed).unwrap().1.clone();
        let (change, effects) = state.todo_op(TodoOp::Accept { id: proposal.clone(), status: None }, 5).unwrap();
        assert!(change.id == proposal && change.message.contains(&proposal), "{change:?}");
        let ClientMsg::Bus { json } = sent(&effects)[0] else { panic!("a bus action") };
        assert_eq!(serde_json::from_str::<serde_json::Value>(json).unwrap(), serde_json::json!({ "action": "todos", "op": { "op": "accept", "id": proposal } }));
        assert!(effects.contains(&Effect::Emit(CoreEvent::Overview)));
        let status = |state: &State, id: &str| state.todos_overview(5).todos.into_iter().find(|t| t.id == id).map(|t| t.status);
        assert_eq!(status(&state, &proposal), Some(TodoStatus::Backlog));
        // Moves, done and new todos; refusals send nothing.
        let (_, effects) = state.todo_op(TodoOp::Move { id: "WI-004".into(), status: Some(TodoStatus::InProgress.into()), board: None, after: None, before: None, rank: None }, 6).unwrap();
        assert!(sent(&effects).iter().any(|m| matches!(m, ClientMsg::Bus { json } if json.contains("\"op\":\"move\"") && json.contains("\"in_progress\""))));
        let (change, _) = state.todo_op(TodoOp::Move { id: "WI-005".into(), status: Some(TodoStatus::InProgress.into()), board: None, after: None, before: None, rank: None }, 6).unwrap();
        assert!(change.message.contains('⚠'), "over the WIP limit of 2: a warning, not a refusal: {}", change.message);
        state.todo_op(TodoOp::Done { id: "WI-004".into(), note: None }, 7).unwrap();
        assert_eq!(status(&state, "WI-004"), Some(TodoStatus::Done));
        let draft = TodoDraft { board: "Canvas App".into(), title: "Todos on the phone".into(), kind: TodoKind::Feature, priority: TodoPriority::High, status: None, body: "From the train.".into(), blocker: false };
        let (change, effects) = state.todo_op(crate::todos::add_op(draft), 8).unwrap();
        assert_eq!(change.id, "WI-008");
        assert!(matches!(sent(&effects)[0], ClientMsg::Bus { json } if json.contains("\"op\":\"add\"") && json.contains("Todos on the phone")));
        assert_eq!(state.todo_detail("WI-008", 8).map(|d| (d.item.status, d.body)), Some((TodoStatus::Backlog, "From the train.".to_owned())));
        assert!(state.todo_op(TodoOp::Done { id: "NOPE-001".into(), note: None }, 9).is_err());
        // A book the service sent before it had them keeps them on top; once
        // they have been out for `PENDING_MS` without coming back, its book stands.
        state.on_service(ServiceMsg::Bus { json: snapshot.clone() }, 10);
        assert_eq!((status(&state, &proposal), status(&state, "WI-008")), (Some(TodoStatus::Backlog), Some(TodoStatus::Backlog)));
        state.on_service(ServiceMsg::Bus { json: snapshot }, 8 + crate::todos::PENDING_MS);
        assert_eq!((status(&state, &proposal), status(&state, "WI-008")), (Some(TodoStatus::Proposed), None));
        state.on_disconnected(ConnState::Offline, String::new());
        assert!(state.todo_op(TodoOp::Done { id: "WI-004".into(), note: None }, 11).unwrap_err().contains("Offline"));
    }

    /// A bus snapshot from the service with its todo book.
    fn book_message(book: &canvas_model::todos::TodoBook) -> ServiceMsg {
        ServiceMsg::Bus { json: serde_json::to_string(&bus::Snapshot { todos: Some(book.clone()), ..Default::default() }).unwrap() }
    }

    /// The race the end-to-end test met: the service (the mock as the real
    /// one) applies this phone's changes in order and broadcasts its book
    /// after each, so the book after the first arrives while the second is
    /// still on its way. Replacing the phone's copy with it dropped the
    /// second change, and the next one (a tick of the item the second added,
    /// by its place) was refused here as out of range.
    #[test]
    fn a_book_sent_before_the_service_had_this_phones_changes_never_undoes_them() {
        use canvas_model::todos::{Ctx, TodoBook, TodoUpdate};
        let mut state = online();
        let first = canvas_model::todos::demo_book();
        state.on_service(book_message(&first), 0);
        let checks = |state: &State| state.todo_detail("WI-002", 0).unwrap().checklist.into_iter().map(|c| (c.text, c.done)).collect::<Vec<_>>();
        let title = |state: &State| state.todo_detail("WI-002", 0).unwrap().item.title;
        let items = checks(&state).len();
        // Two changes in flight: a new title, then a new checklist item.
        let rename = TodoOp::Update(Box::new(TodoUpdate { id: "WI-002".into(), title: Some("Glide after a flick".into()), ..Default::default() }));
        let add = TodoOp::AddCheck { id: "WI-002".into(), text: "Try it on the phone".into() };
        state.todo_op(rename.clone(), 1).unwrap();
        state.todo_op(add.clone(), 2).unwrap();
        // The service's book after the first only: the second stays on top, so its item can be ticked.
        let (you, ctx) = (Actor::human(), Ctx::at(1));
        let mut service: TodoBook = first.clone();
        service.apply(rename, &you, &ctx).unwrap();
        state.on_service(book_message(&service), 3);
        assert_eq!((title(&state).as_str(), checks(&state).len()), ("Glide after a flick", items + 1), "both changes shown");
        let tick = TodoOp::Check { id: "WI-002".into(), index: items, done: true };
        state.todo_op(tick.clone(), 4).expect("the new item is there to tick");
        // A bus pass that read the book before all three: none undone, none applied twice.
        state.on_service(book_message(&first), 5);
        let all_three = |state: &State, why: &str| {
            let checks = checks(state);
            assert_eq!((title(state).as_str(), checks.len(), checks.last().cloned()), ("Glide after a flick", items + 1, Some(("Try it on the phone".to_owned(), true))), "{why}");
        };
        all_three(&state, "a book from before all three");
        // The service's book with all three: shown as it is.
        service.apply(add, &you, &ctx).unwrap();
        service.apply(tick, &you, &ctx).unwrap();
        state.on_service(book_message(&service), 6);
        assert_eq!(state.bus.todos.as_ref(), Some(&service));
        // A late book from before all three still undoes none.
        state.on_service(book_message(&first), 7);
        all_three(&state, "a late, older book");
        // Changes never sent back go after `PENDING_MS` (on a heartbeat): the service's last word stands.
        assert!(state.tick(1 + crate::todos::PENDING_MS - 1).is_empty(), "not yet");
        assert_eq!(state.tick(4 + crate::todos::PENDING_MS), [Effect::Emit(CoreEvent::Overview)]);
        assert_eq!(state.bus.todos.as_ref(), Some(&first), "the service's last book");
        assert!(state.tick(5 + crate::todos::PENDING_MS).is_empty(), "nothing in flight");
    }

    #[test]
    fn a_change_the_service_refuses_is_undone_at_its_notice() {
        use crate::model::TodoStatus;
        let mut state = online();
        let first = canvas_model::todos::demo_book();
        state.on_service(book_message(&first), 0);
        let status = |state: &State, id: &str| state.todo_detail(id, 0).map(|t| t.item.status);
        state.todo_op(TodoOp::Done { id: "WI-004".into(), note: None }, 1).unwrap();
        state.todo_op(TodoOp::Reopen { id: "WI-004".into() }, 2).unwrap();
        state.todo_op(TodoOp::Done { id: "WI-004".into(), note: None }, 3).unwrap();
        // Another error leaves the changes alone.
        state.on_service(ServiceMsg::Error { id: None, message: "The canvas could not be saved".into() }, 4);
        assert_eq!(status(&state, "WI-004"), Some(TodoStatus::Done));
        // The service refused one: its notice undoes them all at once, then its book as it is comes.
        let refused = "Todo change refused: WI-004 is claimed by another session";
        let effects = state.on_service(ServiceMsg::Error { id: None, message: refused.into() }, 5);
        assert!(effects.contains(&Effect::Emit(CoreEvent::Notice { text: refused.into(), error: true })) && effects.contains(&Effect::Emit(CoreEvent::Overview)));
        assert_eq!(state.bus.todos.as_ref(), Some(&first));
        state.on_service(book_message(&first), 6);
        assert_eq!(status(&state, "WI-004"), Some(TodoStatus::Todo));
        assert!(state.tick(6 + crate::todos::PENDING_MS).is_empty(), "nothing left in flight");
    }

    #[test]
    fn the_notebooks_roots_come_from_the_service_and_the_canvas() {
        use crate::model::NotebookRootKind;
        let mut state = online();
        assert!(state.note_roots().is_empty() && state.notebook_overview().is_none());
        state.doc.notebook_root = Some(r"D:\Notes".into());
        state.doc.canvas_id = Some("c-1".into());
        state.doc.notebook_mounts = vec![r"E:\Vault".into()];
        assert_eq!(state.notebook_request(), (Some(r"D:\Notes".to_owned()), Some("c-1".to_owned())));
        state.notebook = Some(canvas_model::notebook::Info { root: r"D:\Notes".into(), canvas_dir: None, home: r"C:\Users\me\.atelier".into() });
        let overview = state.notebook_overview().unwrap();
        assert_eq!((overview.root.as_str(), overview.canvas_dir.as_str()), (r"D:\Notes", ""));
        assert_eq!(overview.roots.iter().map(|r| r.kind).collect::<Vec<_>>(), [NotebookRootKind::Notebook, NotebookRootKind::Mount]);
        state.on_connected();
        assert!(state.notebook.is_none(), "asked for again after a reconnect");
    }
}
