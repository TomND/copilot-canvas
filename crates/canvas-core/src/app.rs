//! Client application state and core behavior. Input handling lives in
//! `events.rs`, dialogs/actions in `actions.rs`, drawing in `ui.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::layout::Rect;

use crate::agents::{AgentKind, agent_of, native_id};
use crate::conn::Conn;
use crate::dialogs::{Hit, Modal};
use crate::doc::{Doc, GRID_X, Node, NodeKind};
use crate::view::RenderMode;
use crate::git::GitInfo;
use crate::paths::now_ms;
use crate::prefs::PrefsFile;
use crate::protocol::{ClientMsg, Input, LaunchKind, ServiceMsg, ServiceStats, SessionSummary, TermInfo};
use crate::rpc::{Backend, BackendInfo};
use crate::status::{self, Status};
use crate::sync::{ClientPrefs, Opened, Replica};

/// Local document changes are sent to the service this soon.
const DOC_FLUSH: Duration = Duration::from_millis(100);
/// View and panel changes are written to the client's preferences this soon.
const PREFS_FLUSH: Duration = Duration::from_millis(400);

pub enum AppEvent {
    Git { req: u64, folder: String, result: Result<Option<GitInfo>, String> },
    Worktree { node: String, result: Result<(), String> },
    Forked { node: String, result: Result<(), String> },
    /// An agent's configuration (plugins, MCP servers, skills, agents, settings).
    Config(AgentKind, Box<crate::config::Snapshot>),
    Automations(crate::automation::AutoLoad),
    /// Loading the automations failed (the boxes stay as they are).
    AutomationsFailed(String),
    CliDone { label: String, result: Result<String, String> },
    VsSync(Vec<crate::vscode::SyncCandidate>, Vec<String>),
    MigratePlan(Vec<crate::migrate::Item>),
    MigrateDone(Result<Vec<String>, String>),
    /// The terminal service was restarted (new build); `mouse`: Copilot's mouse
    /// setting was switched back on.
    ServiceRestarted { result: Result<Conn, String>, mouse: bool },
    /// A dropped connection was re-established (or reconnecting gave up).
    Reconnected(Result<Conn, String>),
    /// Log tails for the automation info page: (path, text).
    Tails(Vec<(String, String)>),
    /// A background step has something to say.
    Notice { text: String, error: bool },
    /// A background step's continuation on the UI thread.
    Run(Box<dyn FnOnce(&mut App) + Send>),
    /// Dropped files were uploaded (remote mode): paste `paths` into `term`.
    Uploaded { term: String, paths: Vec<String>, errors: Vec<String> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Sidebar,
    Canvas,
    Terminal,
}

/// What Ctrl+Z restores.
#[derive(Clone, Debug)]
pub enum Undo {
    /// The canvas before a bulk change (a VS Code sync): label, document.
    Doc(String, Box<Doc>),
    /// Boxes removed together, and the subsession links (child, parent)
    /// their removal cut.
    Removed { nodes: Vec<Node>, links: Vec<(String, String)> },
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
    /// Panning; `group`: pressed on a group's background or border (a click
    /// without moving selects that group).
    Pan { last: (u16, u16), moved: bool, group: Option<String> },
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
    /// Gpu mode: the open HTML report's content area and its safe page
    /// (the GUI shows it in its HTML view there): (cells, report id, page).
    pub html_pane: Option<(Rect, String, Arc<String>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideKind {
    Group(String),
    Session(String),
    Agent(String),
    Section,
    /// A group's folders (its menu opens them).
    Folder(String),
    /// The RECENT section's header (a click folds it).
    RecentHeader,
    /// A recently used session in the RECENT section.
    Recent(String),
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
    /// A group's empty interior or its border (the innermost group there).
    GroupBody(String),
    Empty,
}

pub const ZOOMS: [(i32, i32); 3] = [(1, 1), (1, 2), (1, 4)];

/// Gpu mode: the continuous canvas scale's range (cells per world unit).
pub const MIN_SCALE: f32 = 1.0 / 16.0;
pub const MAX_SCALE: f32 = 2.0;
/// Gpu mode: how far outside a group's outline a click still hits its
/// border (cells across, rows down: about 4 px at 100 % DPI).
const BORDER_SLACK: (f32, f32) = (0.45, 0.25);
/// Gpu mode: denominator of the rational scale `scale()` reports.
const SCALE_DEN: i32 = 1024;

pub struct App {
    /// How the front end draws: character cells (TUI) or the GPU canvas.
    pub render_mode: crate::view::RenderMode,
    /// Gpu mode: canvas scale in cells per world unit (client-local; the TUI
    /// uses the discrete `ZOOMS`).
    pub view_scale: f32,
    /// Gpu mode: the fractional part of the view origin (world units, 0..1),
    /// so panning and zooming are pixel-smooth while the document keeps
    /// integer coordinates.
    pub view_frac: (f32, f32),
    /// Precise pointer position of the last mouse event (cell units).
    pub pointer: (f32, f32),
    /// Gpu mode: the pointer where the current drag started (cell units).
    pub drag_origin: (f32, f32),
    /// Clipboard, links, bells and taskbar of the machine the front end runs on.
    pub platform: std::sync::Arc<dyn crate::platform::Platform>,
    pub demo: bool,
    pub perf: Option<Perf>,
    pub term_select: Option<TermSelection>,
    /// What Ctrl+Z restores (the last VS Code sync or the last removal).
    pub undo: Option<Undo>,
    /// While wheel/trackpad panning, the dot grid is hidden until this time.
    pub pan_quiet_until: Option<Instant>,
    /// Ctrl+L: repaint every cell on the next frame.
    pub repaint_requested: bool,
    /// When you last typed into each terminal (for idle auto-stop).
    pub last_input_at: HashMap<String, Instant>,
    pub idle_check_at: Instant,
    pub default_cwd: String,
    /// The working copy of the canvas document (the service owns it; see `replica`).
    pub doc: Doc,
    /// The service's document and this client's edits in flight.
    pub replica: Replica,
    /// How the document was opened (`DocOpen` path and demo), asked again after reconnecting.
    pub doc_request: (Option<String>, bool),
    /// Waiting for the service to send the whole document again.
    pub doc_resync: bool,
    /// When local document changes started waiting to be sent.
    pub dirty_at: Option<Instant>,
    /// When view or panel changes started waiting to be written.
    pub prefs_dirty_at: Option<Instant>,
    /// This client's own view of the canvas.
    pub prefs: Option<PrefsFile>,
    pub save_error: Option<String>,
    /// Paths, settings and agents of the machine the agents run on.
    pub backend: Arc<dyn Backend>,
    pub info: BackendInfo,
    pub conn: Conn,
    pub connected: bool,
    /// Where the service is (reconnects go there); `None`: never reconnect.
    pub target: Option<crate::conn::ConnTarget>,
    pub conn_options: crate::conn::ConnOptions,
    /// A reconnect is in progress.
    pub reconnecting: bool,
    /// Connection banner for front ends ("Offline — reconnecting…").
    pub banner: Option<String>,
    /// The first terminal list after a reconnect replaces the known terminals.
    pub resync_terms: bool,
    /// Focused tile last sent with `Subscribe8`.
    pub last_focused_sub: Option<String>,
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
    /// The last sidebar row clicked (a second click on it soon after starts it).
    pub last_side_click: Option<(Instant, usize)>,
    pub app_tx: Sender<AppEvent>,
    pub quit: bool,
    pub tick: usize,
    /// Test hooks: animations (spinners, loop dots, pulsing borders) hold
    /// this frame, so screenshots are reproducible.
    pub frozen_tick: Option<usize>,
    pub git_req: u64,
    pub last_subscribed: Vec<String>,
    pub last_sizes: HashMap<String, (u16, u16)>,
    pub last_tracked: Vec<String>,
    pub stats: Option<ServiceStats>,
    pub stats_at: Instant,
    pub build_mismatch: bool,
    pub needs_draw: bool,
    pub auto_imported: bool,
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
    /// Sessions that needed you at the last mirror (`None` before the first).
    pub waiting_sessions: Option<HashSet<String>>,
    /// When each session last sent a desktop notification (rate limit).
    pub notified_at: HashMap<String, Instant>,
    /// The user asked for another window (the front end opens it).
    pub new_window_requested: bool,
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
    /// Configuration of `config_agent` (what the configuration views show).
    pub config: Option<crate::config::Snapshot>,
    /// The agent whose configuration the views show.
    pub config_agent: AgentKind,
    /// Every agent configuration loaded so far (MCP choices for new sessions).
    pub agent_configs: HashMap<AgentKind, crate::config::Snapshot>,
    pub configs_loading: HashSet<AgentKind>,
    /// The machine's agent for new sessions (settings.json, else the first installed).
    pub detected_agent: AgentKind,
    /// Agents whose CLI was found at start.
    pub installed_agents: Vec<AgentKind>,
    /// New subsessions that fork their parent natively: node → parent's agent session.
    pub pending_forks: HashMap<String, String>,
    /// New subsession boxes that resume a subagent's own conversation and
    /// have not run yet (a refused launch removes them again).
    pub resumed_children: HashSet<String>,
    pub automations: Vec<crate::automation::Automation>,
    pub automations_loading: bool,
    pub automations_at: Instant,
    pub automations_error: Option<String>,
    /// The manifest folder loaded (task boxes are only removed when it did).
    pub automations_factory_ok: bool,
    /// The automation manifest folder in use.
    pub automations_folder: String,
    /// Log tails on the automation info page: path → text.
    pub tails: HashMap<String, String>,
    pub tails_at: Instant,
    pub tails_loading: bool,
    pub right_detail: Option<crate::side::RightDetail>,
    pub cli_busy: Option<String>,
    pub vs_syncing: bool,
    pub auto_synced: bool,
    /// Group whose session defaults the Settings view edits (None = canvas defaults).
    pub settings_group: Option<String>,
    /// Dust, stacks, hidden subsessions and effects (`visual.rs`).
    pub visual: crate::visual::VisualState,
    /// Recently used sessions: the walk and the RECENT rows (`recent.rs`).
    pub recent: crate::recent::RecentState,
    /// The canvas's skill library (canvas-wide agents), or why it could not be read.
    pub canvas_skills: Option<Result<Vec<canvas_model::canvas_wide::SkillInfo>, String>>,
    /// The Reports inbox and its viewer (`reports.rs`).
    pub reports: crate::reports::ReportsState,
}

impl App {
    /// A client on an opened document. `prefs`: the file with this client's
    /// view of it (seeded from the document's own view fields the first time).
    pub fn new(
        conn: Conn,
        backend: Arc<dyn Backend>,
        opened: Opened,
        demo: bool,
        default_cwd: String,
        app_tx: Sender<AppEvent>,
        prefs: Option<std::path::PathBuf>,
    ) -> Self {
        // Remote clients never restart or upgrade the service.
        let build_mismatch = !conn.remote && conn.service_build != crate::paths::build_id();
        let replica = Replica::new(&opened);
        let mut doc = opened.doc;
        let mut prefs = prefs.map(PrefsFile::new);
        if let Some(file) = prefs.as_mut() {
            file.load().unwrap_or_else(|| ClientPrefs::of(&doc)).apply(&mut doc);
        }
        let (info, info_error) = match backend.info() {
            Ok(info) => (info, None),
            Err(error) => (BackendInfo::default(), Some(format!("{error:#}"))),
        };
        let automations_folder = backend.automations_dir(doc.automations_dir.clone()).unwrap_or_default();
        let selection = doc.selected.clone().into_iter().collect();
        let detected_agent = info.default_agent;
        let installed_agents = info.installed.clone();
        let config_agent = doc.default_agent.as_deref().and_then(AgentKind::parse).unwrap_or(detected_agent);
        let mut app = Self {
            render_mode: crate::view::RenderMode::Cells,
            view_scale: 1.0,
            view_frac: (0.0, 0.0),
            pointer: (0.0, 0.0),
            drag_origin: (0.0, 0.0),
            platform: std::sync::Arc::new(crate::platform::TerminalPlatform),
            demo,
            default_cwd,
            doc,
            replica,
            doc_request: (None, demo),
            doc_resync: false,
            dirty_at: None,
            prefs_dirty_at: None,
            prefs,
            save_error: None,
            backend,
            info,
            conn,
            connected: true,
            target: None,
            conn_options: crate::conn::ConnOptions::default(),
            reconnecting: false,
            banner: None,
            resync_terms: false,
            last_focused_sub: None,
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
            last_side_click: None,
            app_tx,
            quit: false,
            tick: 0,
            frozen_tick: None,
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
            side_view: crate::side::SideView::Sessions,
            list_sel: 0,
            list_scroll: 0,
            config: None,
            config_agent,
            agent_configs: HashMap::new(),
            configs_loading: HashSet::new(),
            detected_agent,
            installed_agents,
            pending_forks: HashMap::new(),
            resumed_children: HashSet::new(),
            automations: Vec::new(),
            automations_loading: false,
            automations_at: Instant::now() - Duration::from_secs(60),
            automations_error: None,
            automations_factory_ok: false,
            automations_folder,
            tails: HashMap::new(),
            tails_at: Instant::now() - Duration::from_secs(60),
            tails_loading: false,
            right_detail: None,
            cli_busy: None,
            vs_syncing: false,
            auto_synced: false,
            settings_group: None,
            visual: Default::default(),
            recent: Default::default(),
            canvas_skills: None,
            reports: Default::default(),
            instance: None,
            new_service_rx: None,
            restarting: None,
            raw_out: Vec::new(),
            outer_progress: 0,
            progress_check_at: Instant::now(),
            last_bell: None,
            waiting_sessions: None,
            notified_at: HashMap::new(),
            new_window_requested: false,
            bus: crate::bus::Snapshot::default(),
            routing_sent: String::new(),
            routing_check_at: Instant::now(),
            pulses: Vec::new(),
        };
        if app.build_mismatch {
            app.notify("The terminal service is from an older build: click ⟳ Update service in the toolbar to get its fixes.", false);
        }
        if let Some(error) = info_error {
            app.notify(format!("The backend did not answer: {error}"), true);
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

    /// The document or this client's view of it changed: edits go to the
    /// service shortly, preferences to the client's own file.
    pub fn mark_dirty(&mut self) {
        self.dirty_at.get_or_insert_with(Instant::now);
        self.prefs_dirty_at.get_or_insert_with(Instant::now);
        self.needs_draw = true;
    }

    /// Sends pending edits and writes changed preferences now.
    pub fn save_now(&mut self) {
        self.flush_doc();
        self.save_prefs();
    }

    /// Sends local document changes to the service (as one edit).
    pub fn flush_doc(&mut self) {
        self.dirty_at = None;
        // Offline or resynchronizing: the changes stay and go out after the
        // service sent the document again (see `on_doc`).
        if !self.connected || self.doc_resync || self.restarting.is_some() || self.replica.key == "detached" {
            return;
        }
        let Some((base, ops)) = self.replica.take_edit(&self.doc) else { return };
        let message = match serde_json::to_string(&ops) {
            Ok(ops_json) => ClientMsg::DocEdit { key: self.replica.key.clone(), base, ops_json },
            Err(error) => return self.notify(format!("Cannot send a canvas change: {error}"), true),
        };
        if self.conn.tx.try_send(message).is_err() {
            // The edit is lost: start over from the service's document.
            self.request_doc();
        }
    }

    /// Writes this client's view of the canvas when it changed.
    pub fn save_prefs(&mut self) {
        self.prefs_dirty_at = None;
        self.doc.selected = self.selection.last().cloned();
        let prefs = ClientPrefs::of(&self.doc);
        if let Some(Err(error)) = self.prefs.as_mut().map(|file| file.save(&prefs)) {
            self.save_error = Some(format!("cannot save the view: {error:#}"));
        }
    }

    /// Asks the service for the whole document again (after reconnecting or
    /// when a patch went missing); local changes are kept on top of it.
    pub fn request_doc(&mut self) {
        if self.replica.key == "detached" {
            return;
        }
        self.doc_resync = true;
        let (path, demo) = self.doc_request.clone();
        let cwd = self.default_cwd.clone();
        self.send(ClientMsg::DocOpen { path, demo, cwd });
    }

    /// The whole document from the service.
    fn on_doc(&mut self, key: String, version: u64, json: &str) {
        self.doc_resync = false;
        match Opened::from_message(key, version, json) {
            Ok(opened) => {
                self.replica.resync(&mut self.doc, opened);
                self.after_remote_change();
                // Local changes kept on top go out now.
                self.mark_dirty();
            }
            Err(error) => self.notify(format!("The canvas could not be loaded again: {error:#}"), true),
        }
    }

    /// An edit the service applied (another window's, or this one's coming back).
    fn on_doc_patch(&mut self, key: &str, version: u64, origin: u64, ops_json: &str) {
        if key != self.replica.key || self.doc_resync {
            return;
        }
        let Ok(ops) = serde_json::from_str::<Vec<crate::sync::Op>>(ops_json) else { return self.request_doc() };
        let patched = self.replica.apply_patch(&mut self.doc, version, origin, &ops);
        if patched.resync {
            return self.request_doc();
        }
        if patched.changed {
            self.after_remote_change();
        }
        if let Some(name) = patched.overwritten.first() {
            let more = match patched.overwritten.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            self.notify(format!("{name}{more} was changed in another window."), false);
        }
    }

    /// Another window changed the document: forget what is gone.
    fn after_remote_change(&mut self) {
        self.selection.retain(|id| self.doc.node(id).is_some());
        self.selected_group = self.selected_group.take().filter(|g| self.doc.group(g).is_some());
        if let Some(crate::side::RightDetail::Session(id)) = &self.right_detail
            && self.doc.node(id).is_none()
        {
            self.right_detail = None;
        }
        self.needs_draw = true;
    }

    /// Writes a backup of `doc` next to the canvas file (in the background).
    pub fn backup_doc(&self, doc: Doc, label: &'static str, report: bool) {
        if self.replica.path.is_empty() {
            return;
        }
        let (backend, tx, path) = (self.backend.clone(), self.app_tx.clone(), self.replica.path.clone());
        std::thread::spawn(move || {
            if let Err(error) = backend.doc_backup(&path, label, doc)
                && report
            {
                let _ = tx.send(AppEvent::Notice { text: format!("Could not back up the canvas: {error:#}"), error: true });
            }
        });
    }

    /// Runs a backend call off the UI thread; `done` turns its result into an event.
    pub fn in_background<T: Send + 'static>(&self, call: impl FnOnce(&dyn Backend) -> T + Send + 'static, done: impl FnOnce(T) -> AppEvent + Send + 'static) {
        let (backend, tx) = (self.backend.clone(), self.app_tx.clone());
        std::thread::spawn(move || {
            let result = call(&*backend);
            let _ = tx.send(done(result));
        });
    }

    pub fn send(&mut self, message: ClientMsg) {
        let input = is_terminal_input(&message);
        if !self.connected {
            // Nothing is queued while offline: typing into a screen that changed
            // in the meantime is worse than typing again.
            if input && !matches!(message, ClientMsg::Input { input: Input::Focus(_), .. }) {
                self.notify("Offline: input not sent.", true);
            }
            return;
        }
        if let ClientMsg::Input { id, .. } | ClientMsg::Wheel { id, .. } = &message {
            self.last_input_at.insert(id.clone(), Instant::now());
        }
        // Typing into a session counts as using it (the RECENT list).
        let typed = match &message {
            ClientMsg::Input { id, input } if !matches!(input, Input::Focus(_)) => Some(id.clone()),
            _ => None,
        };
        if let Some(id) = typed {
            self.note_access(&id);
        }
        match self.conn.tx.try_send(message) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                if input {
                    self.notify("Offline: input not sent.", true);
                }
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                if self.restarting.is_none() {
                    self.connection_lost();
                }
            }
        }
    }

    /// The connection to the service dropped: input is blocked, a banner shows
    /// and a background thread reconnects with backoff. Front ends call this
    /// when the service channel closes.
    pub fn connection_lost(&mut self) {
        self.connected = false;
        self.needs_draw = true;
        if self.reconnecting {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.notify("The terminal service disconnected. Restart Copilot Canvas to reconnect; your canvas is saved.", true);
            return;
        };
        self.reconnecting = true;
        self.banner = Some("Offline — reconnecting…".into());
        // A local service that is gone for good is started again.
        let options = crate::conn::ConnOptions { start: true, ..self.conn_options.clone() };
        let session = self.conn.session.clone();
        let tx = self.app_tx.clone();
        let spawned = std::thread::Builder::new().name("reconnect".into()).spawn(move || {
            // A refusal keeps the client retrying; the banner says why.
            let mut shown = None;
            let result = crate::conn::reconnect(&target, &options, &session, |_, error| {
                let reason = crate::conn::retry_reason(error);
                if reason != shown {
                    shown = reason;
                    let _ = tx.send(AppEvent::Run(Box::new(move |app: &mut App| {
                        if app.reconnecting {
                            app.banner = Some(reason.map_or_else(|| "Offline — reconnecting…".to_owned(), |why| format!("Offline — reconnecting… ({why})")));
                            app.needs_draw = true;
                        }
                    })));
                }
                true
            })
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Reconnected(result));
        });
        if spawned.is_err() {
            self.reconnecting = false;
            self.notify("Cannot reconnect to the terminal service; restart Copilot Canvas.", true);
        }
    }

