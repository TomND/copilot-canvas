//! Client application state and core behavior. Input handling lives in
//! `events.rs`, dialogs/actions in `actions.rs`, drawing in `ui.rs`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::layout::Rect;

use crate::conn::Conn;
use crate::dialogs::{Hit, Modal};
use crate::doc::{Doc, GRID_X, Node, NodeKind, Store};
use crate::status::{self, Status};
use crate::git::GitInfo;
use crate::paths::now_ms;
use crate::protocol::{ClientMsg, Input, LaunchKind, ServiceMsg, ServiceStats, SessionSummary, TermInfo};

pub enum AppEvent {
    Git { req: u64, folder: String, result: Result<Option<GitInfo>, String> },
    Worktree { node: String, result: Result<(), String> },
    Forked { node: String, result: Result<(), String> },
    Config(Box<crate::copilot_cfg::Snapshot>),
    Automations(crate::factory_client::AutoLoad),
    CliDone { label: String, result: Result<String, String> },
    VsSync(Vec<crate::vscode::SyncCandidate>, Vec<String>),
    MigratePlan(Vec<crate::migrate::Item>),
    MigrateDone(Result<Vec<String>, String>),
    /// The terminal service was restarted (new build); `mouse`: Copilot's mouse
    /// setting was switched back on.
    ServiceRestarted { result: Result<Conn, String>, mouse: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Sidebar,
    Canvas,
    Terminal,
}

pub use canvas_termview::{TermSelection, TermView, user_message_rows};

pub struct Notice {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

pub enum Drag {
    Nodes { ids: Vec<String>, start: (u16, u16), applied: (i32, i32), moved: bool, click: Option<String> },
    Group { id: String, start: (u16, u16), applied: (i32, i32), moved: bool },
    Resize { id: String, start: (u16, u16), orig: (i32, i32) },
    Pan { last: (u16, u16), moved: bool },
    Rubber { start: (u16, u16), now: (u16, u16), additive: bool, right: bool },
    SidebarDivider,
    TermDivider,
    /// Selecting text in a terminal pane.
    Select { id: String, anchor: (u16, u16), moved: bool },
}

#[derive(Default, Clone)]
pub struct Layout {
    pub screen: Rect,
    pub toolbar: Rect,
    pub sidebar: Rect,
    pub canvas: Rect,
    pub terms: Rect,
    pub status: Rect,
    pub tiles: Vec<(String, Rect, Rect)>,
    pub sidebar_rows: Vec<(u16, usize)>,
    pub sidebar_filter_row: Option<u16>,
    pub sidebar_divider: Option<u16>,
    pub term_divider: Option<u16>,
    pub hits: Vec<(Rect, Hit)>,
    /// Clickable links on screen (rebuilt every frame).
    pub links: Vec<crate::links::Link>,
    pub modal_rect: Option<Rect>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideKind {
    Group(String),
    Session(String),
    Agent(String),
    Section,
}

#[derive(Clone, Debug)]
pub struct SideRow {
    pub kind: SideKind,
    pub label: String,
    pub depth: u16,
    pub status: Option<Status>,
    pub count: Option<usize>,
    pub collapsed: bool,
}

pub enum CanvasHit {
    Node(String),
    GroupTitle(String),
    GroupResize(String),
    GroupBody,
    Empty,
}

pub const ZOOMS: [(i32, i32); 3] = [(1, 1), (1, 2), (1, 4)];

pub struct App {
    pub demo: bool,
    pub perf: Option<Perf>,
    pub term_select: Option<TermSelection>,
    /// Canvas before the last bulk change (VS Code sync), for Ctrl+Z.
    pub undo: Option<(String, Box<Doc>)>,
    /// While wheel/trackpad panning, the dot grid is hidden until this time.
    pub pan_quiet_until: Option<Instant>,
    /// Ctrl+L: repaint every cell on the next frame.
    pub repaint_requested: bool,
    /// When you last typed into each terminal (for idle auto-stop).
    pub last_input_at: HashMap<String, Instant>,
    pub idle_check_at: Instant,
    pub default_cwd: String,
    pub doc: Doc,
    pub store: Store,
    pub dirty_at: Option<Instant>,
    pub save_error: Option<String>,
    pub conn: Conn,
    pub connected: bool,
    pub sessions: HashMap<String, SessionSummary>,
    pub sessions_loaded: bool,
    pub terms: HashMap<String, TermInfo>,
    pub views: HashMap<String, TermView>,
    pub launching: HashMap<String, Instant>,
    pub preparing: HashSet<String>,
    pub node_errors: HashMap<String, String>,
    pub pending_prompts: HashMap<String, String>,
    pub focus: Focus,
    pub focused_term: Option<String>,
    pub tile_slots: Vec<String>,
    pub selection: Vec<String>,
    pub selected_group: Option<String>,
    pub flash: Option<(String, Instant)>,
    pub drag: Option<Drag>,
    pub modals: Vec<Modal>,
    pub notice: Option<Notice>,
    pub side_rows: Vec<SideRow>,
    /// Fingerprint of what `side_rows` was built from.
    pub side_rows_key: u64,
    /// Bumped on every session-history / terminal update from the service.
    pub sessions_rev: u64,
    pub terms_rev: u64,
    pub side_scroll: usize,
    pub side_sel: usize,
    pub side_filter: crate::dialogs::TextInput,
    pub side_filter_active: bool,
    pub layout: Layout,
    pub last_click: Option<(Instant, u16, u16)>,
    pub app_tx: Sender<AppEvent>,
    pub quit: bool,
    pub tick: usize,
    pub git_req: u64,
    pub last_subscribed: Vec<String>,
    pub last_sizes: HashMap<String, (u16, u16)>,
    pub last_tracked: Vec<String>,
    pub stats: Option<ServiceStats>,
    pub stats_at: Instant,
    pub build_mismatch: bool,
    pub needs_draw: bool,
    pub auto_imported: bool,
    pub instance_home: std::path::PathBuf,
    pub instance: Option<crate::paths::Instance>,
    /// Receiver of a new service connection for the run loop to switch to.
    pub new_service_rx: Option<crossbeam_channel::Receiver<crate::protocol::ServiceMsg>>,
    /// Sessions to resume once the restarted service is up.
    pub restarting: Option<Vec<String>>,
    /// Raw bytes for the outer terminal, sent before the next frame (bell,
    /// taskbar progress).
    pub raw_out: Vec<u8>,
    /// Progress state last mirrored to the outer terminal's tab and taskbar.
    pub outer_progress: u8,
    pub progress_check_at: Instant,
    pub last_bell: Option<Instant>,
    /// Passive sessions, mailbox and recent events from the service.
    pub bus: crate::bus::Snapshot,
    /// Routing last sent to the service (JSON).
    pub routing_sent: String,
    pub routing_check_at: Instant,
    /// Event pulses on the canvas: (from, to, start).
    pub pulses: Vec<(String, String, Instant)>,
    pub side_view: crate::side::SideView,
    pub list_sel: usize,
    pub list_scroll: usize,
    pub config: Option<crate::copilot_cfg::Snapshot>,
    pub config_loading: bool,
    pub automations: Vec<crate::automations::Automation>,
    pub automations_loading: bool,
    pub automations_at: Instant,
    pub automations_error: Option<String>,
    /// The manifest folder loaded (task boxes are only removed when it did).
    pub automations_factory_ok: bool,
    pub right_detail: Option<crate::side::RightDetail>,
    pub cli_busy: Option<String>,
    pub vs_syncing: bool,
    pub auto_synced: bool,
    /// Group whose session defaults the Settings view edits (None = canvas defaults).
    pub settings_group: Option<String>,
}

impl App {
    pub fn new(conn: Conn, store: Store, doc: Doc, demo: bool, default_cwd: String, app_tx: Sender<AppEvent>, instance_home: std::path::PathBuf) -> Self {
        let build_mismatch = conn.service_build != crate::paths::build_id();
        let selection = doc.selected.clone().into_iter().collect();
        let mut app = Self {
            demo,
            default_cwd,
            doc,
            store,
            dirty_at: None,
            save_error: None,
            conn,
            connected: true,
            sessions: HashMap::new(),
            sessions_loaded: false,
            terms: HashMap::new(),
            views: HashMap::new(),
            launching: HashMap::new(),
            preparing: HashSet::new(),
            node_errors: HashMap::new(),
            pending_prompts: HashMap::new(),
            focus: Focus::Canvas,
            focused_term: None,
            tile_slots: Vec::new(),
            selection,
            selected_group: None,
            flash: None,
            drag: None,
            modals: Vec::new(),
            notice: None,
            side_rows: Vec::new(),
            side_scroll: 0,
            side_sel: 0,
            side_filter: Default::default(),
            side_filter_active: false,
            layout: Layout::default(),
            last_click: None,
            app_tx,
            quit: false,
            tick: 0,
            git_req: 0,
            last_subscribed: Vec::new(),
            last_sizes: HashMap::new(),
            last_tracked: Vec::new(),
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(60),
            last_input_at: HashMap::new(),
            idle_check_at: Instant::now(),
            repaint_requested: false,
            pan_quiet_until: None,
            side_rows_key: 0,
            sessions_rev: 0,
            terms_rev: 0,
            undo: None,
            term_select: None,
            perf: None,
            build_mismatch,
            needs_draw: true,
            auto_imported: false,
            instance_home,
            side_view: crate::side::SideView::Sessions,
            list_sel: 0,
            list_scroll: 0,
            config: None,
            config_loading: false,
            automations: Vec::new(),
            automations_loading: false,
            automations_at: Instant::now() - Duration::from_secs(60),
            automations_error: None,
            automations_factory_ok: false,
            right_detail: None,
            cli_busy: None,
            vs_syncing: false,
            auto_synced: false,
            settings_group: None,
            instance: None,
            new_service_rx: None,
            restarting: None,
            raw_out: Vec::new(),
            outer_progress: 0,
            progress_check_at: Instant::now(),
            last_bell: None,
            bus: crate::bus::Snapshot::default(),
            routing_sent: String::new(),
            routing_check_at: Instant::now(),
            pulses: Vec::new(),
        };
        if app.build_mismatch {
            app.notify("The terminal service is from an older build: click ⟳ Update service in the toolbar to get its fixes.", false);
        }
        app
    }