    /// Switches to a re-established connection. The service starts this client
    /// from scratch, so subscriptions, sizes, tracking and routing are sent
    /// again (full frames follow). Keystrokes the service never confirmed are
    /// reported, never resent.
    pub fn on_reconnected(&mut self, result: Result<Conn, String>) {
        self.reconnecting = false;
        self.needs_draw = true;
        let conn = match result {
            Ok(conn) => conn,
            Err(error) => {
                self.banner = Some("Offline".into());
                self.notify(format!("Could not reconnect to the terminal service: {error}"), true);
                return;
            }
        };
        if conn.remote && conn.protocol_mismatch {
            self.banner = Some("Offline: incompatible service".into());
            self.notify(
                format!(
                    "The service speaks protocol {} and this client {}; update this client (remote clients never restart the service).",
                    conn.protocol,
                    crate::protocol::PROTOCOL
                ),
                true,
            );
            return;
        }
        let lost = conn.session.settle();
        self.build_mismatch = !conn.remote && conn.service_build != crate::paths::build_id();
        self.new_service_rx = Some(conn.rx.clone());
        self.conn = conn;
        self.connected = true;
        self.banner = None;
        self.last_subscribed.clear();
        self.last_focused_sub = None;
        self.last_sizes.clear();
        self.last_tracked.clear();
        self.routing_sent.clear();
        self.resync_terms = true;
        // The new connection opens the document again (local changes are kept).
        self.request_doc();
        if lost > 0 {
            self.notify(format!("Reconnected. {lost} keystroke(s) were not delivered."), true);
        } else {
            self.notify("Reconnected to the terminal service.", false);
        }
    }

    // ---------- geometry ----------

    /// The canvas scale as a fraction: `n / d` cells per world unit.
    pub fn scale(&self) -> (i32, i32) {
        match self.render_mode {
            RenderMode::Cells => ZOOMS[(self.doc.view.zoom as usize).min(ZOOMS.len() - 1)],
            RenderMode::Gpu => (((self.view_scale * SCALE_DEN as f32).round() as i32).max(1), SCALE_DEN),
        }
    }

    /// The canvas scale in cells per world unit.
    pub fn scale_f(&self) -> f32 {
        match self.render_mode {
            RenderMode::Cells => {
                let (n, d) = self.scale();
                n as f32 / d as f32
            }
            RenderMode::Gpu => self.view_scale,
        }
    }

    /// The world point at the canvas's top-left corner (fractional in Gpu mode).
    pub fn origin_f(&self) -> (f32, f32) {
        (self.doc.view.x as f32 + self.view_frac.0, self.doc.view.y as f32 + self.view_frac.1)
    }

    /// World → screen in fractional cells.
    pub fn to_screen_f(&self, wx: f32, wy: f32) -> (f32, f32) {
        let (s, (ox, oy), c) = (self.scale_f(), self.origin_f(), self.layout.canvas);
        (c.x as f32 + (wx - ox) * s, c.y as f32 + (wy - oy) * s)
    }