    // ---------- notices & persistence ----------

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        let text: String = text.into();
        let secs = if error { 10 } else { 5 };
        self.notice = Some(Notice { text: text.replace(['\n', '\r'], " "), error, until: Instant::now() + Duration::from_secs(secs) });
        self.needs_draw = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty_at.get_or_insert_with(Instant::now);
        self.needs_draw = true;
    }

    pub fn save_now(&mut self) {
        self.doc.selected = self.selection.last().cloned();
        match self.store.save(&self.doc) {
            Ok(()) => {
                self.dirty_at = None;
                self.save_error = None;
            }
            Err(error) => {
                self.save_error = Some(format!("{error:#}"));
                self.dirty_at = Some(Instant::now());
            }
        }
    }

    pub fn send(&mut self, message: ClientMsg) {
        if let ClientMsg::Input { id, .. } | ClientMsg::Wheel { id, .. } = &message {
            self.last_input_at.insert(id.clone(), Instant::now());
        }
        if self.conn.tx.try_send(message).is_err() && self.connected && self.restarting.is_none() {
            self.connected = false;
            self.notify("Lost connection to the terminal service. Restart Copilot Canvas to reconnect.", true);
        }
    }

    // ---------- geometry ----------

    pub fn scale(&self) -> (i32, i32) {
        ZOOMS[(self.doc.view.zoom as usize).min(ZOOMS.len() - 1)]
    }

    pub fn to_screen(&self, wx: i32, wy: i32) -> (i32, i32) {
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        (c.x as i32 + ((wx - self.doc.view.x) * n).div_euclid(d), c.y as i32 + ((wy - self.doc.view.y) * n).div_euclid(d))
    }

    pub fn to_world(&self, sx: i32, sy: i32) -> (i32, i32) {
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        (self.doc.view.x + (sx - c.x as i32) * d / n, self.doc.view.y + (sy - c.y as i32) * d / n)
    }

    pub fn scaled(&self, w: i32, h: i32, min_w: i32, min_h: i32) -> (i32, i32) {
        let (n, d) = self.scale();
        ((w * n / d).max(min_w), (h * n / d).max(min_h))
    }

    pub fn node_screen(&self, node: &Node) -> (i32, i32, i32, i32) {
        let r = node.rect();
        let (x, y) = self.to_screen(r.x, r.y);
        let (w, h) = self.scaled(r.w, r.h, 4, 1);
        (x, y, w, h)
    }

    pub fn group_screen(&self, g: &crate::doc::Group) -> (i32, i32, i32, i32) {
        let (x, y) = self.to_screen(g.x, g.y);
        let (w, h) = self.scaled(g.w, g.h, 6, 2);
        (x, y, w, h)
    }

    pub fn canvas_hit(&self, sx: u16, sy: u16) -> CanvasHit {
        let (sx, sy) = (sx as i32, sy as i32);
        let inside = |x: i32, y: i32, w: i32, h: i32| sx >= x && sx < x + w && sy >= y && sy < y + h;
        let mut order: Vec<&Node> = self.doc.nodes.iter().collect();
        order.sort_by_key(|n| self.selection.contains(&n.id));
        for node in order.into_iter().rev() {
            let (x, y, w, h) = self.node_screen(node);
            if inside(x, y, w, h) {
                return CanvasHit::Node(node.id.clone());
            }
        }
        let mut groups: Vec<&crate::doc::Group> = self.doc.groups.iter().collect();
        groups.sort_by_key(|g| std::cmp::Reverse((g.w as i64) * (g.h as i64)));
        for group in groups.into_iter().rev() {
            let (x, y, w, h) = self.group_screen(group);
            if sx >= x + w - 2 && sx < x + w && sy == y + h - 1 {
                return CanvasHit::GroupResize(group.id.clone());
            }
            if sy == y && sx >= x && sx < x + w {
                return CanvasHit::GroupTitle(group.id.clone());
            }
            if inside(x, y, w, h) {
                return CanvasHit::GroupBody;
            }
        }
        CanvasHit::Empty
    }

    pub fn center_on(&mut self, wx: i32, wy: i32) {
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        let (cw, ch) = (c.width.max(20) as i32 * d / n, c.height.max(10) as i32 * d / n);
        self.doc.view.x = wx - cw / 2;
        self.doc.view.y = wy - ch / 2;
        self.mark_dirty();
    }

    pub fn reveal_node(&mut self, id: &str) {
        let Some(node) = self.doc.node(id) else { return };
        let (cx, cy) = node.rect().center();
        self.center_on(cx, cy);
        self.flash = Some((id.to_owned(), Instant::now()));
    }

    pub fn reveal_group(&mut self, id: &str) {
        let Some(group) = self.doc.group(id).cloned() else { return };
        let c = self.layout.canvas;
        for zoom in 0..ZOOMS.len() as u8 {
            let (n, d) = ZOOMS[zoom as usize];
            if group.w * n / d <= c.width as i32 - 2 && group.h * n / d < c.height as i32 {
                if zoom > self.doc.view.zoom {
                    self.doc.view.zoom = zoom;
                }
                break;
            }
            if zoom as usize == ZOOMS.len() - 1 {
                self.doc.view.zoom = zoom;
            }
        }
        let (cx, cy) = group.rect().center();
        self.center_on(cx, cy);
        self.flash = Some((id.to_owned(), Instant::now()));
    }

    pub fn fit_all(&mut self) {
        let Some(bounds) = self.doc.bounds() else { return };
        let c = self.layout.canvas;
        self.doc.view.zoom = (ZOOMS.len() - 1) as u8;
        for (zoom, (n, d)) in ZOOMS.iter().enumerate() {
            if bounds.w * n / d <= c.width as i32 - 2 && bounds.h * n / d < c.height as i32 {
                self.doc.view.zoom = zoom as u8;
                break;
            }
        }
        let (cx, cy) = bounds.center();
        self.center_on(cx, cy);
    }

    pub fn zoom_at(&mut self, sx: i32, sy: i32, delta: i32) {
        let before = self.to_world(sx, sy);
        let zoom = (self.doc.view.zoom as i32 + delta).clamp(0, ZOOMS.len() as i32 - 1) as u8;
        if zoom == self.doc.view.zoom {
            return;
        }
        self.doc.view.zoom = zoom;
        let after = self.to_world(sx, sy);
        self.doc.view.x += before.0 - after.0;
        self.doc.view.y += before.1 - after.1;
        self.mark_dirty();
    }

    // ---------- status ----------

    pub fn session_of(&self, node_id: &str) -> Option<String> {
        let node = self.doc.node(node_id)?;
        match node.kind {
            NodeKind::Session => Some(node.id.clone()),
            NodeKind::Agent => node.parent.clone(),
            NodeKind::Automation => None,
        }
    }

    pub fn owned_alive(&self, id: &str) -> bool {
        self.terms.get(id).is_some_and(|t| t.alive)
    }

    /// Running in another terminal (CLI or VS Code) rather than in this service.
    pub fn live_elsewhere(&self, node: &Node) -> bool {
        node.kind == NodeKind::Session && !self.owned_alive(&node.id) && self.sessions.get(&node.id).is_some_and(|s| s.live)
    }