    /// Screen (fractional cells) → world.
    pub fn to_world_f(&self, sx: f32, sy: f32) -> (f32, f32) {
        let (s, (ox, oy), c) = (self.scale_f(), self.origin_f(), self.layout.canvas);
        (ox + (sx - c.x as f32) / s, oy + (sy - c.y as f32) / s)
    }

    pub fn to_screen(&self, wx: i32, wy: i32) -> (i32, i32) {
        if self.render_mode == RenderMode::Gpu {
            // The cell whose centre the point's cell starts at.
            let (x, y) = self.to_screen_f(wx as f32, wy as f32);
            return (x.round() as i32, y.round() as i32);
        }
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        (c.x as i32 + ((wx - self.doc.view.x) * n).div_euclid(d), c.y as i32 + ((wy - self.doc.view.y) * n).div_euclid(d))
    }

    pub fn to_world(&self, sx: i32, sy: i32) -> (i32, i32) {
        if self.render_mode == RenderMode::Gpu {
            let (x, y) = self.to_world_f(sx as f32, sy as f32);
            return (x.floor() as i32, y.floor() as i32);
        }
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        (self.doc.view.x + (sx - c.x as i32) * d / n, self.doc.view.y + (sy - c.y as i32) * d / n)
    }

    pub fn scaled(&self, w: i32, h: i32, min_w: i32, min_h: i32) -> (i32, i32) {
        if self.render_mode == RenderMode::Gpu {
            let s = self.view_scale;
            return (((w as f32 * s).round() as i32).max(min_w), ((h as f32 * s).round() as i32).max(min_h));
        }
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
        if self.render_mode == RenderMode::Gpu {
            return self.canvas_hit_f(self.point_in(sx, sy));
        }
        let (sx, sy) = (sx as i32, sy as i32);
        let inside = |x: i32, y: i32, w: i32, h: i32| sx >= x && sx < x + w && sy >= y && sy < y + h;
        let concealed = self.concealed_now();
        let mut order: Vec<&Node> = self.doc.nodes.iter().filter(|n| !concealed.contains_key(&n.id)).collect();
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
            // The interior and the border cells (the outline is drawn inside the rect).
            if inside(x, y, w, h) {
                return CanvasHit::GroupBody(group.id.clone());
            }
        }
        CanvasHit::Empty
    }

    /// The precise pointer when it lies in cell `(sx, sy)`, else the cell's centre.
    pub fn point_in(&self, sx: u16, sy: u16) -> (f32, f32) {
        let (px, py) = self.pointer;
        if px.floor() as i32 == sx as i32 && py.floor() as i32 == sy as i32 { (px, py) } else { (sx as f32 + 0.5, sy as f32 + 0.5) }
    }

    /// Pixel-precise hit test at a point in fractional cells (Gpu mode): the
    /// targets of `canvas_hit` at any scale. A group's title row and resize
    /// corner stay at least about a cell large on screen.
    pub fn canvas_hit_f(&self, (px, py): (f32, f32)) -> CanvasHit {
        let s = self.scale_f();
        let (wx, wy) = self.to_world_f(px, py);
        let inside = |r: crate::doc::Rect| wx >= r.x as f32 && wx < (r.x + r.w) as f32 && wy >= r.y as f32 && wy < (r.y + r.h) as f32;
        let concealed = self.concealed_now();
        let mut order: Vec<&Node> = self.doc.nodes.iter().filter(|n| !concealed.contains_key(&n.id)).collect();
        order.sort_by_key(|n| self.selection.contains(&n.id));
        if let Some(node) = order.into_iter().rev().find(|n| inside(n.rect())) {
            return CanvasHit::Node(node.id.clone());
        }
        let mut groups: Vec<&crate::doc::Group> = self.doc.groups.iter().collect();
        groups.sort_by_key(|g| std::cmp::Reverse((g.w as i64) * (g.h as i64)));
        let row = 1f32.max(0.8 / s);
        let corner = 2f32.max(1.6 / s);
        // The border is easy to hit: a few pixels outside the outline count too.
        let (edge_x, edge_y) = (BORDER_SLACK.0 / s, BORDER_SLACK.1 / s);
        for group in groups.into_iter().rev() {
            let (x0, y0, x1, y1) = (group.x as f32, group.y as f32, (group.x + group.w) as f32, (group.y + group.h) as f32);
            if wx >= x1 - corner && wx < x1 && wy >= y1 - row && wy < y1 {
                return CanvasHit::GroupResize(group.id.clone());
            }
            if wy >= y0 && wy < y0 + row && wx >= x0 && wx < x1 {
                return CanvasHit::GroupTitle(group.id.clone());
            }
            if wx >= x0 - edge_x && wx < x1 + edge_x && wy >= y0 - edge_y && wy < y1 + edge_y {
                return CanvasHit::GroupBody(group.id.clone());
            }
        }
        CanvasHit::Empty
    }

    /// Gpu mode: puts the canvas's top-left corner at world point `(x, y)`.
    fn set_origin(&mut self, x: f32, y: f32) {
        let (fx, fy) = (x.floor(), y.floor());
        self.doc.view.x = fx as i32;
        self.doc.view.y = fy as i32;
        self.view_frac = (x - fx, y - fy);
    }

    /// Gpu mode: pans by a pointer movement in fractional cells.
    pub fn pan_cells(&mut self, dx: f32, dy: f32) {
        let s = self.scale_f();
        let (ox, oy) = self.origin_f();
        self.set_origin(ox - dx / s, oy - dy / s);
        if dx != 0.0 || dy != 0.0 {
            self.mark_dirty();
        }
    }

    pub fn center_on(&mut self, wx: i32, wy: i32) {
        if self.render_mode == RenderMode::Gpu {
            return self.center_on_f(wx as f32, wy as f32);
        }
        let (n, d) = self.scale();
        let c = self.layout.canvas;
        let (cw, ch) = (c.width.max(20) as i32 * d / n, c.height.max(10) as i32 * d / n);
        self.doc.view.x = wx - cw / 2;
        self.doc.view.y = wy - ch / 2;
        self.mark_dirty();
    }

    /// Gpu mode: puts world point `(wx, wy)` at the canvas's centre.
    pub fn center_on_f(&mut self, wx: f32, wy: f32) {
        let c = self.layout.canvas;
        let s = self.view_scale;
        self.set_origin(wx - c.width.max(20) as f32 / s / 2.0, wy - c.height.max(10) as f32 / s / 2.0);
        self.mark_dirty();
    }

    /// Gpu mode: the world point at the canvas's centre.
    pub fn canvas_center_f(&self) -> (f32, f32) {
        let c = self.layout.canvas;
        self.to_world_f(c.x as f32 + c.width.max(20) as f32 / 2.0, c.y as f32 + c.height.max(10) as f32 / 2.0)
    }

    pub fn reveal_node(&mut self, id: &str) {
        let Some(node) = self.doc.node(id) else { return };
        // A stacked or hidden box: where it shows (its stack or visible ancestor).
        let (cx, cy) = self.shown_center(id).unwrap_or_else(|| node.rect().center());
        self.center_on(cx, cy);
        self.flash = Some((id.to_owned(), Instant::now()));
    }

    pub fn reveal_group(&mut self, id: &str) {
        let Some(group) = self.doc.group(id).cloned() else { return };
        let c = self.layout.canvas;
        if self.render_mode == RenderMode::Gpu {
            // Zoom out (never in) until the group fits.
            let fit = ((c.width as f32 - 2.0) / group.w.max(1) as f32).min((c.height as f32 - 1.0) / group.h.max(1) as f32);
            if fit < self.view_scale {
                self.set_scale(fit);
            }
        } else {
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
        }
        let (cx, cy) = group.rect().center();
        self.center_on(cx, cy);
        self.flash = Some((id.to_owned(), Instant::now()));
    }

    pub fn fit_all(&mut self) {
        let Some(bounds) = self.doc.bounds() else { return };
        let c = self.layout.canvas;
        if self.render_mode == RenderMode::Gpu {
            // Everything in view, never above 100 %.
            let fit = ((c.width as f32 - 2.0) / bounds.w.max(1) as f32).min((c.height as f32 - 1.0) / bounds.h.max(1) as f32);
            self.set_scale(fit.min(1.0));
        } else {
            self.doc.view.zoom = (ZOOMS.len() - 1) as u8;
            for (zoom, (n, d)) in ZOOMS.iter().enumerate() {
                if bounds.w * n / d <= c.width as i32 - 2 && bounds.h * n / d < c.height as i32 {
                    self.doc.view.zoom = zoom as u8;
                    break;
                }
            }
        }
        let (cx, cy) = bounds.center();
        self.center_on(cx, cy);
    }