    pub fn node_status(&self, node: &Node) -> Status {
        let now = now_ms();
        match node.kind {
            NodeKind::Session if node.passive.is_some() => {
                // A passive session shows its reported state; runs show as working.
                let view = self.passive_view(node);
                if view.is_some_and(|v| v.running) || self.owned_alive(&node.id) && status::session_status(self.sessions.get(&node.id), self.terms.get(&node.id), node.reviewed_ms) == Status::Working {
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
                if self.preparing.contains(&node.id) || self.launching.contains_key(&node.id) {
                    return Status::Preparing;
                }
                if self.node_errors.contains_key(&node.id) && !self.owned_alive(&node.id) {
                    return Status::Failed;
                }
                status::session_status(self.sessions.get(&node.id), self.terms.get(&node.id), node.reviewed_ms)
            }
            NodeKind::Automation => match node.automation.as_deref().and_then(|k| self.automation(k)) {
                Some(automation) => crate::side::automation_status(automation),
                None => Status::Unknown,
            },
            NodeKind::Agent => {
                let parent = node.parent.as_deref().unwrap_or_default();
                let summary = self.sessions.get(parent);
                let running = self.owned_alive(parent) || summary.is_some_and(|s| s.live);
                match summary.and_then(|s| s.agents.iter().find(|a| Some(&a.key) == node.agent_key.as_ref())) {
                    Some(agent) => status::agent_status(agent.status, agent.finished_ms, running, now),
                    None if self.demo => Status::JustFinished,
                    None => Status::Unknown,
                }
            }
        }
    }

    pub fn subtitle(&self, node: &Node) -> String {
        if let Some(error) = self.node_errors.get(&node.id) {
            return error.clone();
        }
        if node.kind == NodeKind::Agent {
            return "subagent".into();
        }
        if node.kind == NodeKind::Automation {
            if let Some(subtitle) = node.automation.as_deref().and_then(|k| self.automation(k)).and_then(crate::factory_client::task_subtitle) {
                return subtitle;
            }
            return match node.automation.as_deref().and_then(|k| self.automation(k)) {
                Some(a) if a.enabled => format!("{} · next {}", a.schedule, crate::side::when(&a.next_run_at)),
                Some(_) => "disabled".into(),
                None => "automation".into(),
            };
        }
        if let Some(summary) = self.sessions.get(&node.id)
            && !summary.branch.is_empty()
        {
            let folder = summary.cwd.rsplit(['\\', '/']).next().unwrap_or_default();
            return format!("{folder} · {}", summary.branch);
        }
        let cwd = node.cwd.as_deref().or_else(|| self.sessions.get(&node.id).map(|s| s.cwd.as_str())).unwrap_or_default();
        cwd.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(cwd).to_owned()
    }

    // ---------- selection & focus ----------

    pub fn primary(&self) -> Option<&Node> {
        self.selection.last().and_then(|id| self.doc.node(id))
    }

    pub fn set_focus(&mut self, focus: Focus) {
        if self.focus == focus {
            return;
        }
        if self.focus == Focus::Terminal
            && let Some(id) = self.focused_term.clone()
            && self.owned_alive(&id)
        {
            self.send(ClientMsg::Input { id, input: Input::Focus(false) });
        }
        self.focus = focus;
        if focus == Focus::Terminal
            && let Some(id) = self.focused_term.clone()
            && self.owned_alive(&id)
        {
            self.send(ClientMsg::Input { id, input: Input::Focus(true) });
        }
        self.needs_draw = true;
    }

    pub fn select_node(&mut self, id: &str, additive: bool) {
        if additive {
            if let Some(pos) = self.selection.iter().position(|s| s == id) {
                self.selection.remove(pos);
            } else {
                self.selection.push(id.to_owned());
            }
        } else {
            self.selection = vec![id.to_owned()];
        }
        self.selected_group = None;
        self.right_detail = self.doc.node(id).and_then(|n| match (&n.automation, &n.passive) {
            (Some(key), _) => Some(crate::side::RightDetail::Automation(key.clone())),
            (_, Some(_)) if !additive => Some(crate::side::RightDetail::Passive(n.id.clone())),
            _ => None,
        });
        if let Some(session) = self.session_of(id) {
            if !additive {
                self.mark_reviewed(&session);
            }
            if self.terms.contains_key(&session) {
                self.focused_term = Some(session);
            }
        }
        if let Some(index) = self.side_rows.iter().position(|r| matches!(&r.kind, SideKind::Session(s) | SideKind::Agent(s) if s == id)) {
            self.side_sel = index;
        }
        self.mark_dirty();
    }

    pub fn select_group(&mut self, id: &str) {
        self.selection.clear();
        self.selected_group = Some(id.to_owned());
        if let Some(index) = self.side_rows.iter().position(|r| r.kind == SideKind::Group(id.to_owned())) {
            self.side_sel = index;
        }
        self.needs_draw = true;
    }

    /// Mirrors the canvas to the outer terminal's tab and taskbar (OSC 9;4), as
    /// Copilot does in a plain terminal: busy while any session works, paused
    /// (yellow) while one needs you.
    pub fn mirror_progress(&mut self) {
        self.progress_check_at = Instant::now();
        let mut state = 0;
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
            match self.node_status(node) {
                Status::Waiting => {
                    state = 4;
                    break;
                }
                Status::Working => state = 3,
                _ => {}
            }
        }
        if state != self.outer_progress {
            self.outer_progress = state;
            let value = if state == 4 { 100 } else { 0 };
            self.raw_out.extend_from_slice(format!("\x1b]9;4;{state};{value}\x07").as_bytes());
            self.needs_draw = true;
        }
    }

    /// A session started inside `parent`'s terminal: show it as a linked
    /// subsession (added next to its parent, or linked if already on the canvas).
    pub fn link_subsession(&mut self, parent: &str, child: &str, name: &str, cwd: &str) {
        if child == parent || self.doc.node(parent).is_none() {
            return;
        }
        if let Some(node) = self.doc.node_mut(child) {
            if node.parent.is_none() && node.kind == NodeKind::Session {
                node.parent = Some(parent.to_owned());
                self.mark_dirty();
            }
            return;
        }
        let Some(p) = self.doc.node(parent).cloned() else { return };
        let (x, y) = self.doc.free_slot(p.x + GRID_X + 4, p.y, 1, NodeKind::Session);
        let name = if name.trim().is_empty() { format!("Subsession {}", &child[..child.len().min(8)]) } else { name.trim().to_owned() };
        self.doc.nodes.push(Node {
            id: child.to_owned(),
            kind: NodeKind::Session,
            name: name.clone(),
            x,
            y,
            cwd: (!cwd.is_empty()).then(|| cwd.to_owned()),
            parent: Some(parent.to_owned()),
            agent_key: None,
            source: Some("spawned".into()),
            command: None,
            fresh: false,
            automation: None,
            synced_name: None,
            reviewed_ms: None,
            mcp_off: None, passive: None, no_messages: false,
        });
        self.mark_dirty();
        self.notify(format!("“{}” started subsession “{name}”.", p.name), false);
    }

    /// "every 15m" when this box repeats on a schedule that is active now: a live
    /// session with a recurring schedule (`/every`), Copilot's saved session
    /// schedule, or an enabled automation.
    pub fn node_loop(&self, node: &Node) -> Option<String> {
        match node.kind {
            NodeKind::Session if node.passive.is_some() => {
                let passive = node.passive.as_ref()?;
                let done = self.passive_view(node).is_some_and(|v| v.status == "done");
                (!passive.paused && !done).then(|| Self::passive_trigger(passive))
            }
            NodeKind::Session => {
                let live = self.owned_alive(&node.id) || self.sessions.get(&node.id).is_some_and(|s| s.live);
                if let Some(summary) = self.sessions.get(&node.id).filter(|s| live && s.loop_ms > 0) {
                    return Some(format!("every {}", short_interval(summary.loop_ms)));
                }
                self.automation(&format!("session:{}", node.id)).filter(|a| a.enabled && repeats(&a.schedule)).map(|a| a.schedule.clone())
            }
            NodeKind::Automation => {
                // Scheduled tasks all repeat; the loop arc is for sessions that loop.
                node.automation.as_deref().and_then(|k| self.automation(k)).filter(|a| a.factory.is_none() && a.enabled && repeats(&a.schedule)).map(|a| a.schedule.clone())
            }
            NodeKind::Agent => None,
        }
    }

    /// You looked at this session: clears its "done · review" state.
    pub fn mark_reviewed(&mut self, id: &str) {
        let finished = self.sessions.get(id).and_then(|s| s.finished_ms);
        if let Some(node) = self.doc.node_mut(id)
            && node.kind == NodeKind::Session
            && (node.reviewed_ms.is_none() || finished.is_some_and(|f| Some(f) > node.reviewed_ms))
        {
            node.reviewed_ms = Some(now_ms());
            self.mark_dirty();
        }
    }

    /// Gives sessions that have never been looked at a baseline, so only turns
    /// finishing from now on (or while the canvas was closed) ask for review.
    fn baseline_reviewed(&mut self) {
        let now = now_ms();
        let mut changed = false;
        for node in &mut self.doc.nodes {
            if node.kind == NodeKind::Session && node.reviewed_ms.is_none() {
                node.reviewed_ms = Some(now);
                changed = true;
            }
        }
        if changed {
            self.mark_dirty();
        }
    }

    /// Shows a terminal, keeping tile positions stable.
    pub fn show_terminal(&mut self, id: &str) {
        self.mark_reviewed(id);
        self.right_detail = None;
        self.focused_term = Some(id.to_owned());
        self.doc.touch_tile(id);
        if !self.tile_slots.iter().any(|t| t == id) {
            if self.tile_slots.len() < 4 {
                self.tile_slots.push(id.to_owned());
            } else {
                let rank = |t: &String| self.doc.tiles.iter().position(|x| x == t).unwrap_or(usize::MAX);
                if let Some((slot, _)) = self.tile_slots.iter().enumerate().max_by_key(|(_, t)| rank(t)) {
                    self.tile_slots[slot] = id.to_owned();
                }
            }
        }
        self.mark_dirty();
    }

    pub fn visible_terminals(&self) -> Vec<String> {
        // Details (automations, configuration items) take over the right panel.
        if self.right_detail.is_some() {
            return Vec::new();
        }
        if self.doc.ui.tiled {
            let mut slots: Vec<String> = self.tile_slots.iter().filter(|id| self.terms.contains_key(*id)).cloned().collect();
            if slots.is_empty()
                && let Some(id) = &self.focused_term
                && self.terms.contains_key(id)
            {
                slots.push(id.clone());
            }
            slots
        } else {
            // A selected group shows its own panel (folders, defaults, members).
            if self.selected_group.is_some() && self.selection.is_empty() {
                return Vec::new();
            }
            let selected_session = self.selection.last().and_then(|id| self.session_of(id));
            let id = match selected_session {
                Some(session) if self.terms.contains_key(&session) => Some(session),
                Some(_) => None,
                None => self.focused_term.clone().filter(|id| self.terms.contains_key(id)),
            };
            id.into_iter().collect()
        }
    }

    pub fn pane_size(&self) -> (u16, u16) {
        let tiles = &self.layout.tiles;
        if let Some((_, _, inner)) = tiles.first() {
            return (inner.width.max(20), inner.height.max(5));
        }
        let t = self.layout.terms;
        if t.width > 4 { (t.width - 2, t.height.saturating_sub(2).max(5)) } else { (100, 30) }
    }

    // ---------- launching ----------

    pub fn open_node(&mut self, id: &str) {
        let Some(session) = self.session_of(id) else {
            // Automation boxes have no terminal; selecting shows their details.
            self.select_node(id, false);
            return;
        };
        self.select_node(id, false);
        if self.owned_alive(&session) {
            self.show_terminal(&session);
            self.set_focus(Focus::Terminal);
            return;
        }
        // A passive session opens its panel; its chat starts only on request.
        if self.doc.node(&session).is_some_and(|n| n.passive.is_some()) {
            self.right_detail = Some(crate::side::RightDetail::Passive(session));
            self.needs_draw = true;
            return;
        }
        if self.preparing.contains(&session) {
            self.notify("This session's worktree is still being created.", false);
            return;
        }
        if self.sessions.get(&session).is_some_and(|s| s.live) {
            self.notify("This session is open in another terminal (CLI or VS Code). Copilot Canvas never starts a second copy.", true);
            return;
        }
        self.launch(&session);
    }