    /// Gpu mode: sets the scale (clamped) and the nearest discrete zoom the
    /// document keeps for the TUI.
    fn set_scale(&mut self, scale: f32) {
        self.view_scale = scale.clamp(MIN_SCALE, MAX_SCALE);
        self.doc.view.zoom = if self.view_scale >= 0.75 {
            0
        } else if self.view_scale >= 0.375 {
            1
        } else {
            2
        };
    }

    /// Zooms one step around a cell: `delta < 0` zooms in. Gpu mode steps
    /// between powers of two (… 25 % · 50 % · 100 % · 200 %).
    pub fn zoom_at(&mut self, sx: i32, sy: i32, delta: i32) {
        if self.render_mode == RenderMode::Gpu {
            let level = self.view_scale.log2();
            let target = if delta < 0 { (level + 1e-3).floor() + 1.0 } else { (level - 1e-3).ceil() - 1.0 };
            return self.zoom_to(sx as f32, sy as f32, 2f32.powf(target));
        }
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

    /// Gpu mode: continuous zoom by `factor` around a point in fractional cells.
    pub fn zoom_by(&mut self, sx: f32, sy: f32, factor: f32) {
        self.zoom_to(sx, sy, self.view_scale * factor);
    }

    /// Gpu mode: sets the scale keeping the world point under `(sx, sy)` in place.
    fn zoom_to(&mut self, sx: f32, sy: f32, scale: f32) {
        if (scale.clamp(MIN_SCALE, MAX_SCALE) - self.view_scale).abs() < 1e-6 {
            return;
        }
        let before = self.to_world_f(sx, sy);
        self.set_scale(scale);
        let after = self.to_world_f(sx, sy);
        let (ox, oy) = self.origin_f();
        self.set_origin(ox + before.0 - after.0, oy + before.1 - after.1);
        self.mark_dirty();
    }

    /// Gpu mode: the visual offset (world units, below one) of the boxes and
    /// groups being dragged; the document moves in whole units.
    pub fn drag_offset(&self) -> (f32, f32) {
        if self.render_mode != RenderMode::Gpu {
            return (0.0, 0.0);
        }
        let s = self.scale_f();
        let (dx, dy) = ((self.pointer.0 - self.drag_origin.0) / s, (self.pointer.1 - self.drag_origin.1) / s);
        match &self.drag {
            Some(Drag::Nodes { applied, .. } | Drag::Group { applied, .. }) => (dx - applied.0 as f32, dy - applied.1 as f32),
            _ => (0.0, 0.0),
        }
    }

    // ---------- status ----------

    /// The agent for new sessions: the canvas setting, else the machine's.
    pub fn default_agent(&self) -> AgentKind {
        self.doc.default_agent.as_deref().and_then(AgentKind::parse).unwrap_or(self.detected_agent)
    }

    /// History summary of a session box (its agent's session), or of a
    /// subagent box's parent.
    pub fn summary(&self, node: &Node) -> Option<&SessionSummary> {
        match node.kind {
            NodeKind::Session => self.sessions.get(native_id(node)),
            NodeKind::Agent => {
                let parent = node.parent.as_deref()?;
                // Usually the parent box id is its session id (one hash lookup per box).
                self.sessions.get(parent).or_else(|| self.doc.node(parent).map(native_id).filter(|n| *n != parent).and_then(|n| self.sessions.get(n)))
            }
            NodeKind::Automation => None,
        }
    }

    /// Agent sessions already on the canvas (box ids and learned agent ids).
    pub fn native_ids(&self) -> HashSet<&str> {
        self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).flat_map(|n| [n.id.as_str(), native_id(n)]).collect()
    }