    pub fn launch(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        let cwd = node
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| self.sessions.get(id).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()))
            .unwrap_or_else(|| self.default_cwd.clone());
        if !Path::new(&cwd).is_dir() {
            self.node_errors.insert(id.to_owned(), "folder missing".into());
            self.notify(format!("Cannot open: folder not found: {cwd}"), true);
            return;
        }
        let kind = match &node.command {
            Some(command) if !command.is_empty() => LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() },
            _ => {
                let (args, env) = match self.launch_extras(id, node.fresh) {
                    Ok(extras) => extras,
                    Err(error) => {
                        self.notify(format!("Cannot prepare the group folders: {error}"), true);
                        (Vec::new(), Vec::new())
                    }
                };
                LaunchKind::CopilotWith {
                    new_name: node.fresh.then(|| node.name.clone()),
                    prompt: self.pending_prompts.remove(id),
                    resume: !node.fresh,
                    args,
                    env,
                }
            }
        };
        let (cols, rows) = self.pane_size();
        self.node_errors.remove(id);
        self.send(ClientMsg::Launch { id: id.to_owned(), kind, cwd: cwd.clone(), cols, rows });
        self.launching.insert(id.to_owned(), Instant::now());
        if let Some(n) = self.doc.node_mut(id) {
            n.fresh = false;
            n.cwd = Some(cwd.clone());
        }
        self.doc.note_folder(&cwd, now_ms());
        self.show_terminal(id);
        self.set_focus(Focus::Terminal);
    }

    pub fn open_group(&mut self, id: &str) {
        self.select_group(id);
        self.reveal_group(id);
        let members = self.doc.members(id);
        let running: Vec<String> = members.iter().filter(|m| self.owned_alive(m)).cloned().collect();
        if running.is_empty() {
            self.notify("No sessions in this group are running here. Click a session to open it.", false);
            return;
        }
        if running.len() > 1 {
            self.doc.ui.tiled = true;
        }
        self.tile_slots.clear();
        for member in running.iter().take(4).rev() {
            self.show_terminal(member);
        }
        self.focused_term = running.first().cloned();
    }

    // ---------- service messages ----------

    pub fn on_service(&mut self, message: ServiceMsg) {
        self.needs_draw = true;
        match message {
            ServiceMsg::Frame(frame) => {
                let id = frame.id.clone();
                self.views.entry(id).or_default().apply(frame);
            }
            ServiceMsg::Terminals(infos) => {
                self.terms_rev += 1;
                for info in infos {
                    // Bells and notifications from a session ring the outer terminal
                    // (at most once a second).
                    if self.terms.get(&info.id).is_some_and(|old| info.bells > old.bells)
                        && self.last_bell.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                    {
                        self.last_bell = Some(Instant::now());
                        self.raw_out.push(0x07);
                    }
                    if info.alive {
                        self.launching.remove(&info.id);
                    } else if self.launching.remove(&info.id).is_some() && info.exit_code != Some(0) {
                        self.notify(format!("Terminal exited immediately (code {:?}).", info.exit_code), true);
                    }
                    self.terms.insert(info.id.clone(), info);
                }
                // A reattaching client restores its tiles from the saved order.
                if self.tile_slots.is_empty() {
                    let restore: Vec<String> =
                        self.doc.tiles.iter().filter(|id| self.owned_alive(id) && self.doc.node(id).is_some()).take(4).cloned().collect();
                    for id in restore.iter().rev() {
                        if !self.tile_slots.contains(id) {
                            self.tile_slots.push(id.clone());
                        }
                    }
                    if self.focused_term.is_none() {
                        self.focused_term = restore.first().cloned();
                    }
                }
            }
            ServiceMsg::TerminalGone(id) => {
                self.terms.remove(&id);
                self.views.remove(&id);
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
                self.sessions_loaded = true;
                self.sessions_rev += 1;
                self.sync_agents();
                if !self.auto_imported && !self.demo && self.doc.nodes.is_empty() && !self.sessions.is_empty() {
                    self.auto_imported = true;
                    let mut recent: Vec<&SessionSummary> = self.sessions.values().collect();
                    recent.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
                    let ids: Vec<String> = recent.into_iter().take(8).map(|s| s.id.clone()).collect();
                    self.import_ids(ids, crate::dialogs::Grouping::None);
                    self.notify("Added your 8 most recent sessions. Press I to import more or V for VS Code agents.", false);
                }
            }
            ServiceMsg::Clipboard(text) => self.copy_to_clipboard(&text),
            ServiceMsg::Error { id, message } => {
                if let Some(id) = id {
                    self.launching.remove(&id);
                    self.node_errors.insert(id, "launch failed".into());
                }
                self.notify(message, true);
            }
            ServiceMsg::Stats(stats) => self.stats = Some(stats),
            ServiceMsg::Subsession { parent, child, name, cwd } => self.link_subsession(&parent, &child, &name, &cwd),
            ServiceMsg::Bus { json } => self.on_bus(&json),
            ServiceMsg::BusEvent { json } => self.on_bus_event(&json),
            ServiceMsg::Ready { .. } | ServiceMsg::Challenge { .. } => {}
        }
    }

    /// Copilot asked to copy (OSC 52): use the Windows clipboard directly; the
    /// console belongs to the output thread.
    pub fn copy_to_clipboard(&mut self, text: &str) {
        match crate::clipboard::set_text(text) {
            Ok(()) => self.notify("Copied to the clipboard.", false),
            Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
        }
    }

    /// Adds boxes for subagents discovered in history, under their parent.
    pub fn sync_agents(&mut self) {
        let mut added = false;
        // Hash lookups: this runs on every session update, for every box.
        let existing: std::collections::HashSet<String> = self.doc.nodes.iter().map(|n| n.id.clone()).collect();
        let missing: Vec<(Node, Vec<(String, String)>)> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Session)
            .filter_map(|parent| {
                let summary = self.sessions.get(&parent.id)?;
                let agents: Vec<(String, String)> = summary
                    .agents
                    .iter()
                    .filter(|a| !existing.contains(&format!("{}:{}", parent.id, a.key)))
                    .map(|a| (a.key.clone(), a.name.clone()))
                    .collect();
                (!agents.is_empty()).then(|| (parent.clone(), agents))
            })
            .collect();
        let mut index = (!missing.is_empty()).then(|| crate::doc::SpatialIndex::new(&self.doc));
        for (parent, agents) in missing {
            for (key, name) in agents {
                let id = format!("{}:{key}", parent.id);
                let current_parent = parent.clone();
                let index = index.as_mut().expect("built when anything is missing");
                let (x, y) = self.doc.agent_slot_in(&current_parent, index);
                let node = Node {
                    id: id.clone(),
                    kind: NodeKind::Agent,
                    name,
                    x,
                    y,
                    cwd: None,
                    parent: Some(parent.id.clone()),
                    agent_key: Some(key),
                    source: None,
                    command: None,
                    fresh: false,
                    automation: None,
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
                };
                let rect = node.rect();
                index.insert(rect);
                // Keep new subagents inside their parent's group, unless growing it
                // would cover another group (membership is geometric).
                let group_index = self
                    .doc
                    .group_of(&current_parent)
                    .and_then(|g| self.doc.groups.iter().position(|candidate| candidate.id == g.id));
                self.doc.nodes.push(node);
                if let Some(gi) = group_index {
                    let current = self.doc.groups[gi].rect();
                    let grown = current.union(&crate::doc::Rect { x: rect.x - 2, y: rect.y - 1, w: rect.w + 4, h: rect.h + 2 });
                    let covers = self.doc.groups.iter().enumerate().any(|(i, g)| i != gi && !current.contains_rect(&g.rect()) && g.rect().intersects(&grown));
                    if !covers {
                        let group = &mut self.doc.groups[gi];
                        group.x = grown.x;
                        group.y = grown.y;
                        group.w = grown.w;
                        group.h = grown.h;
                    }
                }
                added = true;
            }
        }
        if added {
            self.mark_dirty();
        }
    }

    pub fn import_ids(&mut self, ids: Vec<String>, grouping: crate::dialogs::Grouping) {
        use crate::dialogs::Grouping;
        let mut buckets: Vec<(Option<String>, Vec<Seed>)> = Vec::new();
        for id in ids {
            if self.doc.node(&id).is_some() {
                continue;
            }
            let Some(summary) = self.sessions.get(&id) else { continue };
            let key = match grouping {
                Grouping::None => None,
                Grouping::Folder => {
                    let root = if summary.git_root.is_empty() { &summary.cwd } else { &summary.git_root };
                    Some(root.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("Other").to_owned())
                }
                Grouping::Client => Some(match summary.client.as_str() {
                    "vscode-agent-host" => "VS Code".to_owned(),
                    "github/cli" | "" => "Copilot CLI".to_owned(),
                    other => other.to_owned(),
                }),
            };
            let seed = Seed { id: id.clone(), name: summary.name.clone(), cwd: summary.cwd.clone(), source: None };
            match buckets.iter_mut().find(|(k, _)| *k == key) {
                Some((_, seeds)) => seeds.push(seed),
                None => buckets.push((key, vec![seed])),
            }
        }
        self.place(buckets);
    }

    /// Lays out imported sessions to the right of existing content.
    pub fn place(&mut self, buckets: Vec<(Option<String>, Vec<Seed>)>) -> (usize, usize) {
        let (base_x, base_y) = match self.doc.bounds() {
            Some(b) => (b.x + b.w + 8, b.y),
            None => (self.doc.view.x + 3, self.doc.view.y + 2),
        };
        let (mut cursor_x, mut row_y, mut row_h) = (base_x, base_y, 0);
        let (mut nodes, mut groups) = (0, 0);
        let mut first = None;
        for (name, seeds) in buckets {
            if seeds.is_empty() {
                continue;
            }
            let count = seeds.len() as i32;
            let cols = ((count as f64).sqrt().ceil() as i32).clamp(1, 8);
            let rows = (count + cols - 1) / cols;
            let pad = if name.is_some() { 3 } else { 0 };
            let mut ids = Vec::new();
            let seeds: Vec<Seed> = seeds.into_iter().filter(|s| self.doc.node(&s.id).is_none()).collect();
            for (i, seed) in seeds.into_iter().enumerate() {
                let i = i as i32;
                let node = Node {
                    id: seed.id.clone(),
                    kind: NodeKind::Session,
                    name: seed.name,
                    x: cursor_x + pad + (i % cols) * GRID_X,
                    y: row_y + pad + (i / cols) * crate::doc::GRID_Y,
                    cwd: (!seed.cwd.is_empty()).then_some(seed.cwd),
                    parent: None,
                    agent_key: None,
                    source: seed.source,
                    command: None,
                    fresh: false,
                    automation: None,
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
                };
                first.get_or_insert_with(|| node.id.clone());
                ids.push(node.id.clone());
                self.doc.nodes.push(node);
                nodes += 1;
            }
            let width = cols * GRID_X + pad * 2;
            let height = rows * crate::doc::GRID_Y + pad * 2;
            if let Some(name) = name {
                self.doc.group_around(&ids, &name);
                groups += 1;
            }
            row_h = row_h.max(height);
            cursor_x += width + 6;
            if cursor_x - base_x > 4 * GRID_X * 3 {
                cursor_x = base_x;
                row_y += row_h + 4;
                row_h = 0;
            }
        }
        self.sync_agents();
        if let Some(first) = first {
            self.select_node(&first, false);
            self.fit_all();
        }
        self.mark_dirty();
        (nodes, groups)
    }

    /// Stops canvas-started Copilot sessions that are idle, off screen and
    /// untouched for `doc.idle_stop_minutes` (they stay resumable).
    pub fn stop_idle_sessions(&mut self) {
        self.idle_check_at = Instant::now();
        let minutes = self.doc.idle_stop_minutes;
        if minutes == 0 || self.demo {
            return;
        }
        let limit = minutes as u64 * 60_000;
        let now = now_ms();
        let on_screen: std::collections::HashSet<&str> = self.layout.tiles.iter().map(|(id, _, _)| id.as_str()).collect();
        let mut idle = Vec::new();
        for (id, info) in &self.terms {
            if !info.alive || !info.copilot || on_screen.contains(id.as_str()) || self.focused_term.as_deref() == Some(id.as_str()) {
                continue;
            }
            let Some(node) = self.doc.node(id) else { continue };
            // Never stop a session waiting for its next scheduled run: its schedule would end.
            if !matches!(self.node_status(node), Status::Idle | Status::Running) || self.node_loop(node).is_some() {
                continue;
            }
            let summary = self.sessions.get(id);
            let typed = self.last_input_at.get(id).map(|t| now.saturating_sub(t.elapsed().as_millis() as u64)).unwrap_or(0);
            let last = [info.started_ms, info.last_output_ms, typed, summary.map(|s| s.last_event_ms.max(s.updated_ms)).unwrap_or(0)].into_iter().max().unwrap_or(0);
            if now.saturating_sub(last) >= limit {
                idle.push(id.clone());
            }
        }
        for id in &idle {
            self.send(ClientMsg::Stop { id: id.clone(), force: false });
        }
        if !idle.is_empty() {
            self.notify(format!("Stopped {} session(s) idle for {minutes}+ min to free memory; click one to resume it.", idle.len()), false);
        }
    }

    pub fn after_draw(&mut self) {
        let visible: Vec<String> = self.layout.tiles.iter().map(|(id, _, _)| id.clone()).collect();
        if visible != self.last_subscribed {
            self.send(ClientMsg::Subscribe { ids: visible.clone() });
            self.last_subscribed = visible;
        }
        if !matches!(self.drag, Some(Drag::SidebarDivider | Drag::TermDivider)) {
            let tiles = self.layout.tiles.clone();
            for (id, _, inner) in tiles {
                let size = (inner.width.max(20), inner.height.max(5));
                if self.owned_alive(&id) && self.last_sizes.get(&id) != Some(&size) {
                    self.last_sizes.insert(id.clone(), size);
                    self.send(ClientMsg::Resize { id, cols: size.0, rows: size.1 });
                }
            }
        }
        let mut tracked: Vec<String> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Session && n.command.is_none())
            .map(|n| n.id.clone())
            .collect();
        tracked.sort();
        if tracked != self.last_tracked {
            self.send(ClientMsg::Track { ids: tracked.clone() });
            self.last_tracked = tracked;
        }
    }

    pub fn on_tick(&mut self) {
        // Spinner frame: one step per 80 ms regardless of the housekeeping rate.
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let frame = (START.get_or_init(Instant::now).elapsed().as_millis() / 80) as usize;
        let spun = frame != self.tick;
        self.tick = frame;
        if self.dirty_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(400)) {
            self.save_now();
        }
        if self.notice.as_ref().is_some_and(|n| Instant::now() >= n.until) {
            self.notice = None;
            self.needs_draw = true;
        }
        if self.flash.as_ref().is_some_and(|(_, t)| t.elapsed() >= Duration::from_millis(1500)) {
            self.flash = None;
            self.needs_draw = true;
        }
        self.launching.retain(|_, t| t.elapsed() < Duration::from_secs(20));
        if self.stats_at.elapsed() >= Duration::from_secs(3) {
            self.stats_at = Instant::now();
            self.send(ClientMsg::Stats);
        }
        if self.pan_quiet_until.is_some_and(|t| Instant::now() >= t) {
            self.pan_quiet_until = None;
            self.needs_draw = true;
        }
        self.baseline_reviewed();
        if self.routing_check_at.elapsed() >= Duration::from_secs(1) {
            self.sync_routing();
        }
        if self.tick_pulses() {
            self.needs_draw = true;
        }
        if self.progress_check_at.elapsed() >= Duration::from_millis(500) {
            self.mirror_progress();
        }
        if self.focus == Focus::Terminal
            && let Some(id) = self.focused_term.clone()
        {
            self.mark_reviewed(&id);
        }
        if self.idle_check_at.elapsed() >= Duration::from_secs(30) {
            self.stop_idle_sessions();
        }
        self.poll_git_debounce();
        self.tick_side();
        // Failed automations pulse until their next good run.
        let animated = self.doc.nodes.iter().any(|n| {
            let status = self.node_status(n);
            status.animated() || self.node_loop(n).is_some() || n.kind == NodeKind::Automation && status == Status::Failed
        });
        if (animated && spun) || self.flash.is_some() {
            self.needs_draw = true;
        }
    }
}