    /// The terminal (box id) of a session box or of a subagent's parent.
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
        node.kind == NodeKind::Session && !self.owned_alive(&node.id) && self.summary(node).is_some_and(|s| s.live)
    }

    pub fn node_status(&self, node: &Node) -> Status {
        let now = now_ms();
        match node.kind {
            NodeKind::Session if node.is_canvas_agent() => self.canvas_agent_status(node),
            NodeKind::Session if node.passive.is_some() => {
                // A passive session shows its reported state; runs show as working.
                let view = self.passive_view(node);
                if view.is_some_and(|v| v.running) || self.owned_alive(&node.id) && status::session_status(self.summary(node), self.terms.get(&node.id), node.reviewed_ms) == Status::Working {
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
                status::session_status(self.summary(node), self.terms.get(&node.id), node.reviewed_ms)
            }
            NodeKind::Automation => match node.automation.as_deref().and_then(|k| self.automation(k)) {
                Some(automation) => crate::side::automation_status(automation),
                None => Status::Unknown,
            },
            NodeKind::Agent => {
                let parent = node.parent.as_deref().unwrap_or_default();
                let summary = self.summary(node);
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
        if let Some(summary) = self.summary(node)
            && !summary.branch.is_empty()
        {
            let folder = summary.cwd.rsplit(['\\', '/']).next().unwrap_or_default();
            return format!("{folder} · {}", summary.branch);
        }
        let cwd = node.cwd.as_deref().or_else(|| self.summary(node).map(|s| s.cwd.as_str())).unwrap_or_default();
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
                // Viewing it counts as using it (the RECENT list).
                self.note_access(&session);
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
        // The group's own panel replaces a box's details (a list view's item stays).
        if !matches!(self.right_detail, Some(crate::side::RightDetail::Item(_))) {
            self.right_detail = None;
        }
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
        let mut waiting = 0;
        let mut now_waiting = HashSet::new();
        for node in self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
            match self.node_status(node) {
                Status::Waiting => {
                    state = 4;
                    waiting += 1;
                    now_waiting.insert(node.id.clone());
                }
                Status::Working if state != 4 => state = 3,
                _ => {}
            }
        }
        if state != self.outer_progress {
            self.outer_progress = state;
            let value = if state == 4 { 100 } else { 0 };
            self.platform.progress(state, value, &mut self.raw_out);
            self.needs_draw = true;
        }
        self.platform.title(waiting);
        // Sessions that started to need you since the last mirror (not the ones
        // already waiting when the canvas opened and its history loaded).
        if self.sessions_loaded
            && let Some(before) = self.waiting_sessions.replace(now_waiting.clone())
        {
            for id in now_waiting.difference(&before) {
                let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
                self.platform.needs_you(&name);
            }
        }
    }

    /// A session's program sent a desktop notification: the platform shows it
    /// (at most one per session every 3 s).
    pub fn on_notification(&mut self, term: &str, title: &str, body: &str) {
        if self.notified_at.get(term).is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return;
        }
        self.notified_at.insert(term.to_owned(), Instant::now());
        let name = self.doc.node(term).map(|n| n.name.clone()).unwrap_or_else(|| term.to_owned());
        self.platform.notification(&name, title, body);
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
            mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
        });
        self.mark_dirty();
        self.notify(format!("“{}” started subsession “{name}”.", p.name), false);
    }

    /// The service learned which agent session terminal `id` runs.
    pub fn set_agent_session(&mut self, id: &str, native: &str) {
        if apply_agent_session(&mut self.doc, id, native) {
            self.mark_dirty();
            // The history under the new id may already list subagents.
            self.sync_agents();
        }
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
                let live = self.owned_alive(&node.id) || self.summary(node).is_some_and(|s| s.live);
                if let Some(summary) = self.summary(node).filter(|s| live && s.loop_ms > 0) {
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
        let finished = self.doc.node(id).and_then(|n| self.summary(n)).and_then(|s| s.finished_ms);
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
        self.note_access(id);
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
            // A selected session shows its terminal while it runs (or starts);
            // a finished one only while you are still in it, else its details.
            let shown = |id: &String| {
                self.owned_alive(id)
                    || self.launching.contains_key(id)
                    || (self.terms.contains_key(id) && self.focus == Focus::Terminal && self.focused_term.as_ref() == Some(id))
            };
            let id = match selected_session {
                Some(session) if shown(&session) => Some(session),
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

    /// Starts a session box's terminal (see `interact.rs` for what may call it).
    pub fn launch(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        let cwd = node
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| self.summary(&node).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()))
            .unwrap_or_else(|| self.default_cwd.clone());
        let (cols, rows) = self.pane_size();
        self.node_errors.remove(id);
        // The folder is on the service's machine; when it cannot be asked, the
        // launch itself reports a missing folder.
        if self.backend.path_info(&cwd).is_ok_and(|info| !info.dir) {
            self.node_errors.insert(id.to_owned(), "folder missing".into());
            self.notify(format!("Cannot open: folder not found: {cwd}"), true);
            return;
        }
        let mut record_model = None;
        let kind = match &node.command {
            Some(command) if !command.is_empty() => LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() },
            _ => {
                let group = self.group_for_session(id);
                let prompt = self.pending_prompts.remove(id);
                // A native fork starts once from its parent; later launches resume it.
                let fork_from = self.pending_forks.remove(id).filter(|_| node.fresh);
                let launch = crate::side::agent_launch(&node, &self.doc.defaults, group.as_ref(), prompt, fork_from, !node.fresh);
                // A new conversation records the model it starts with (resumes keep it).
                if !launch.resume && launch.fork_from.is_none() && node.model.is_none() {
                    record_model = Some(launch.model.clone().unwrap_or_default());
                }
                LaunchKind::Agent(Box::new(launch))
            }
        };
        self.send(ClientMsg::Launch { id: id.to_owned(), kind, cwd: cwd.clone(), cols, rows });
        self.launching.insert(id.to_owned(), Instant::now());
        if let Some(n) = self.doc.node_mut(id) {
            n.fresh = false;
            n.cwd = Some(cwd.clone());
            if record_model.is_some() {
                n.model = record_model;
            }
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
            self.notify("No sessions in this group are running here. Double-click a session (or press Enter) to start it.", false);
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
            ServiceMsg::Frame8(frame) => {
                let id = frame.id.clone();
                self.views.entry(id).or_default().apply8(frame);
            }
            ServiceMsg::Terminals(infos) => {
                self.terms_rev += 1;
                // After a reconnect the first list is complete: forget the rest.
                if std::mem::take(&mut self.resync_terms) {
                    let known: HashSet<&str> = infos.iter().map(|i| i.id.as_str()).collect();
                    self.terms.retain(|id, _| known.contains(id.as_str()));
                    self.views.retain(|id, _| known.contains(id.as_str()));
                }
                for info in infos {
                    // Bells and notifications from a session ring the outer terminal
                    // (at most once a second).
                    if self.terms.get(&info.id).is_some_and(|old| info.bells > old.bells)
                        && self.last_bell.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                    {
                        self.last_bell = Some(Instant::now());
                        self.platform.bell(&mut self.raw_out);
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
                // A loop that starts or stops (`/every`) may also have saved a
                // session schedule: read the automations again now rather than
                // at their next 15 s refresh.
                let loop_changed = upsert.iter().any(|s| self.sessions.get(&s.id).map_or(s.loop_ms > 0, |old| (old.loop_ms > 0) != (s.loop_ms > 0)));
                if loop_changed && !self.demo && self.sessions_loaded {
                    self.load_automations();
                }
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
                // A turn you watch end needs no review: never drawn as "done · review" first.
                if self.focus == Focus::Terminal
                    && let Some(id) = self.focused_term.clone()
                {
                    self.mark_reviewed(&id);
                }
                let agent = self.default_agent();
                if !self.auto_imported && !self.demo && self.doc.nodes.is_empty() && self.sessions.values().any(|s| crate::agents::agent_of_summary(s) == agent) {
                    self.auto_imported = true;
                    // The default agent's most recent sessions.
                    let mut recent: Vec<&SessionSummary> = self.sessions.values().filter(|s| crate::agents::agent_of_summary(s) == agent).collect();
                    recent.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
                    let ids: Vec<String> = recent.into_iter().take(8).map(|s| s.id.clone()).collect();
                    let count = ids.len();
                    self.import_ids(ids, crate::dialogs::Grouping::None);
                    self.notify(format!("Added your {count} most recent {} sessions. Press I to import more or V for VS Code agents.", agent.label()), false);
                }
            }
            ServiceMsg::Clipboard(text) => self.copy_to_clipboard(&text),
            ServiceMsg::Error { id, message } => {
                if let Some(id) = &id {
                    self.launching.remove(id);
                    self.node_errors.insert(id.clone(), "launch failed".into());
                }
                // A resumed child that may not start goes again (with its own notice).
                if !id.is_some_and(|id| self.launch_refused(&id, &message)) {
                    self.notify(message, true);
                }
            }
            ServiceMsg::Stats(stats) => self.stats = Some(stats),
            ServiceMsg::Subsession { parent, child, name, cwd } => self.link_subsession(&parent, &child, &name, &cwd),
            ServiceMsg::Bus { json } => self.on_bus(&json),
            ServiceMsg::BusEvent { json } => self.on_bus_event(&json),
            ServiceMsg::AgentSession { id, native } => self.set_agent_session(&id, &native),
            ServiceMsg::Doc { key, version, json } => self.on_doc(key, version, &json),
            ServiceMsg::DocPatch { key, version, origin, ops_json } => self.on_doc_patch(&key, version, origin, &ops_json),
            ServiceMsg::DocSaved { key, error, .. } => {
                if key == self.replica.key {
                    self.save_error = error.map(|e| format!("the service cannot save the canvas: {e}"));
                }
            }
            // Answers to backend requests go to their callers on the connection's thread.
            ServiceMsg::RpcReply { .. } => {}
            ServiceMsg::Image { term, image_id, width, height, rgba_or_png, format } => {
                self.views.entry(term).or_default().set_image(image_id, width, height, format, rgba_or_png);
            }
            ServiceMsg::Placements { term, placements } => self.views.entry(term).or_default().set_placements(placements),
            ServiceMsg::Notification { term, title, body } => self.on_notification(&term, &title, &body),
            ServiceMsg::Reports { json } => self.on_reports(&json),
            ServiceMsg::Ready { .. }
            | ServiceMsg::Ready8 { .. }
            | ServiceMsg::Heartbeat { .. }
            | ServiceMsg::Challenge { .. }
            | ServiceMsg::Paired { .. }
            | ServiceMsg::PairChallenge { .. }
            | ServiceMsg::Refusal { .. }
            | ServiceMsg::Welcome { .. }
            // Only for the iPhone client (CAP_TUNNEL_ACCESS).
            | ServiceMsg::TunnelAccess { .. } => {}
        }
    }

    /// Copilot asked to copy (OSC 52): use the Windows clipboard directly; the
    /// console belongs to the output thread.
    pub fn copy_to_clipboard(&mut self, text: &str) {
        match self.platform.set_clipboard_text(text) {
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
                let summary = self.sessions.get(native_id(parent))?;
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
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
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
        let present: HashSet<String> = self.native_ids().into_iter().map(str::to_owned).collect();
        for id in ids {
            if present.contains(&id) {
                continue;
            }
            let Some(summary) = self.sessions.get(&id) else { continue };
            let agent = crate::agents::agent_of_summary(summary);
            let key = match grouping {
                Grouping::None => None,
                Grouping::Folder => {
                    let root = if summary.git_root.is_empty() { &summary.cwd } else { &summary.git_root };
                    Some(root.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("Other").to_owned())
                }
                Grouping::Client => Some(client_label(summary).to_owned()),
            };
            let seed = Seed { id: id.clone(), name: summary.name.clone(), cwd: summary.cwd.clone(), source: None, agent: (agent != AgentKind::Copilot).then(|| agent.id().to_owned()) };
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
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: seed.agent, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
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
            let summary = self.summary(node);
            let typed = self.last_input_at.get(id).map(|t| now.saturating_sub(t.elapsed().as_millis() as u64)).unwrap_or(0);
            let last = [info.started_ms, info.last_output_ms, typed, summary.map(|s| s.last_event_ms.max(s.updated_ms)).unwrap_or(0)].into_iter().max().unwrap_or(0);
            if now.saturating_sub(last) >= limit {
                idle.push(id.clone());
            }
        }
        for id in &idle {
            self.stop_terminal(id, false);
        }
        if !idle.is_empty() {
            self.notify(format!("Stopped {} session(s) idle for {minutes}+ min to free memory; double-click one to resume it.", idle.len()), false);
        }
    }

    /// Options that open this canvas in another front end: the same service
    /// (instance, data folder, `--remote`), canvas and default folder.
    pub fn other_client_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(instance) = &self.instance {
            args.extend(["--instance".to_owned(), instance.name.clone(), "--home".to_owned(), instance.home.display().to_string()]);
            args.extend(["--sessions-dir".to_owned(), instance.sessions_dir.display().to_string()]);
        }
        if let Some(remote) = self.target.as_ref().and_then(|t| t.remote_arg()) {
            args.extend(["--remote".to_owned(), remote]);
        }
        if let Some(canvas) = &self.doc_request.0 {
            args.extend(["--canvas".to_owned(), canvas.clone()]);
        }
        if self.doc_request.1 {
            args.push("--demo".to_owned());
        }
        args.extend(["--cwd".to_owned(), self.default_cwd.clone()]);
        args
    }

    /// Stops a terminal: agents exit gracefully (Ctrl+D; Claude Code wants it
    /// twice), anything else is ended.
    pub fn stop_terminal(&mut self, id: &str, force: bool) {
        self.send(ClientMsg::Stop { id: id.to_owned(), force });
        let claude = self.doc.node(id).is_some_and(|n| n.command.is_none() && agent_of(n) == AgentKind::Claude);
        if !force && claude {
            let input = Input::Key { key: crate::protocol::Key::Char('d'), mods: crate::protocol::MOD_CTRL };
            self.send(ClientMsg::Input { id: id.to_owned(), input });
        }
    }

    pub fn after_draw(&mut self) {
        let visible: Vec<String> = self.layout.tiles.iter().map(|(id, _, _)| id.clone()).collect();
        // The focused tile gets frames at a higher rate over remote links.
        let focused = self.focused_term.clone().filter(|id| visible.contains(id));
        if visible != self.last_subscribed || focused != self.last_focused_sub {
            self.send(ClientMsg::Subscribe8 { ids: visible.clone(), focused: focused.clone() });
            self.last_subscribed = visible;
            self.last_focused_sub = focused;
        }
        // The service forgets this client's size for terminals it stopped
        // showing (sizes follow the client typing in a terminal: the iPhone
        // may take one over); showing one again sends its size again.
        self.last_sizes.retain(|id, _| self.last_subscribed.contains(id));
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
        // History is indexed by the agents' own session ids.
        let mut tracked: Vec<String> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Session && n.command.is_none())
            .map(|n| native_id(n).to_owned())
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
        let frame = self.frozen_tick.unwrap_or_else(|| (START.get_or_init(Instant::now).elapsed().as_millis() / 80) as usize);
        let spun = frame != self.tick;
        self.tick = frame;
        if self.dirty_at.is_some_and(|t| t.elapsed() >= DOC_FLUSH) {
            self.flush_doc();
        }
        if self.prefs_dirty_at.is_some_and(|t| t.elapsed() >= PREFS_FLUSH) {
            self.save_prefs();
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
        self.tick_inspector();
        self.tick_recent();
        self.visual_tick();
        // Failed automations pulse until their next good run. Loop dots: the
        // TUI redraws when a visible one moves to another cell; the GPU
        // client moves them on its own (only its loop layer, only in view).
        let animated = self.doc.nodes.iter().any(|n| {
            let status = self.node_status(n);
            status.animated() || n.kind == NodeKind::Automation && status == Status::Failed
        });
        let loops_moved = self.loop_cells_moved();
        if (animated && spun) || loops_moved || self.flash.is_some() {
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

/// Terminal input: blocked while offline, never queued.
fn is_terminal_input(message: &ClientMsg) -> bool {
    matches!(
        message,
        ClientMsg::Input { .. } | ClientMsg::Input8 { .. } | ClientMsg::Click { .. } | ClientMsg::Wheel { .. } | ClientMsg::Scroll { .. }
    )
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
    /// Agent of the session (`None`: Copilot).
    pub agent: Option<String>,
}

/// "Copilot CLI", "VS Code", "OpenCode", "Claude Code": where a session comes from.
pub fn client_label(summary: &SessionSummary) -> &str {
    match (crate::agents::agent_of_summary(summary), summary.client.as_str()) {
        (AgentKind::Copilot, "vscode-agent-host") => "VS Code",
        (AgentKind::Copilot, "github/cli" | "") => "Copilot CLI",
        (AgentKind::Copilot, other) => other,
        (kind, _) => kind.label(),
    }
}

/// Points session box `id` at the agent's own session `native` (OpenCode
/// learns it after the first prompt). Returns true when the box changed.
pub fn apply_agent_session(doc: &mut Doc, id: &str, native: &str) -> bool {
    let Some(node) = doc.node_mut(id).filter(|n| n.kind == NodeKind::Session && !native.is_empty()) else { return false };
    let value = (native != id).then(|| native.to_owned());
    if node.agent_session == value {
        return false;
    }
    node.agent_session = value;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learned_agent_sessions_rekey_their_box_once() {
        let mut doc = Doc::default();
        doc.nodes.push(serde_json::from_value(serde_json::json!({ "id": "t1", "kind": "session", "name": "n", "x": 0, "y": 0, "agent": "opencode" })).unwrap());
        assert!(apply_agent_session(&mut doc, "t1", "ses_abc"));
        assert_eq!((doc.nodes[0].agent_session.as_deref(), native_id(&doc.nodes[0])), (Some("ses_abc"), "ses_abc"));
        assert!(!apply_agent_session(&mut doc, "t1", "ses_abc"), "unchanged: nothing to save");
        assert!(!apply_agent_session(&mut doc, "missing", "ses_x"), "unknown boxes are ignored");
        assert!(apply_agent_session(&mut doc, "t1", "t1"), "the box id itself clears the mapping");
        assert_eq!(doc.nodes[0].agent_session, None);
    }

    #[test]
    fn client_labels_name_the_agent() {
        let summary = |id: &str, client: &str| SessionSummary { id: id.into(), client: client.into(), ..Default::default() };
        assert_eq!(client_label(&summary("u", "vscode-agent-host")), "VS Code");
        assert_eq!(client_label(&summary("u", "github/cli")), "Copilot CLI");
        assert_eq!(client_label(&summary("ses_x", "opencode")), "OpenCode");
        assert_eq!(client_label(&summary("u", "claude-code")), "Claude Code");
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use crossbeam_channel::{Receiver, bounded, unbounded};

    fn detached() -> (Conn, Receiver<ClientMsg>) {
        let (tx, rx) = bounded(64);
        let (_service, service_rx) = bounded(1);
        (Conn::detached(tx, service_rx), rx)
    }

    #[test]
    fn the_other_client_opens_the_same_service_and_canvas() {
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), true, r"C:\work".into(), app_tx, None);
        let home = std::env::temp_dir().join(format!("cc-other-{}", uuid::Uuid::new_v4()));
        let instance = crate::paths::Instance::new(Some("dogfood".into()), Some(home.clone()), Some(home.join("sessions"))).unwrap();
        app.instance = Some(instance.clone());
        app.target = Some(crate::conn::ConnTarget::Loopback(instance.clone()));
        app.doc_request = (Some(r"C:\c\canvas.json".into()), true);
        let args = app.other_client_args();
        let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
        assert_eq!(after("--instance").as_deref(), Some("dogfood"));
        assert_eq!(after("--home"), Some(instance.home.display().to_string()));
        assert_eq!(after("--remote").as_deref(), Some("loopback"));
        assert_eq!(after("--canvas").as_deref(), Some(r"C:\c\canvas.json"));
        assert_eq!(after("--cwd").as_deref(), Some(r"C:\work"));
        assert!(args.iter().any(|a| a == "--demo"));
        app.target = Some(crate::conn::ConnTarget::Local(instance));
        assert!(!app.other_client_args().iter().any(|a| a == "--remote"), "the local pipe needs no --remote");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn offline_input_is_dropped_and_a_reconnect_resends_state_and_reports_lost_keys() {
        let (conn, sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        app.connected = false;
        app.send(ClientMsg::Input { id: "t".into(), input: Input::Paste("rm".into()) });
        app.send(ClientMsg::Wheel { id: "t".into(), up: true, col: 0, row: 0, mods: 0 });
        app.send(ClientMsg::Stats);
        assert!(sent.try_recv().is_err(), "nothing is queued while offline");
        assert_eq!(app.notice.as_ref().map(|n| n.text.as_str()), Some("Offline: input not sent."));

        let (conn, sent) = detached();
        for _ in 0..5 {
            conn.session.next_seq();
        }
        conn.session.ack(3);
        app.last_subscribed = vec!["t".into()];
        app.last_focused_sub = Some("t".into());
        app.last_sizes.insert("t".into(), (80, 24));
        app.last_tracked = vec!["t".into()];
        app.routing_sent = "{}".into();
        app.reconnecting = true;
        app.banner = Some("Offline — reconnecting…".into());
        app.on_reconnected(Ok(conn));
        assert!(app.connected && !app.reconnecting && app.banner.is_none());
        assert!(app.last_subscribed.is_empty() && app.last_focused_sub.is_none() && app.last_sizes.is_empty());
        assert!(app.last_tracked.is_empty() && app.routing_sent.is_empty() && app.resync_terms);
        assert_eq!(app.notice.as_ref().map(|n| n.text.as_str()), Some("Reconnected. 2 keystroke(s) were not delivered."));
        app.send(ClientMsg::Input { id: "t".into(), input: Input::Paste("ls".into()) });
        assert!(matches!(sent.try_recv(), Ok(ClientMsg::Input { .. })), "online input goes out");

        // The first terminal list after a reconnect is complete.
        app.terms.insert("gone".into(), TermInfo { id: "gone".into(), alive: true, ..Default::default() });
        app.views.insert("gone".into(), TermView::default());
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "t".into(), alive: true, ..Default::default() }]));
        assert!(!app.terms.contains_key("gone") && !app.views.contains_key("gone") && app.terms.contains_key("t"));

        // A drop without a reconnect target says so; with one, the banner shows.
        drop(sent);
        app.target = None;
        app.send(ClientMsg::Stats);
        assert!(!app.connected && app.banner.is_none());
        app.target = Some(crate::conn::ConnTarget::Tcp { addr: "127.0.0.1:1".into(), remote_id: None, secret: vec![1] });
        app.reconnecting = false;
        app.connection_lost();
        assert!(app.reconnecting && app.banner.as_deref() == Some("Offline — reconnecting…"));
    }

    fn node(id: &str) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0, "reviewed_ms": 1 })).unwrap()
    }

    fn edits(sent: &Receiver<ClientMsg>) -> Vec<(u64, Vec<crate::sync::Op>)> {
        sent.try_iter().filter_map(|m| if let ClientMsg::DocEdit { base, ops_json, .. } = m { Some((base, serde_json::from_str(&ops_json).unwrap())) } else { None }).collect()
    }

    #[test]
    fn document_edits_go_to_the_service_and_other_windows_patches_come_back() {
        use crate::sync::Op;
        let (conn, sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: vec![node("a"), node("b")], ..Doc::default() };
        let opened = Opened { key: "k".into(), version: 1, path: String::new(), client: 7, doc };
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), opened, false, ".".into(), app_tx, None);
        // A move becomes one edit; view changes are this client's own.
        app.doc.node_mut("a").unwrap().x = 5;
        app.doc.view.x = 99;
        app.mark_dirty();
        app.flush_doc();
        let sent_edits = edits(&sent);
        assert_eq!(sent_edits.len(), 1);
        let (base, ops) = &sent_edits[0];
        assert!(*base == 1 && matches!(ops.as_slice(), [Op::NodeFields(n, f)] if n.id == "a" && n.x == 5 && *f == ["x"]), "{ops:?}");
        app.flush_doc();
        assert!(edits(&sent).is_empty(), "nothing new to send");
        // The service applied it (our own patch), then another window moved and renamed "a" and removed "b".
        let own = serde_json::to_string(ops).unwrap();
        app.on_service(ServiceMsg::DocPatch { key: "k".into(), version: 2, origin: 7, ops_json: own });
        assert_eq!(app.replica.in_flight(), 0);
        app.selection = vec!["b".into()];
        let mut renamed = app.doc.node("a").unwrap().clone();
        renamed.name = "renamed elsewhere".into();
        renamed.x = 7;
        let theirs = serde_json::to_string(&[Op::NodeFields(Box::new(renamed), vec!["name".into(), "x".into()]), Op::DelNode("b".into())]).unwrap();
        app.on_service(ServiceMsg::DocPatch { key: "k".into(), version: 3, origin: 8, ops_json: theirs });
        assert_eq!((app.doc.node("a").unwrap().name.as_str(), app.doc.node("a").unwrap().x), ("renamed elsewhere", 7));
        assert!(app.doc.node("b").is_none() && app.selection.is_empty(), "a removed box leaves the selection");
        assert_eq!(app.notice.as_ref().map(|n| n.text.as_str()), Some("“a” was changed in another window."));
        assert_eq!(app.doc.view.x, 99, "patches never touch the view");
        // A missing patch asks for the whole document; it comes back with local changes kept.
        app.doc.node_mut("a").unwrap().y = 3;
        app.on_service(ServiceMsg::DocPatch { key: "k".into(), version: 9, origin: 8, ops_json: "[]".into() });
        assert!(app.doc_resync && matches!(sent.try_recv(), Ok(ClientMsg::DocOpen { .. })));
        app.flush_doc();
        assert!(edits(&sent).is_empty(), "no edits while resynchronizing");
        let fresh = Doc { nodes: vec![node("a"), node("c")], ..Doc::default() };
        let reply = crate::sync::DocReply { path: "p".into(), client: 9, doc: Some(fresh), error: None };
        app.on_service(ServiceMsg::Doc { key: "k".into(), version: 9, json: serde_json::to_string(&reply).unwrap() });
        assert!(!app.doc_resync && app.replica.client == 9 && app.doc.node("c").is_some());
        assert_eq!(app.doc.node("a").unwrap().y, 3, "the local change survives the resync");
        app.flush_doc();
        let resent = edits(&sent);
        assert!(matches!(resent.as_slice(), [(9, ops)] if ops.len() == 1), "{resent:?}");
        // Save errors from the service are shown.
        app.on_service(ServiceMsg::DocSaved { key: "k".into(), version: 9, error: Some("disk full".into()) });
        assert_eq!(app.save_error.as_deref(), Some("the service cannot save the canvas: disk full"));
    }

    #[test]
    fn a_turn_that_ends_while_you_watch_its_terminal_needs_no_review() {
        use crate::protocol::Activity;
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut box_ = node("t1");
        box_.agent = Some("opencode".into());
        box_.agent_session = Some("ses_x".into());
        let doc = Doc { nodes: vec![box_], ..Doc::default() };
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        app.terms.insert("t1".into(), TermInfo { id: "t1".into(), alive: true, ..Default::default() });
        let working = SessionSummary { id: "ses_x".into(), activity: Activity::Working, live: true, tracked: true, ..Default::default() };
        app.on_service(ServiceMsg::Sessions { full: true, upsert: vec![working.clone()], remove: Vec::new() });
        app.focused_term = Some("t1".into());
        app.focus = Focus::Terminal;
        app.on_tick();
        let status = |app: &App| app.node_status(app.doc.node("t1").unwrap());
        assert_eq!(status(&app), Status::Working);
        let done = SessionSummary { activity: Activity::Idle, finished_ms: Some(now_ms()), ..working };
        app.on_service(ServiceMsg::Sessions { full: false, upsert: vec![done], remove: Vec::new() });
        assert_eq!(status(&app), Status::Idle, "you saw it finish: no review flash before the next tick");
        // Not watching it: the finished turn waits for you.
        app.focus = Focus::Canvas;
        let later = SessionSummary { id: "ses_x".into(), activity: Activity::Idle, finished_ms: Some(now_ms() + 1), live: true, tracked: true, ..Default::default() };
        app.on_service(ServiceMsg::Sessions { full: false, upsert: vec![later], remove: Vec::new() });
        app.on_tick();
        assert_eq!(status(&app), Status::JustFinished);
    }
}