/// "15m", "2h", "1d", "30s".
pub fn short_interval(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{}s", s.max(1)),
        60..3600 => format!("{}m", s / 60),
        3600..86400 if s.is_multiple_of(3600) => format!("{}h", s / 3600),
        3600..86400 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// A schedule that repeats (not manual or one-off).
fn repeats(schedule: &str) -> bool {
    !schedule.is_empty() && schedule != "manual" && !schedule.starts_with("once")
}
/// Client-side frame statistics (`--perf`).
#[derive(Default)]
pub struct Perf {
    /// Completion times of the frames drawn in the last second.
    frames: std::collections::VecDeque<Instant>,
    draw_ms: Vec<f32>,
    latency_ms: Vec<f32>,
    bytes: Vec<f32>,
    compose_ms: Vec<f32>,
    input_ms: Vec<f32>,
    service_ms: Vec<f32>,
    flush_ms: Vec<f32>,
    /// Arrival of the oldest input not yet on screen.
    pub input_pending: Option<Instant>,
    /// Time per part of a frame (toolbar, sidebar, canvas, …).
    phases: Vec<(&'static str, Vec<f32>)>,
    /// Arrival of the oldest terminal update not yet on screen.
    pub pending: Option<Instant>,
}

impl Perf {
    pub fn drawn(&mut self, started: Instant, done: Instant, bytes: u64, compose: Duration) {
        self.frames.push_back(done);
        while self.frames.front().is_some_and(|t| done.duration_since(*t) > Duration::from_secs(1)) {
            self.frames.pop_front();
        }
        let keep = |v: &mut Vec<f32>, x: f32| {
            v.push(x);
            if v.len() > 240 {
                v.remove(0);
            }
        };
        keep(&mut self.draw_ms, done.duration_since(started).as_secs_f32() * 1000.0);
        keep(&mut self.bytes, bytes as f32 / 1024.0);
        keep(&mut self.compose_ms, compose.as_secs_f32() * 1000.0);
    }

    /// A frame reached the console (on the output thread).
    pub fn presented(&mut self, console: Duration, input_since: Option<Instant>, output_since: Option<Instant>) {
        let keep = |v: &mut Vec<f32>, x: f32| {
            v.push(x);
            if v.len() > 240 {
                v.remove(0);
            }
        };
        keep(&mut self.flush_ms, console.as_secs_f32() * 1000.0);
        if let Some(since) = output_since {
            keep(&mut self.latency_ms, since.elapsed().as_secs_f32() * 1000.0);
        }
        if let Some(since) = input_since {
            keep(&mut self.input_ms, since.elapsed().as_secs_f32() * 1000.0);
        }
    }

    pub fn phase(&mut self, name: &'static str, took: Duration) {
        let ms = took.as_secs_f32() * 1000.0;
        match self.phases.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => {
                v.push(ms);
                if v.len() > 240 {
                    v.remove(0);
                }
            }
            None => self.phases.push((name, vec![ms])),
        }
    }

    /// The costliest frame parts (p95), e.g. "canvas 0.9 · sidebar 0.4".
    pub fn top_phases(&self) -> String {
        let mut costs: Vec<(&str, f32)> = self.phases.iter().map(|(n, v)| (*n, Self::p95(v))).collect();
        costs.sort_by(|a, b| b.1.total_cmp(&a.1));
        costs.iter().take(4).map(|(n, ms)| format!("{n} {ms:.2}")).collect::<Vec<_>>().join(" · ")
    }

    /// Time the UI thread spent handling one batch of service messages.
    pub fn service_batch(&mut self, took: Duration) {
        self.service_ms.push(took.as_secs_f32() * 1000.0);
        if self.service_ms.len() > 240 {
            self.service_ms.remove(0);
        }
    }

    fn p95(values: &[f32]) -> f32 {
        if values.is_empty() {
            return 0.0;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)]
    }

    pub fn summary(&self) -> String {
        let fps = self.frames.iter().filter(|t| t.elapsed() <= Duration::from_secs(1)).count();
        format!(
            " {fps} fps · draw {:.1} ms (ui {:.1} [{}], console {:.1}) · {:.0} KB · input→screen {:.1} ms · output→screen {:.1} ms · svc batch {:.1}/{:.1} ms ",
            Self::p95(&self.draw_ms),
            Self::p95(&self.compose_ms),
            self.top_phases(),
            Self::p95(&self.flush_ms),
            Self::p95(&self.bytes),
            Self::p95(&self.input_ms),
            Self::p95(&self.latency_ms),
            Self::p95(&self.service_ms),
            self.service_ms.iter().copied().fold(0.0, f32::max)
        )
    }
}

pub struct Seed {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub source: Option<String>,
}
