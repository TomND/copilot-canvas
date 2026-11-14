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
/// A document asked for and not received by then is asked for again (a
/// lost `DocOpen` or `Doc` would otherwise hold back every edit for good).
pub const DOC_RETRY: Duration = Duration::from_secs(10);
/// View and panel changes are written to the client's preferences this soon.
const PREFS_FLUSH: Duration = Duration::from_millis(400);
/// A session whose box's terminal ended here and whose summary still says
/// live this long after, unchanged, is taken as no longer live (R2-D-03).
pub const LIVE_DOUBT: Duration = Duration::from_secs(5);
/// A launch refused because the box's own previous run is still closing is
/// sent again once, this long after (R2-D-10).
pub const CLOSING_RETRY: Duration = Duration::from_millis(2500);

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

pub use crate::undo::Undo;
pub use canvas_termview::{TermSelection, TermView, user_message_rows};

#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// A session's `live` in doubt ([`App::live_doubted`]): its box, since when,
/// and the summary as it was then (the same one sent again is no news).
pub struct LiveDoubt {
    pub box_id: String,
    pub since: Instant,
    pub summary: SessionSummary,
}

/// A bus snapshot this long after a restore cancel that still lists the box
/// comes from a service that ignored the cancel (an earlier one may predate it).
pub const RESTORE_CANCEL_SETTLE: Duration = Duration::from_secs(3);

/// The first wait before the backend's facts are asked again after no answer.
pub const INFO_RETRY: Duration = Duration::from_secs(15);

/// Asking the backend for its facts again ([`App::info_check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoCheck {
    Idle,
    /// A request is under way.
    Asking,
    /// No answer: asked again at `at` (`tries` so far).
    Retry { at: Instant, tries: u32 },
}

/// What a launch used that only its first start uses ([`App::launch_once`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LaunchOnce {
    pub prompt: Option<String>,
    pub fork: Option<String>,
    /// The box was new (a new conversation, not a resume).
    pub fresh: bool,
}

/// Box removals the last run kept ([`App::held_removals`]).
#[derive(Debug)]
pub struct HeldRemovals {
    pub kept: Vec<crate::sync::Kept>,
    /// When the last run kept them (kept again at quit with this time).
    pub saved_ms: u64,
    /// The service's terminal list came on this connection.
    pub terms_in: bool,
}

/// What became of the changes the last run kept (`App::new`).
pub struct Restored {
    pub replayed: crate::sync::Replayed,
    /// Dropped unread: older than [`crate::prefs::UNSENT_MAX_AGE_MS`].
    pub stale: usize,
    /// How long ago they were kept.
    pub age_ms: u64,
    pub saved_ms: u64,
}

/// Replays the changes the last run kept on `doc` (the service's canvas as
/// it is now): none when they are older than a day.
pub fn replay_unsent(doc: &mut Doc, unsent: &crate::prefs::Unsent, now_ms: u64) -> Restored {
    let age_ms = now_ms.saturating_sub(unsent.saved_ms);
    if age_ms > crate::prefs::UNSENT_MAX_AGE_MS {
        return Restored { replayed: Default::default(), stale: unsent.kept.len() + unsent.ops.len(), age_ms, saved_ms: unsent.saved_ms };
    }
    Restored { replayed: crate::sync::replay(doc, &unsent.kept, &unsent.ops), stale: 0, age_ms, saved_ms: unsent.saved_ms }
}

/// The first few names of a list, for a notice ("“a”, “b” and 3 more").
fn some_names(names: &[String]) -> String {
    let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    match names.len() {
        0..=3 => shown,
        n => format!("{shown} and {} more", n - 3),
    }
}

pub enum Drag {
    Nodes { ids: Vec<String>, start: (u16, u16), applied: (i32, i32), moved: bool, click: Option<String> },
    /// A group dragged by its title: what moves (`nodes`, `groups`: the
    /// group, its nested groups) was decided when the press came, so boxes
    /// and groups it passes over stay where they are.
    Group { id: String, nodes: Vec<String>, groups: Vec<String>, start: (u16, u16), applied: (i32, i32), moved: bool },
    Resize { id: String, start: (u16, u16), orig: (i32, i32) },
    /// Panning; `group`: pressed on a group's background or border (a click
    /// without moving selects that group).
    Pan { last: (u16, u16), moved: bool, group: Option<String> },
    /// An area selection (Shift- or Ctrl-drag, right-drag), wherever it
    /// started; `click`: what the press was on, for a release that did not
    /// move (a Shift-click toggles a box, a right-click opens its menu).
    Rubber { start: (u16, u16), now: (u16, u16), additive: bool, right: bool, click: Option<CanvasPress> },
    SidebarDivider,
    TermDivider,
    /// Selecting text in a terminal pane.
    Select { id: String, anchor: (u16, u16), moved: bool },
    /// A press in the Notebook's or Tasks' main area or sidebar: its drag and
    /// release go to that module too.
    Space,
}

/// What a press that begins an area selection was on.
#[derive(Clone, Debug, PartialEq)]
pub enum CanvasPress {
    Node(String),
    /// A stack's deck (its parent's id).
    Deck(String),
    /// A group's title or ◢ corner.
    Group(String),
}

#[derive(Default, Clone)]
pub struct Layout {
    pub screen: Rect,
    pub toolbar: Rect,
    pub sidebar: Rect,
    pub canvas: Rect,
    pub terms: Rect,
    /// Where the right panel is when it shows (`terms` is empty while it
    /// does not): a launch's size before its tile exists.
    pub terms_shown: Rect,
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
    /// Gpu mode: the HTML page to show over a rectangle of cells (the open
    /// HTML report's content area; the GUI shows it in its HTML view there).
    pub html_pane: Option<crate::view::HtmlPane>,
    /// Areas the Notebook or Tasks module drew (plain-text links are not
    /// looked for there: the module registers its own).
    pub space_areas: Vec<Rect>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideKind {
    Group(String),
    Session(String),
    Agent(String),
    Section,
    /// A group's folders (its menu opens them).
    Folder(String),
    /// A group's passive sessions header (a click opens their panel; the
    /// sessions are listed beneath it).
    Passives(String),
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
    /// What Ctrl+Z restores: this window's moves, resizes, removals and
    /// VS Code syncs, newest last (`undo.rs`).
    pub undo: crate::undo::History,
    /// While wheel/trackpad panning, the dot grid is hidden until this time.
    pub pan_quiet_until: Option<Instant>,
    /// Momentum after a background pan (`glide.rs`).
    pub glide: Option<crate::glide::Glide>,
    /// The pointer's recent places during a background pan.
    pub pan_track: crate::glide::PanTrack,
    /// The trackpad gesture in progress and what it is latched to (`gesture.rs`).
    pub gesture: Option<crate::gesture::GestureLatch>,
    /// A running smart-zoom animation of the canvas view.
    pub view_anim: Option<crate::gesture::ViewAnim>,
    /// The view a smart zoom left (a second one returns to it).
    pub smart_back: Option<crate::gesture::ViewState>,
    /// Text size steps asked for by pinching over terminals (the GUI applies
    /// and clears them).
    pub text_zoom_steps: i32,
    /// Command+0 in a terminal: the text size back to the default.
    pub text_zoom_reset: bool,
    /// How the front end's pointer scrolls (help text; MAC-LINUX-PLAN D15).
    pub pointer_policy: crate::input::PointerPolicy,
    /// How the GUI's keyboard names the app's shortcuts in hints (macOS: ⌘).
    pub key_policy: crate::input::KeyPolicy,
    /// Ctrl+L: repaint every cell on the next frame.
    pub repaint_requested: bool,
    /// Gpu mode: show the print dialog of the HTML pane on the next frame
    /// (a one-shot request; the GUI's web view prints or saves a PDF).
    pub html_print: bool,
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
    /// When the document was last asked for (asked again after [`DOC_RETRY`]).
    pub doc_asked_at: Instant,
    /// `DocOpen`s sent on this connection and not answered yet.
    pub docs_asked: u32,
    /// Box removals kept by the last run, waiting for the service's terminal
    /// list: made then for boxes that do not run again (R3-C-02).
    pub held_removals: Option<HeldRemovals>,
    /// How many times the document was asked for again without an answer.
    pub doc_retries: u32,
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
    /// Asking the backend for `info` again (in the background).
    pub info_check: InfoCheck,
    pub conn: Conn,
    pub connected: bool,
    /// Where the service is (reconnects go there); `None`: never reconnect.
    pub target: Option<crate::conn::ConnTarget>,
    pub conn_options: crate::conn::ConnOptions,
    /// A reconnect is in progress.
    pub reconnecting: bool,
    /// The local service was stopped on purpose (`atelier service stop`):
    /// the reconnect does not start it; ▶ Start service does (R3-B-01).
    pub service_stopped: bool,
    /// When this connection was made (a stop recorded since is this one's).
    pub connected_at: std::time::SystemTime,
    /// Connection banner for front ends ("Offline — reconnecting…").
    pub banner: Option<String>,
    /// The first terminal list after a reconnect replaces the known terminals.
    pub resync_terms: bool,
    /// Focused tile last sent with `Subscribe8`.
    pub last_focused_sub: Option<String>,
    pub sessions: HashMap<String, SessionSummary>,
    pub sessions_loaded: bool,
    /// Sessions whose box's terminal ended here while their summary said
    /// live (session id → doubt). Still live and unchanged after
    /// [`LIVE_DOUBT`], the summary is stale (an update an older host lost
    /// would keep the box "open elsewhere" until the next reconnect).
    pub live_doubted: HashMap<String, LiveDoubt>,
    pub terms: HashMap<String, TermInfo>,
    pub views: HashMap<String, TermView>,
    pub launching: HashMap<String, Instant>,
    /// Boxes whose launch was refused because their own previous run is
    /// still closing: when it goes again ([`CLOSING_RETRY`]) …
    pub closing_retry: HashMap<String, Instant>,
    /// … and those sent again already (a second such refusal is shown).
    pub closing_retried: HashSet<String>,
    /// Boxes this window asked the restore not to resume, and when: told
    /// once the service's snapshot says whether it did (R3-C-09).
    pub restore_cancels: HashMap<String, Instant>,
    /// What each launch under way used only once (its first prompt, its
    /// native fork, `fresh`): given back when the service refuses it.
    pub launch_once: HashMap<String, LaunchOnce>,
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
    /// The size last sent (`Resize`) for each shown terminal.
    pub last_sizes: HashMap<String, (u16, u16)>,
    /// Tile sizes waiting to hold before they are sent: id → (size, since).
    pub pending_sizes: HashMap<String, ((u16, u16), Instant)>,
    /// How each shown terminal's real size compares with its tile (`sizes.rs`).
    pub size_states: HashMap<String, crate::sizes::SizeState>,
    /// The terminal this client last claimed the size of (it has the keyboard here).
    pub last_claimed: Option<String>,
    /// The terminal whose claim this client believes the service still
    /// holds: only it is claimed again by a size repair (not after another
    /// client took it, nor after a reconnect, until you act here).
    pub claim_held: Option<String>,
    /// The size each shown terminal had when this client last asked for
    /// another, and when: going back to it soon after is a resize undone,
    /// not another client's (`sizes::REVERT_WITHIN`).
    pub resized_from: HashMap<String, ((u16, u16), Instant)>,
    pub last_tracked: Vec<String>,
    pub stats: Option<ServiceStats>,
    pub stats_at: Instant,
    /// Bytes read on the connection, and when that count last moved: a
    /// connection that went silent before it ended suspects the tunnel.
    pub heard: (u64, Instant),
    /// Keeps the terminal and session lists in step with the service's (`resync.rs`).
    pub resync: crate::resync::Resync,
    pub build_mismatch: bool,
    /// A remote service of another build (its build id): mixed builds are
    /// unsupported, so the toolbar says so ([`App::note_mixed_builds`]).
    pub mixed_builds: Option<String>,
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
    /// Remote mode: sessions the service said have history to fork (agent session ids).
    pub fork_history: HashMap<String, bool>,
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
    /// The ✦ Skills view: where every session's skills come from (`skills.rs`).
    pub skills: crate::skills::SkillsState,
    /// The Reports inbox and its viewer (`reports.rs`).
    pub reports: crate::reports::ReportsState,
    /// What the main area shows: the session canvas, the notebook or the
    /// todo boards (`spaces.rs`; kept in this window's preferences).
    pub space: crate::spaces::Space,
    /// Where this window has been (Alt+← / Alt+→).
    pub nav: crate::spaces::History,
    /// The Notebook space (`notebook/`).
    pub notebook: crate::notebook::State,
    /// The Tasks space (`tasks/`).
    pub tasks: crate::tasks::State,
    /// The canvas-wide agents' window state: chats waiting for a run, open settings sections, rules read.
    pub agent_ui: crate::canvas_wide::AgentsUi,
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
        let build_mismatch = conn.outdated();
        let replica = Replica::new(&opened);
        let mut doc = opened.doc;
        let mut prefs = prefs.map(PrefsFile::new);
        let mut space = crate::spaces::Space::Canvas;
        // Canvas changes the last run could not send: replayed where the canvas
        // still holds what they were made from, then their file goes (R3-C-02).
        let mut replayed = None;
        if let Some(file) = prefs.as_mut() {
            let loaded = file.load().unwrap_or_else(|| ClientPrefs::of(&doc));
            space = loaded.space.as_deref().and_then(crate::spaces::Space::parse).unwrap_or_default();
            loaded.apply(&mut doc);
            if let Some(unsent) = file.load_unsent() {
                file.clear_unsent();
                if unsent.key == replica.key {
                    replayed = Some(replay_unsent(&mut doc, &unsent, now_ms()));
                }
            }
        }
        // The backend's facts as the last run got them, asked for again in
        // the background (a remote service's round trip, or the local agents'
        // lookup, no longer delays the first frame); asked now the first time.
        let cached = prefs.as_ref().and_then(PrefsFile::load_info);
        let refresh = cached.is_some();
        let (info, info_error) = match cached.map_or_else(|| backend.info(), Ok) {
            Ok(info) => (info, None),
            Err(error) => (BackendInfo::default(), Some(format!("{error:#}"))),
        };
        if !refresh
            && info_error.is_none()
            && let Some(file) = &prefs
        {
            let _ = file.save_info(&info);
        }
        // A remote service is not asked here (one round trip less before the
        // first frame): the first automations load, in the background, says it.
        let automations_folder = if conn.remote { String::new() } else { backend.automations_dir(doc.automations_dir.clone()).unwrap_or_default() };
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
            doc_asked_at: Instant::now(),
            docs_asked: 0,
            held_removals: None,
            doc_retries: 0,
            dirty_at: None,
            prefs_dirty_at: None,
            prefs,
            save_error: None,
            backend,
            info,
            info_check: InfoCheck::Idle,
            conn,
            connected: true,
            target: None,
            conn_options: crate::conn::ConnOptions::default(),
            reconnecting: false,
            service_stopped: false,
            connected_at: std::time::SystemTime::now(),
            banner: None,
            resync_terms: false,
            last_focused_sub: None,
            sessions: HashMap::new(),
            sessions_loaded: false,
            live_doubted: HashMap::new(),
            terms: HashMap::new(),
            views: HashMap::new(),
            launching: HashMap::new(),
            closing_retry: HashMap::new(),
            closing_retried: HashSet::new(),
            launch_once: HashMap::new(),
            restore_cancels: HashMap::new(),
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
            pending_sizes: HashMap::new(),
            size_states: HashMap::new(),
            last_claimed: None,
            claim_held: None,
            resized_from: HashMap::new(),
            last_tracked: Vec::new(),
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(60),
            heard: (0, Instant::now()),
            resync: Default::default(),
            last_input_at: HashMap::new(),
            idle_check_at: Instant::now(),
            repaint_requested: false,
            html_print: false,
            pan_quiet_until: None,
            glide: None,
            pan_track: Default::default(),
            gesture: None,
            view_anim: None,
            smart_back: None,
            text_zoom_steps: 0,
            text_zoom_reset: false,
            pointer_policy: Default::default(),
            key_policy: Default::default(),
            side_rows_key: 0,
            sessions_rev: 0,
            terms_rev: 0,
            undo: Default::default(),
            term_select: None,
            perf: None,
            build_mismatch,
            mixed_builds: None,
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
            fork_history: HashMap::new(),
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
            skills: Default::default(),
            reports: Default::default(),
            space,
            nav: Default::default(),
            notebook: Default::default(),
            tasks: Default::default(),
            agent_ui: Default::default(),
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
            // Asked again in the background until it answers (R3-C-06).
            app.info_check = InfoCheck::Retry { at: Instant::now() + INFO_RETRY, tries: 1 };
        }
        let differs = app.conn.remote_build_differs().map(str::to_owned);
        app.note_mixed_builds(differs);
        if refresh {
            app.refresh_backend_info();
        }
        if let Some(replayed) = replayed {
            app.after_replay(replayed);
        }
        app
    }

    /// Asks the backend for its facts on a background thread; they replace
    /// the copy the app started with when they come. Without an answer they
    /// are asked again, 15 s later, then 30 s, then every minute (the
    /// failure is said once); a reconnect asks again too (R3-C-06).
    pub fn refresh_backend_info(&mut self) {
        let tries = match self.info_check {
            InfoCheck::Asking => return,
            InfoCheck::Retry { tries, .. } => tries,
            InfoCheck::Idle => 0,
        };
        self.info_check = InfoCheck::Asking;
        let (backend, tx) = (self.backend.clone(), self.app_tx.clone());
        let spawned = std::thread::Builder::new().name("backend-info".into()).spawn(move || {
            let info = backend.info();
            let _ = tx.send(AppEvent::Run(Box::new(move |app: &mut App| match info {
                Ok(info) => {
                    app.info_check = InfoCheck::Idle;
                    app.set_backend_info(info);
                }
                Err(error) => {
                    if tries == 0 {
                        app.notify(format!("The backend did not answer: {error:#}"), true);
                    }
                    let wait = (INFO_RETRY * 2u32.pow(tries.min(2))).min(Duration::from_secs(60));
                    app.info_check = InfoCheck::Retry { at: Instant::now() + wait, tries: tries + 1 };
                }
            })));
        });
        if spawned.is_err() {
            self.info_check = InfoCheck::Retry { at: Instant::now() + INFO_RETRY, tries: tries + 1 };
        }
    }

    /// The backend's facts, as they are now (kept for the next start).
    pub fn set_backend_info(&mut self, info: BackendInfo) {
        if info == self.info {
            return;
        }
        // The agent the configuration views show follows the machine's, unless chosen.
        if self.config_agent == self.detected_agent && self.doc.default_agent.as_deref().and_then(AgentKind::parse).is_none() {
            self.config_agent = info.default_agent;
        }
        self.detected_agent = info.default_agent;
        self.installed_agents = info.installed.clone();
        if let Some(file) = &self.prefs {
            let _ = file.save_info(&info);
        }
        self.info = info;
        self.side_rows_key = 0;
        self.needs_draw = true;
    }

    // ---------- notices & persistence ----------

    /// Records whether the remote service is of another build; a new
    /// difference is also said once as a notice: mixed builds are
    /// unsupported (an older client froze resuming a large session against a
    /// newer service), and how to get one build on both machines.
    pub fn note_mixed_builds(&mut self, service_build: Option<String>) {
        if service_build.is_some() && service_build != self.mixed_builds {
            self.notify(mixed_builds_notice(crate::paths::build_id(), service_build.as_deref().unwrap_or_default()), true);
        }
        self.mixed_builds = service_build;
    }

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

    /// Sends pending edits, writes changed preferences and the notes typed
    /// in the last second now (every quit, detach and window close of both
    /// clients ends here).
    pub fn save_now(&mut self) {
        crate::notebook::flush(self);
        self.flush_doc();
        self.save_prefs();
        self.save_unsent();
    }

    /// Canvas changes that cannot go now (offline, waiting for the canvas
    /// again, the service restarting) are kept for the next start instead of
    /// being lost with this client, with what they were made from (the next
    /// start replays them only where that still holds); those sent but not
    /// confirmed while offline too, and removals the last run kept that wait
    /// for the terminal list. Nothing to keep: no file.
    fn save_unsent(&mut self) {
        if self.replica.key == "detached" {
            return;
        }
        let Some(file) = &self.prefs else { return };
        let pending = !self.connected || self.doc_resync || self.restarting.is_some();
        let mut kept = if pending { self.replica.kept(&self.doc) } else { Vec::new() };
        let mut saved_ms = now_ms();
        if let Some(held) = &self.held_removals {
            kept.extend(held.kept.iter().cloned());
            saved_ms = saved_ms.min(held.saved_ms);
        }
        if kept.is_empty() {
            file.clear_unsent();
            return;
        }
        let unsent = crate::prefs::Unsent { key: self.replica.key.clone(), saved_ms, kept, ops: Vec::new() };
        if let Err(error) = file.save_unsent(&unsent) {
            self.save_error = Some(format!("cannot keep the canvas changes not sent yet: {error:#}"));
        }
    }

    /// The last run's kept changes were replayed (`App::new`): removals wait
    /// for the terminal list, and a notice says what was restored and what
    /// was dropped, and why.
    fn after_replay(&mut self, restored: Restored) {
        let Restored { replayed, stale, age_ms, saved_ms } = restored;
        if replayed.applied > 0 {
            self.mark_dirty();
        }
        if !replayed.removals.is_empty() {
            self.held_removals = Some(HeldRemovals { kept: replayed.removals.clone(), saved_ms, terms_in: false });
        }
        let mut parts = Vec::new();
        if stale > 0 {
            parts.push(format!("{stale} canvas change(s) kept {} ago were dropped (older than a day)", short_interval(age_ms)));
        }
        if replayed.applied > 0 {
            parts.push(format!("{} canvas change(s) not sent before Atelier closed last time go to the service now", replayed.applied));
        }
        if !replayed.changed.is_empty() {
            parts.push(format!("{} not applied, changed elsewhere since: {}", replayed.changed.len(), some_names(&replayed.changed)));
        }
        if !replayed.gone.is_empty() {
            parts.push(format!("{} not applied, no longer on the canvas: {}", replayed.gone.len(), some_names(&replayed.gone)));
        }
        if replayed.unchecked > 0 {
            parts.push(format!("{} kept by an older version dropped (they could not be checked)", replayed.unchecked));
        }
        if !parts.is_empty() {
            self.notify(format!("{}.", parts.join("; ")), stale + replayed.dropped() > 0);
        }
    }

    /// Makes the box removals the last run kept once the service's terminal
    /// list is in: a box whose session runs again (here, being started, or
    /// waiting in the restore) or that changed since stays, with its
    /// subagent boxes; the others go as a removal here does (`remove_nodes`).
    fn make_held_removals(&mut self) {
        let Some(held) = self.held_removals.take() else { return };
        let (mut removed, mut stay) = (0, Vec::new());
        for kept in &held.kept {
            let crate::sync::Op::DelNode(id) = &kept.op else { continue };
            let Some(node) = self.doc.node(id) else { continue };
            let runs = node.kind == NodeKind::Session
                && (self.owned_alive(id) || self.launching.contains_key(id) || self.bus.restore.as_ref().is_some_and(|r| r.is_resuming(id)));
            if runs || serde_json::to_value(node).ok() != kept.base {
                stay.push(format!("“{}”", node.name));
                continue;
            }
            if node.kind == NodeKind::Session {
                self.send(ClientMsg::Forget { id: id.clone() });
                self.tile_slots.retain(|t| t != id);
            }
            self.doc.remove_node(id);
            removed += 1;
        }
        if removed > 0 {
            self.after_remote_change();
            self.mark_dirty();
        }
        match (removed, stay.len()) {
            (0, 0) => {}
            (removed, 0) => self.notify(format!("{removed} box removal(s) kept from the last run were made."), false),
            (removed, kept) => self.notify(format!("{removed} box removal(s) kept from the last run were made; {kept} not, the box runs again or changed since: {}.", some_names(&stay)), true),
        }
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
        let mut prefs = ClientPrefs::of(&self.doc);
        // The canvas is the default space (not written).
        prefs.space = (self.space != crate::spaces::Space::Canvas).then(|| self.space.key().to_owned());
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
        self.doc_asked_at = Instant::now();
        let (path, demo) = self.doc_request.clone();
        let cwd = self.default_cwd.clone();
        if self.queue(ClientMsg::DocOpen { path, demo, cwd }) {
            self.docs_asked += 1;
        } else {
            // Not sent (a full queue): asked again in a second.
            self.doc_asked_at -= self.doc_retry_wait().saturating_sub(Duration::from_secs(1));
        }
    }

    /// How long an answer to `DocOpen` is waited for before it is asked
    /// again: [`DOC_RETRY`], twice that after a retry, and so on up to 8×
    /// (a multi-MB canvas behind other bulk on a slow tunnel is only late).
    fn doc_retry_wait(&self) -> Duration {
        DOC_RETRY * 2u32.pow(self.doc_retries.min(3))
    }

    /// The whole document from the service: an answer to `DocOpen`, or sent
    /// unasked when the service refused an edit of this client.
    fn on_doc(&mut self, key: String, version: u64, json: &str) {
        let asked = self.docs_asked > 0;
        self.docs_asked = self.docs_asked.saturating_sub(1);
        // Another answer to a document asked for again (the first was only
        // slow): nothing newer than this copy, and taking it would send the
        // changes made since the first again, over newer ones (R2-D-09).
        if asked && !self.doc_resync && key == self.replica.key && version <= self.replica.version {
            return;
        }
        let refused = !asked && !self.doc_resync;
        self.doc_resync = false;
        self.doc_retries = 0;
        match Opened::from_message(key, version, json) {
            Ok(opened) => {
                // After a refusal the changes it was sent go (S8-05); otherwise
                // those not confirmed are kept and sent again.
                if refused {
                    self.replica.resync_refused(&mut self.doc, opened);
                } else {
                    self.replica.resync(&mut self.doc, opened);
                }
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
        self.queue(message);
    }

    /// Sends a message to the service; false when it was not queued (offline,
    /// or the outgoing queue is full), so state sent once (subscriptions,
    /// sizes, tracking) is sent again later.
    pub fn queue(&mut self, message: ClientMsg) -> bool {
        let input = is_terminal_input(&message);
        if !self.connected {
            // Nothing is queued while offline: typing into a screen that changed
            // in the meantime is worse than typing again.
            if input && !matches!(message, ClientMsg::Input { input: Input::Focus(_), .. }) {
                self.notify("Offline: input not sent.", true);
            }
            return false;
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
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                if input {
                    self.notify("Offline: input not sent.", true);
                }
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                if self.restarting.is_none() {
                    self.connection_lost();
                }
                false
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
        // Silent before it ended (this client's watchdog closed it): the tunnel
        // is suspect, not the service, which closes what it ends (R2-D-02).
        let silent = self.went_silent();
        if let Some(crate::conn::ConnTarget::Server(server)) = &self.target
            && silent
        {
            server.went_silent();
        }
        let Some(target) = self.target.clone() else {
            self.notify("The terminal service disconnected. Restart Atelier to reconnect; your canvas is saved.", true);
            return;
        };
        self.reconnecting = true;
        self.banner = Some("Offline — reconnecting…".into());
        // A local service that is gone for good (a crash, a reboot) is started
        // again, unless it was stopped on purpose since this connection was
        // made: then only a deliberate start (▶ Start service, `atelier
        // service start`) brings it back, and this client reconnects to it.
        let options = crate::conn::ConnOptions { start: true, respect_stop: Some(self.connected_at), ..self.conn_options.clone() };
        let session = self.conn.session.clone();
        let tx = self.app_tx.clone();
        let spawned = std::thread::Builder::new().name("reconnect".into()).spawn(move || {
            // A refusal keeps the client retrying; the banner says why.
            let mut shown = (false, None);
            let result = crate::conn::reconnect(&target, &options, &session, |_, error| {
                let state = (error.is::<crate::conn::StoppedOnPurpose>(), crate::conn::retry_reason(error));
                if state != shown {
                    shown = state;
                    let _ = tx.send(AppEvent::Run(Box::new(move |app: &mut App| {
                        if app.reconnecting {
                            app.offline_banner(state.0, state.1);
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
            self.notify("Cannot reconnect to the terminal service; restart Atelier.", true);
        }
    }

    /// What the banner says while reconnecting: why it keeps failing, or
    /// that the service was stopped on purpose (it waits for ▶ Start service).
    fn offline_banner(&mut self, stopped: bool, reason: Option<&'static str>) {
        if stopped && !self.service_stopped {
            self.notify("The terminal service was stopped (atelier service stop), so it is not started again on its own: ▶ Start service in the toolbar starts it.", false);
        }
        self.service_stopped = stopped;
        self.banner = Some(match (stopped, reason) {
            (true, _) => "Service stopped".to_owned(),
            (false, Some(why)) => format!("Offline — reconnecting… ({why})"),
            (false, None) => "Offline — reconnecting…".to_owned(),
        });
        self.needs_draw = true;
    }

    /// ▶ Start service, while the local service was stopped on purpose: its
    /// record goes, so the reconnect under way starts it (within 1.5 s).
    pub fn start_service(&mut self) {
        let instance = match &self.target {
            Some(crate::conn::ConnTarget::Local(instance) | crate::conn::ConnTarget::Loopback(instance)) => instance.clone(),
            _ => return,
        };
        crate::conn::clear_stop_record(&instance);
        self.service_stopped = false;
        self.banner = Some("Offline — starting the service…".into());
        self.notify("Starting the terminal service…", false);
    }

    /// Switches to a re-established connection. The service starts this client
    /// from scratch, so subscriptions, sizes, tracking and routing are sent
    /// again (full frames follow). Keystrokes the service never confirmed are
    /// reported, never resent.
    pub fn on_reconnected(&mut self, result: Result<Conn, String>) {
        self.reconnecting = false;
        self.service_stopped = false;
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
        self.build_mismatch = conn.outdated();
        self.note_mixed_builds(conn.remote_build_differs().map(str::to_owned));
        self.new_service_rx = Some(conn.rx.clone());
        self.conn = conn;
        self.connected = true;
        self.connected_at = std::time::SystemTime::now();
        self.banner = None;
        self.reset_service_state();
        // The new connection opens the document again (local changes are kept).
        self.request_doc();
        // A restarted or upgraded service may have other facts (agents installed, home).
        self.refresh_backend_info();
        let repaired = std::mem::take(&mut self.resync.repairing);
        if lost > 0 {
            self.notify(format!("Reconnected. {lost} keystroke(s) were not delivered."), true);
        } else if repaired {
            self.notify("Reconnected to bring the terminal list back in step with the service.", false);
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
    pub(crate) fn set_origin(&mut self, x: f32, y: f32) {
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
        self.glide = None;
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
        self.glide = None;
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
        // End an in-flight pan or glide before changing its coordinate system.
        if matches!(self.drag, Some(Drag::Pan { .. })) {
            self.drag = None;
        }
        self.glide = None;
        self.pan_quiet_until = None;
        let (under, hidden) = self.conceal_maps();
        let nodes = self.doc.nodes.iter().filter(|n| !under.contains_key(&n.id) && !hidden.contains_key(&n.id)).map(|n| n.rect());
        let roots: HashSet<&String> = under.values().collect();
        let decks = roots.into_iter().filter_map(|root| {
            self.doc.node(root).map(|n| {
                let r = self.deck_rect_in(n, &under);
                crate::doc::Rect { x: r.x as i32, y: r.y as i32, w: r.w as i32 + 2, h: r.h as i32 + 1 }
            })
        });
        let bounds = nodes.chain(self.doc.groups.iter().map(|g| g.rect())).chain(decks).reduce(|a, b| a.union(&b));
        let Some(bounds) = bounds else {
            self.set_scale(1.0);
            self.view_frac = (0.0, 0.0);
            self.center_on(0, 0);
            return;
        };
        let c = self.layout.canvas;
        // Two columns and one row on each side (cells are about twice as tall).
        let (width, height) = ((c.width as f32 - 4.0).max(1.0), (c.height as f32 - 2.0).max(1.0));
        if self.render_mode == RenderMode::Gpu {
            // Everything in view, never above 100 %.
            let fit = (width / bounds.w.max(1) as f32).min(height / bounds.h.max(1) as f32);
            self.set_scale(fit.min(1.0));
        } else {
            self.doc.view.zoom = (ZOOMS.len() - 1) as u8;
            for (zoom, (n, d)) in ZOOMS.iter().enumerate() {
                if bounds.w as f32 * *n as f32 / *d as f32 <= width && bounds.h as f32 * *n as f32 / *d as f32 <= height {
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
    pub(crate) fn set_scale(&mut self, scale: f32) {
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
    pub(crate) fn zoom_to(&mut self, sx: f32, sy: f32, scale: f32) {
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

    /// Possibly running in another terminal (CLI or VS Code) rather than in
    /// this service: a hint (this client's copy can be stale); the service
    /// decides when it launches.
    pub fn live_elsewhere(&self, node: &Node) -> bool {
        node.kind == NodeKind::Session && !self.owned_alive(&node.id) && self.summary(node).is_some_and(|s| s.live)
    }

    /// Why the box's last start failed or was refused. A refusal because it
    /// ran elsewhere lasts while its history still says it is live there.
    pub fn node_error(&self, node: &Node) -> Option<&String> {
        self.node_errors.get(&node.id).filter(|error| !is_open_elsewhere(error) || self.owned_alive(&node.id) || self.summary(node).is_some_and(|s| s.live))
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
                // The service resumes it after a restart (until it has settled).
                if self.bus.restore.as_ref().is_some_and(|r| r.is_resuming(&node.id)) {
                    return Status::Resuming;
                }
                if let Some(error) = self.node_error(node)
                    && !self.owned_alive(&node.id)
                {
                    // Refused because it runs elsewhere: that, not a failure.
                    return if is_open_elsewhere(error) { Status::External } else { Status::Failed };
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
        if let Some(error) = self.node_error(node) {
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
            mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None, subagents_shown: None, unplaced: false, hidden: false, placement: None,
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
    /// session with a recurring schedule (`/every`) or Copilot's saved session
    /// schedule, or an enabled automation.
    pub fn node_loop(&self, node: &Node) -> Option<String> {
        match node.kind {
            NodeKind::Session if node.passive.is_some() => {
                let passive = node.passive.as_ref()?;
                let done = self.passive_view(node).is_some_and(|v| v.status == "done");
                // A canvas agent with no triggers never runs on its own: no loop.
                let wakes = passive.has_triggers() || !passive.is_canvas();
                (!passive.paused && !done && wakes).then(|| Self::passive_trigger(passive))
            }
            NodeKind::Session => {
                // A loop whose program went away does not run: drawn broken (`loop_interrupted`).
                if self.loop_interrupted(node).is_some() {
                    return None;
                }
                let live = self.owned_alive(&node.id) || self.summary(node).is_some_and(|s| s.live);
                if let Some(summary) = self.summary(node).filter(|s| live && s.loop_ms > 0) {
                    return Some(format!("every {}", short_interval(summary.loop_ms)));
                }
                // A saved session schedule fires only while its session's CLI
                // runs: a stopped session's never looks alive (after a restart too).
                if !live {
                    return None;
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

    /// "every 15m" when this session looped and was interrupted (a restart, a
    /// crash): its program is gone, so the loop no longer runs. Its history's
    /// schedule, else its saved session schedule.
    pub fn loop_interrupted(&self, node: &Node) -> Option<String> {
        if node.kind != NodeKind::Session || node.passive.is_some() || !matches!(self.node_status(node), Status::Interrupted | Status::Resuming) {
            return None;
        }
        if self.owned_alive(&node.id) || self.summary(node).is_some_and(|s| s.live) {
            return None;
        }
        if let Some(summary) = self.summary(node).filter(|s| s.loop_ms > 0) {
            return Some(format!("every {}", short_interval(summary.loop_ms)));
        }
        self.automation(&format!("session:{}", node.id)).filter(|a| a.enabled && repeats(&a.schedule)).map(|a| a.schedule.clone())
    }

    /// What the details panel says about an interrupted or resuming session.
    pub fn interruption_text(&self, node: &Node, status: Status) -> Option<String> {
        match status {
            Status::Resuming => Some("Atelier is resuming it after a restart of the service; a note asks it to restore what it was doing.".into()),
            Status::Interrupted => {
                let summary = self.summary(node);
                let mut was = Vec::new();
                if summary.is_some_and(|s| s.activity == crate::protocol::Activity::Working) {
                    was.push("working".to_owned());
                }
                if summary.is_some_and(|s| s.pending > 0) {
                    was.push("waiting for you".to_owned());
                }
                if let Some(every) = self.loop_interrupted(node) {
                    was.push(format!("looping {every}"));
                }
                let was = if was.is_empty() { String::new() } else { format!(" while {}", was.join(", ")) };
                let skipped = self.bus.restore.as_ref().and_then(|r| r.skipped(&node.id)).map(|why| format!(" Not resumed after the restart: {why}.")).unwrap_or_default();
                Some(format!("Interrupted{was}: its program went away (a restart of the service or the machine, or a crash).{skipped} Resume continues the conversation; tell it what to pick up again."))
            }
            _ => None,
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
        self.tile_slots = self.slots_showing(&self.tile_slots, id);
        self.mark_dirty();
    }

    /// The tile slots once `id` shows: kept where it is, else in a free
    /// slot, else instead of the least recently shown one.
    pub fn slots_showing(&self, slots: &[String], id: &str) -> Vec<String> {
        let mut slots = slots.to_vec();
        if !slots.iter().any(|t| t == id) {
            if slots.len() < 4 {
                slots.push(id.to_owned());
            } else {
                let rank = |t: &String| self.doc.tiles.iter().position(|x| x == t).unwrap_or(usize::MAX);
                if let Some((slot, _)) = slots.iter().enumerate().max_by_key(|(_, t)| rank(t)) {
                    slots[slot] = id.to_owned();
                }
            }
        }
        slots
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
            // Another space shows its own panel; a terminal shows there only
            // while you are in it (a session started from that space).
            if self.space != crate::spaces::Space::Canvas {
                return self.focused_term.clone().filter(|id| self.focus == Focus::Terminal && self.terms.contains_key(id)).into_iter().collect();
            }
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

    // ---------- launching ----------

    /// Starts a session box's terminal (see `interact.rs` for what may call it).
    pub fn launch(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        self.closing_retry.remove(id);
        self.closing_retried.remove(id);
        let cwd = node
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| self.summary(&node).map(|s| s.cwd.clone()).filter(|c| !c.is_empty()))
            .unwrap_or_else(|| self.default_cwd.clone());
        // The size of the tile it gets (a size sent for an earlier run is sent again).
        let (cols, rows) = self.launch_size(id);
        self.forget_size(id);
        self.node_errors.remove(id);
        // The folder is on the service's machine; when it cannot be asked, the
        // launch itself reports a missing folder. A remote service is not
        // asked from here: a round trip on the UI thread to a busy service
        // froze the window (its launch answers the same).
        if !self.conn.remote && !self.service_is_remote() && self.backend.path_info(&cwd).is_ok_and(|info| !info.dir) {
            self.node_errors.insert(id.to_owned(), "folder missing".into());
            self.notify(format!("Cannot open: folder not found: {cwd}"), true);
            return;
        }
        let mut record_model = None;
        // What only this start uses, given back if the service refuses it (R3-C-04).
        let mut once = LaunchOnce { fresh: node.fresh, ..LaunchOnce::default() };
        let kind = match &node.command {
            Some(command) if !command.is_empty() => LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() },
            _ => {
                let group = self.group_for_session(id);
                let prompt = self.pending_prompts.remove(id);
                // A native fork starts once from its parent; later launches resume it.
                let fork_from = self.pending_forks.remove(id).filter(|_| node.fresh);
                (once.prompt, once.fork) = (prompt.clone(), fork_from.clone());
                let launch = crate::side::agent_launch(&node, &self.doc.defaults, group.as_ref(), prompt, fork_from, !node.fresh);
                // A canvas-wide agent's chat may read the canvas's goals and skills, as its runs do.
                let mut launch = launch;
                if node.is_canvas_agent()
                    && let Some(dir) = self.canvas_agents_path()
                {
                    launch.add_dirs.push(dir);
                }
                // A new conversation records the model it starts with (resumes keep it).
                if !launch.resume && launch.fork_from.is_none() && node.model.is_none() {
                    record_model = Some(launch.model.clone().unwrap_or_default());
                }
                LaunchKind::Agent(Box::new(launch))
            }
        };
        self.send(ClientMsg::Launch { id: id.to_owned(), kind, cwd: cwd.clone(), cols, rows });
        self.launching.insert(id.to_owned(), Instant::now());
        if once.prompt.is_some() || once.fork.is_some() || once.fresh {
            self.launch_once.insert(id.to_owned(), once);
        } else {
            self.launch_once.remove(id);
        }
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
        let drawn = !self.needs_draw;
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
                // Terminals that were running here and ended (or are gone from a
                // complete list): their sessions' `live` is doubted.
                let mut ended: Vec<String> = Vec::new();
                // After a reconnect the first list is complete: forget the rest.
                let whole = std::mem::take(&mut self.resync_terms);
                if whole {
                    let known: HashSet<&str> = infos.iter().map(|i| i.id.as_str()).collect();
                    ended.extend(self.terms.values().filter(|t| t.alive && !known.contains(t.id.as_str())).map(|t| t.id.clone()));
                    self.terms.retain(|id, _| known.contains(id.as_str()));
                    self.views.retain(|id, _| known.contains(id.as_str()));
                }
                for info in infos {
                    // The state of an earlier run, overtaken by the newer one held
                    // here, is never taken back (R2-A-08). A complete list is the
                    // service's present (a restarted service's clock may differ).
                    if !whole && self.terms.get(&info.id).is_some_and(|held| info.started_ms < held.started_ms) {
                        continue;
                    }
                    // Bells and notifications from a session ring the outer terminal
                    // (at most once a second).
                    if self.terms.get(&info.id).is_some_and(|old| info.bells > old.bells)
                        && self.last_bell.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                    {
                        self.last_bell = Some(Instant::now());
                        self.platform.bell(&mut self.raw_out);
                    }
                    if info.alive {
                        let launched_here = self.launching.remove(&info.id).is_some();
                        // It started: what its start used is spent.
                        self.launch_once.remove(&info.id);
                        // Started again (here, by another client or the restore):
                        // its size is sent again, whatever an earlier run had. It
                        // is claimed only when this window started it: one the
                        // phone or the restore started stays theirs (R2-D-04).
                        if !self.owned_alive(&info.id) {
                            let claimed = self.last_claimed.clone();
                            self.forget_size(&info.id);
                            if !launched_here {
                                self.last_claimed = claimed;
                            }
                            if !self.live_doubted.is_empty() {
                                self.live_doubted.retain(|_, d| d.box_id != info.id);
                            }
                        }
                    } else if self.launching.remove(&info.id).is_some() && info.exit_code != Some(0) {
                        self.notify(format!("Terminal exited immediately (code {:?}).", info.exit_code), true);
                    }
                    if !info.alive && self.owned_alive(&info.id) {
                        ended.push(info.id.clone());
                    }
                    self.terms.insert(info.id.clone(), info);
                }
                for id in ended {
                    self.doubt_live(&id);
                }
                if let Some(held) = self.held_removals.as_mut() {
                    held.terms_in = true;
                }
                self.check_restore_cancels(false);
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
                    // News of a doubted session: believed again (the same summary sent
                    // again, as a reconnect's whole list does, is no news).
                    let mut summary = summary;
                    if let Some(doubt) = self.live_doubted.get(&summary.id) {
                        if doubt.summary != summary {
                            self.live_doubted.remove(&summary.id);
                        } else if doubt.since.elapsed() >= LIVE_DOUBT {
                            summary.live = false;
                        }
                    }
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
            ServiceMsg::Error { id, message } => match id {
                // Only a launch's errors name a terminal.
                Some(id) => self.on_launch_error(&id, &message),
                None => {
                    // A todo change of this window refused (the book as it is follows): it is undone now.
                    if message.starts_with("Todo change ") {
                        crate::tasks::refused(self);
                    }
                    self.notify(message, true);
                }
            },
            ServiceMsg::Stats(stats) => {
                // Every 3 s: the screen changes only when the status bar's figures
                // (or the terminal list) do, not with every answer (S6-11).
                let shown = |app: &App| (app.stats.as_ref().map(|s| (s.private_bytes >> 20, s.alive)), app.terms_rev);
                let before = shown(self);
                self.on_stats(stats);
                if drawn && shown(self) == before {
                    self.needs_draw = false;
                }
            }
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
            | ServiceMsg::TunnelAccess { .. }
            // Only for VM sessions' satellites (CAP_SATELLITE).
            | ServiceMsg::Sat(_)
            // Read by the connection itself (flow control, CAP_ACK).
            | ServiceMsg::Caps { .. } => {}
        }
    }

    /// The service did not launch terminal `id` and says why: the box shows
    /// that reason (a session open elsewhere shows as such, anything else as
    /// failed) and the notice names the box.
    pub fn on_launch_error(&mut self, id: &str, message: &str) {
        self.launching.remove(id);
        // A resumed child that may not start goes again (with its own notice).
        if self.launch_refused(id, message) {
            return;
        }
        // Not started: what its start used goes with the next one (a chat
        // message, a native fork, a new conversation), never lost (R3-C-04).
        let kept = self.give_back_launch(id);
        // Its own previous run still closing (Enter right after it exited):
        // started again once in a moment, preparing meanwhile, never shown as
        // open elsewhere (R2-D-10).
        if is_own_closing(message) && !self.closing_retried.contains(id) && self.doc.node(id).is_some() {
            self.closing_retry.insert(id.to_owned(), Instant::now() + CLOSING_RETRY);
            self.launching.insert(id.to_owned(), Instant::now());
            let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
            self.notify(format!("“{name}”: its previous run is still closing; starting it again in a moment."), false);
            return;
        }
        let reason = launch_reason(message);
        // The service says the session runs elsewhere: a doubt of its `live`
        // (a terminal that ended here) was wrong, so ↗ and the holder show (R3-C-08).
        if is_open_elsewhere(message) {
            self.believe_live(id);
        }
        self.node_errors.insert(id.to_owned(), reason.clone());
        let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_owned());
        let kept = if kept { " (its message is kept: start it again to send it)" } else { "" };
        self.notify(format!("“{name}” was not started: {reason}{kept}"), true);
    }

    /// A start the service refused: what it used only once (its first
    /// prompt, its native fork, `fresh`) is put back for the next start.
    /// True when a message waits again.
    fn give_back_launch(&mut self, id: &str) -> bool {
        let Some(once) = self.launch_once.remove(id) else { return false };
        if let Some(fork) = once.fork {
            self.pending_forks.entry(id.to_owned()).or_insert(fork);
        }
        if once.fresh
            && let Some(node) = self.doc.node_mut(id)
            && !node.fresh
        {
            node.fresh = true;
            self.mark_dirty();
        }
        match once.prompt {
            Some(prompt) => {
                self.pending_prompts.entry(id.to_owned()).or_insert(prompt);
                true
            }
            None => false,
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

    /// Adds boxes for subagents discovered in history, under their parent: a
    /// slot of their own when the session shows its subagent boxes (its
    /// group grows to hold it), else folded into its deck without a place of
    /// their own (`Node::unplaced`; no room taken, no group grown).
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
        let mut index = None;
        for (parent, agents) in missing {
            let folded = canvas_model::visual::subagents_folded(&self.doc, &parent);
            for (key, name) in agents {
                let id = format!("{}:{key}", parent.id);
                let (x, y) = match folded {
                    // On its session until it is first shown.
                    true => (parent.x + 3, parent.y),
                    false => self.doc.agent_slot_in(&parent, index.get_or_insert_with(|| crate::doc::SpatialIndex::new(&self.doc))),
                };
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
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None, subagents_shown: None, unplaced: folded, hidden: false, placement: None,
                };
                let rect = node.rect();
                self.doc.nodes.push(node);
                if !folded {
                    if let Some(index) = index.as_mut() {
                        index.insert(rect);
                    }
                    self.grow_group_for(&parent, rect);
                }
                added = true;
            }
        }
        if self.place_shown_subagents() || added {
            self.mark_dirty();
        }
    }

    /// Keeps a shown subagent box inside its session's group, unless growing
    /// the group would cover another group (membership is geometric).
    fn grow_group_for(&mut self, parent: &Node, rect: crate::doc::Rect) {
        let Some(gi) = self.doc.group_of(parent).and_then(|g| self.doc.groups.iter().position(|candidate| candidate.id == g.id)) else { return };
        let current = self.doc.groups[gi].rect();
        let grown = current.union(&crate::doc::Rect { x: rect.x - 2, y: rect.y - 1, w: rect.w + 4, h: rect.h + 2 });
        let covers = self.doc.groups.iter().enumerate().any(|(i, g)| i != gi && !current.contains_rect(&g.rect()) && g.rect().intersects(&grown));
        if !covers {
            let group = &mut self.doc.groups[gi];
            (group.x, group.y, group.w, group.h) = (grown.x, grown.y, grown.w, grown.h);
        }
    }

    /// Gives shown subagent boxes that never had a place of their own (they
    /// arrived while folded) a slot under their session (`agent_slot_in`)
    /// and grows its group for them, as for new boxes. True when any moved.
    pub fn place_shown_subagents(&mut self) -> bool {
        if !self.doc.nodes.iter().any(|n| n.unplaced) {
            return false;
        }
        let sessions: HashMap<&str, &Node> = self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).map(|n| (n.id.as_str(), n)).collect();
        // A box in the hidden area gets its slot when it is shown.
        let hidden = canvas_model::visual::hidden_area(&self.doc);
        let todo: Vec<(String, String)> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Agent && n.unplaced && !hidden.contains_key(&n.id))
            .filter_map(|n| n.parent.as_deref().and_then(|p| sessions.get(p)).filter(|p| !canvas_model::visual::subagents_folded(&self.doc, p)).map(|p| (n.id.clone(), p.id.clone())))
            .collect();
        if todo.is_empty() {
            return false;
        }
        let mut index = crate::doc::SpatialIndex::new(&self.doc);
        for (id, parent) in todo {
            let Some(parent) = self.doc.node(&parent).cloned() else { continue };
            let (x, y) = self.doc.agent_slot_in(&parent, &index);
            let Some(node) = self.doc.node_mut(&id) else { continue };
            (node.x, node.y, node.unplaced) = (x, y, false);
            let rect = node.rect();
            index.insert(rect);
            self.grow_group_for(&parent, rect);
        }
        true
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
                    synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: seed.agent, agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None, subagents_shown: None, unplaced: false, hidden: false, placement: None,
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
            // A VM box is the service's to suspend (stopping its program would leave its VM running).
            if node.placement.as_ref().is_some_and(|p| p.is_vm()) {
                continue;
            }
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
        if (visible != self.last_subscribed || focused != self.last_focused_sub) && self.queue(ClientMsg::Subscribe8 { ids: visible.clone(), focused: focused.clone() }) {
            // Images of terminals no longer shown go (up to 64 MB each decoded);
            // the service sends them again with the next subscription.
            for id in self.last_subscribed.iter().filter(|id| !visible.contains(id)) {
                if let Some(view) = self.views.get_mut(id) {
                    view.images.clear();
                    view.placements.clear();
                }
            }
            self.last_subscribed = visible;
            self.last_focused_sub = focused;
        }
        // The service forgets this client's size for terminals it stopped
        // showing (sizes follow the client typing in a terminal: the iPhone
        // may take one over); showing one again sends its size again.
        self.sync_sizes(Instant::now());
        // History is indexed by the agents' own session ids.
        let mut tracked: Vec<String> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Session && n.command.is_none())
            .map(|n| native_id(n).to_owned())
            .collect();
        tracked.sort();
        if tracked != self.last_tracked && self.queue(ClientMsg::Track { ids: tracked.clone() }) {
            self.last_tracked = tracked;
        }
    }

    /// Notes whether bytes came in since the last look (any change counts: a
    /// new connection counts from 0).
    fn note_heard(&mut self) {
        let bytes = self.conn.wire.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
        if bytes != self.heard.0 {
            self.heard = (bytes, Instant::now());
        }
    }

    /// Nothing came in for most of the dead-after time (heartbeats come every
    /// 2 s): the connection went silent rather than being closed. Any byte
    /// read counts, a part of a large message too (R3-C-07).
    pub fn went_silent(&mut self) -> bool {
        self.note_heard();
        let quiet = self.heard.1.elapsed();
        let quiet = self.conn.liveness.as_ref().map_or(quiet, |l| l.silence().min(quiet));
        quiet >= self.conn_options.dead_after * 2 / 3
    }

    /// Box `id`'s terminal ended here: if its session's summary says it is
    /// live, that is doubted from now on ([`LIVE_DOUBT`]).
    fn doubt_live(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).filter(|n| n.kind == NodeKind::Session) else { return };
        let Some(summary) = self.sessions.get(native_id(node)).filter(|s| s.live) else { return };
        let doubt = LiveDoubt { box_id: id.to_owned(), since: Instant::now(), summary: summary.clone() };
        self.live_doubted.insert(summary.id.clone(), doubt);
    }

    /// Box `id`'s session is live elsewhere after all (the service refused
    /// its start for that): no doubt, and its summary says live again.
    fn believe_live(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).filter(|n| n.kind == NodeKind::Session) else { return };
        let session = native_id(node).to_owned();
        self.live_doubted.remove(&session);
        self.live_doubted.retain(|_, d| d.box_id != id);
        if let Some(summary) = self.sessions.get_mut(&session).filter(|s| !s.live) {
            summary.live = true;
            self.sessions_rev += 1;
            self.needs_draw = true;
        }
    }

    /// Doubted sessions still live and unchanged after [`LIVE_DOUBT`] are
    /// taken as not live here (shown stopped or interrupted, not open
    /// elsewhere); news of them, or their box running again, ends the doubt.
    fn settle_doubts(&mut self) {
        for (id, doubt) in &self.live_doubted {
            if doubt.since.elapsed() >= LIVE_DOUBT
                && !self.terms.get(&doubt.box_id).is_some_and(|t| t.alive)
                && let Some(summary) = self.sessions.get_mut(id).filter(|s| s.live)
            {
                summary.live = false;
                self.sessions_rev += 1;
                self.needs_draw = true;
            }
        }
    }

    /// Launches refused while the box's own previous run was closing go
    /// again once their moment came (tiles and the keyboard stay where they are).
    fn retry_closing(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self.closing_retry.iter().filter(|(_, at)| now >= **at).map(|(id, _)| id.clone()).collect();
        for id in due {
            self.closing_retry.remove(&id);
            self.launching.remove(&id);
            let Some(name) = self.doc.node(&id).map(|n| n.name.clone()) else {
                self.pending_prompts.remove(&id);
                continue;
            };
            // Started meanwhile (another window, the phone): its message cannot go as a first prompt.
            if self.owned_alive(&id) {
                if self.pending_prompts.remove(&id).is_some() {
                    self.notify(format!("“{name}” was started elsewhere meanwhile: its message was not sent; type it in its terminal."), true);
                }
                continue;
            }
            // Offline when its moment came: said, and its message kept for Enter.
            if !self.connected {
                let kept = if self.pending_prompts.contains_key(&id) { " (its message is kept: start it again to send it)" } else { "" };
                self.notify(format!("“{name}” was not started: offline{kept}"), true);
                continue;
            }
            let (slots, focused, focus) = (self.tile_slots.clone(), self.focused_term.clone(), self.focus);
            self.launch(&id);
            (self.tile_slots, self.focused_term, self.focus) = (slots, focused, focus);
            self.closing_retried.insert(id);
        }
    }

    pub fn on_tick(&mut self) {
        // Spinner frame: one step per 80 ms regardless of the housekeeping rate.
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let frame = self.frozen_tick.unwrap_or_else(|| (START.get_or_init(Instant::now).elapsed().as_millis() / 80) as usize);
        let spun = frame != self.tick;
        self.tick = frame;
        self.note_heard();
        if !self.live_doubted.is_empty() {
            self.settle_doubts();
        }
        if !self.closing_retry.is_empty() {
            self.retry_closing();
        }
        if matches!(self.info_check, InfoCheck::Retry { at, .. } if Instant::now() >= at) && self.connected {
            self.refresh_backend_info();
        }
        self.check_restore_cancels(false);
        // Removals the last run kept: once this connection's terminal list is in.
        if self.held_removals.as_ref().is_some_and(|h| h.terms_in) && self.connected && !self.reconnecting && !self.doc_resync {
            self.make_held_removals();
        }
        if self.dirty_at.is_some_and(|t| t.elapsed() >= DOC_FLUSH) {
            self.flush_doc();
        }
        if self.doc_resync && self.connected && self.doc_asked_at.elapsed() >= self.doc_retry_wait() {
            self.doc_retries += 1;
            self.request_doc();
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
        // Tile sizes that settled and sizes due a repair, also without a draw.
        self.sync_sizes(Instant::now());
        if self.stats_at.elapsed() >= Duration::from_secs(3) {
            self.stats_at = Instant::now();
            self.send(ClientMsg::Stats);
        }
        self.tick_resync();
        if self.pan_quiet_until.is_some_and(|t| Instant::now() >= t) {
            self.pan_quiet_until = None;
            self.needs_draw = true;
        }
        self.step_glide();
        self.baseline_reviewed();
        if self.routing_check_at.elapsed() >= Duration::from_secs(1) {
            self.sync_routing();
        }
        // A canvas agent's chat asked for during a run: once the run's terminal is gone.
        self.open_waiting_chats();
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
        // The Notebook and Tasks spaces (autosave, polling), whichever shows.
        crate::notebook::tick(self);
        crate::tasks::tick(self);
        // A burst of arrow-key moves ends a second after the last one (one undo step).
        self.tick_undo();
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

/// A launch error from the service as the box shows it: one line, bounded.
pub fn launch_reason(message: &str) -> String {
    let line = message.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
    let line = if line.is_empty() { "the service gave no reason".to_owned() } else { line };
    crate::paint::truncate(&line, 240)
}

/// The service refused a launch because the session already runs (another
/// terminal, VS Code, another box): "open elsewhere", not a failure. The
/// box's own previous run still closing is not elsewhere.
pub fn is_open_elsewhere(reason: &str) -> bool {
    let reason = reason.to_lowercase();
    !is_own_closing(&reason) && ["already running", "running elsewhere", "open elsewhere", "open in another", "running in another", "running inside"].iter().any(|p| reason.contains(p))
}

/// The service refused a launch because the box's own previous run is still
/// closing: "still closing in this box's own terminal …" (this build), "already
/// running in this box's own terminal, which is still closing …" (a4bf57f).
pub fn is_own_closing(reason: &str) -> bool {
    reason.to_lowercase().contains("this box's own terminal")
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

/// What the mixed-builds notice says (the toolbar's ⚠ Mixed builds repeats it on a click).
pub fn mixed_builds_notice(client: &str, service: &str) -> String {
    format!(
        "Mixed builds are unsupported: this client is {client} and the service is {service}. Install the same build on both machines (update this client, or update the service's machine and run `atelier service restart` there)."
    )
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
    fn a_remote_service_of_another_build_is_called_out_once_and_cleared_when_builds_match() {
        let (mut conn, _sent) = detached();
        (conn.remote, conn.service_build) = (true, "0.0.0-elsewhere".into());
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        assert_eq!(app.mixed_builds.as_deref(), Some("0.0.0-elsewhere"));
        let notice = app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default();
        assert!(notice.contains("Mixed builds are unsupported") && notice.contains("0.0.0-elsewhere") && notice.contains(crate::paths::build_id()), "{notice}");
        // The same service again: no second notice.
        app.notice = None;
        let (mut conn, _sent) = detached();
        (conn.remote, conn.service_build) = (true, "0.0.0-elsewhere".into());
        app.on_reconnected(Ok(conn));
        assert!(app.notice.as_ref().is_none_or(|n| !n.text.contains("Mixed builds")), "said once per service build");
        // A service of this client's build: nothing to say.
        let (mut conn, _sent) = detached();
        conn.remote = true;
        app.on_reconnected(Ok(conn));
        assert_eq!(app.mixed_builds, None);
        // A local service is replaced instead (⟳ Update service): never called mixed.
        let (mut conn, _sent) = detached();
        conn.service_build = "0.0.0-elsewhere".into();
        app.on_reconnected(Ok(conn));
        assert_eq!(app.mixed_builds, None);
        assert!(app.build_mismatch);
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

    #[test]
    fn live_left_over_from_a_terminal_that_ended_here_is_doubted_until_news_comes() {
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: vec![node("s1"), node("s2")], ..Doc::default() };
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        let live = |id: &str| SessionSummary { id: id.into(), live: true, tracked: true, updated_ms: 1, ..Default::default() };
        app.on_service(ServiceMsg::Sessions { full: true, upsert: vec![live("s1"), live("s2")], remove: Vec::new() });
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "s1".into(), alive: true, started_ms: 1, ..Default::default() }]));
        // Its terminal ends here; the summary saying it is no longer live never comes (an older host lost it).
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "s1".into(), alive: false, exit_code: Some(0), started_ms: 1, ..Default::default() }]));
        let elsewhere = |app: &App, id: &str| app.live_elsewhere(app.doc.node(id).unwrap());
        app.on_tick();
        assert!(elsewhere(&app, "s1"), "not at once: the index may not have looked yet");
        assert!(elsewhere(&app, "s2"), "a session that never ran here is not doubted");
        app.live_doubted.get_mut("s1").unwrap().since -= LIVE_DOUBT;
        app.on_tick();
        assert!(!elsewhere(&app, "s1") && app.node_status(app.doc.node("s1").unwrap()) == Status::Stopped);
        // The service refuses its start: really open elsewhere, so shown so, with the holder (R3-C-08).
        app.on_service(ServiceMsg::Error { id: Some("s1".into()), message: "this session is already running in VS Code (Code.exe pid 9): close it there first".into() });
        assert!(elsewhere(&app, "s1") && app.live_doubted.is_empty());
        assert_eq!(app.node_status(app.doc.node("s1").unwrap()), Status::External);
        assert!(app.subtitle(app.doc.node("s1").unwrap()).contains("VS Code"), "{}", app.subtitle(app.doc.node("s1").unwrap()));
        // Doubted again for the rest of this test.
        app.doubt_live("s1");
        app.live_doubted.get_mut("s1").unwrap().since -= LIVE_DOUBT;
        app.on_tick();
        assert!(!elsewhere(&app, "s1"));
        // The same summary again (a reconnect's whole list) is no news …
        app.on_service(ServiceMsg::Sessions { full: true, upsert: vec![live("s1"), live("s2")], remove: Vec::new() });
        assert!(!elsewhere(&app, "s1"));
        // … a changed one is believed (opened elsewhere since).
        app.on_service(ServiceMsg::Sessions { full: false, upsert: vec![SessionSummary { updated_ms: 2, ..live("s1") }], remove: Vec::new() });
        assert!(elsewhere(&app, "s1") && app.live_doubted.is_empty());
        // A terminal gone from the list after a reconnect (a restarted service) ended too.
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "s2".into(), alive: true, started_ms: 1, ..Default::default() }]));
        app.reset_service_state();
        app.on_service(ServiceMsg::Terminals(Vec::new()));
        assert_eq!(app.node_status(app.doc.node("s2").unwrap()), Status::External, "live, not running here");
        app.live_doubted.get_mut("s2").unwrap().since -= LIVE_DOUBT;
        app.on_tick();
        assert_eq!(app.node_status(app.doc.node("s2").unwrap()), Status::Stopped);
        // Running again here: no doubt left.
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "s2".into(), alive: true, started_ms: 2, ..Default::default() }]));
        assert!(app.live_doubted.is_empty());
    }

    #[test]
    fn changes_made_offline_survive_a_quit_and_go_at_the_next_start() {
        let home = std::env::temp_dir().join(format!("cc-unsent-{}", uuid::Uuid::new_v4()));
        let prefs = crate::prefs::PrefsFile::path(&home, "default", "k");
        let opened = || Opened { key: "k".into(), version: 3, path: String::new(), client: 7, doc: Doc { nodes: vec![node("a")], ..Doc::default() } };
        let start = || {
            let (conn, sent) = detached();
            let (app_tx, _app_rx) = unbounded();
            (App::new(conn, Arc::new(crate::rpc::Offline), opened(), false, ".".into(), app_tx, Some(prefs.clone())), sent)
        };
        let (mut app, sent) = start();
        // Offline: the move cannot go; quitting keeps it.
        app.connected = false;
        app.doc.node_mut("a").unwrap().x = 12;
        app.mark_dirty();
        app.save_now();
        assert!(edits(&sent).is_empty());
        drop(app);
        // The next start puts it back and sends it; then it is forgotten.
        let (mut app, sent) = start();
        assert_eq!(app.doc.node("a").unwrap().x, 12);
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("1 canvas change")));
        app.flush_doc();
        assert!(matches!(edits(&sent).as_slice(), [(3, ops)] if ops.len() == 1));
        drop(app);
        let (app, _sent) = start();
        assert_eq!(app.doc.node("a").unwrap().x, 0, "sent once, not again");
        // Connected with everything sent: nothing is kept.
        let mut app = app;
        app.save_now();
        assert!(crate::prefs::PrefsFile::new(prefs.clone()).load_unsent().is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_service_stopped_on_purpose_shows_stopped_with_a_start_action() {
        let home = std::env::temp_dir().join(format!("cc-stopped-{}", uuid::Uuid::new_v4()));
        let instance = crate::paths::Instance::new(Some("t".into()), Some(home.clone()), Some(home.clone())).unwrap();
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        app.target = Some(crate::conn::ConnTarget::Local(instance.clone()));
        std::fs::create_dir_all(home.join("run")).unwrap();
        std::fs::write(crate::conn::stop_record_path(&instance), b"{}").unwrap();
        // The reconnect found the record (see conn's test): the banner and the toolbar say so.
        app.connected = false;
        app.reconnecting = true;
        app.offline_banner(true, None);
        assert_eq!(app.banner.as_deref(), Some("Service stopped"));
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("▶ Start service")));
        let area = ratatui::layout::Rect::new(0, 0, 160, 40);
        crate::ui::render(&mut app, &mut ratatui::buffer::Buffer::empty(area), area, None);
        assert!(app.layout.hits.iter().any(|(_, hit)| *hit == Hit::Toolbar(crate::dialogs::Action::StartService)));
        // ▶ Start service: the record goes, so the reconnect under way starts it.
        app.run(crate::dialogs::Action::StartService);
        assert!(!app.service_stopped && !crate::conn::stop_record_path(&instance).exists());
        assert_eq!(app.banner.as_deref(), Some("Offline — starting the service…"));
        crate::ui::render(&mut app, &mut ratatui::buffer::Buffer::empty(area), area, None);
        assert!(!app.layout.hits.iter().any(|(_, hit)| *hit == Hit::Toolbar(crate::dialogs::Action::StartService)));
        // Any other failure keeps the usual banner.
        app.offline_banner(false, Some("the service refused this client"));
        assert_eq!(app.banner.as_deref(), Some("Offline — reconnecting… (the service refused this client)"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn kept_changes_are_checked_against_the_canvas_at_the_next_start_and_their_file_goes() {
        let home = std::env::temp_dir().join(format!("cc-unsent-{}", uuid::Uuid::new_v4()));
        let prefs = crate::prefs::PrefsFile::path(&home, "default", "k");
        let file = || crate::prefs::PrefsFile::new(prefs.clone());
        let start = |doc: Doc| {
            let (conn, sent) = detached();
            let (app_tx, _app_rx) = unbounded();
            let opened = Opened { key: "k".into(), version: 3, path: String::new(), client: 7, doc };
            (App::new(conn, Arc::new(crate::rpc::Offline), opened, false, ".".into(), app_tx, Some(prefs.clone())), sent)
        };
        let base = Doc { nodes: vec![node("a"), node("b"), node("c")], ..Doc::default() };
        // Offline: a renamed, b and c (stopped here) removed; then Atelier quits.
        let (mut app, _sent) = start(base.clone());
        app.connected = false;
        app.doc.node_mut("a").unwrap().name = "old".into();
        app.remove_nodes(&["b".into(), "c".into()]);
        app.save_now();
        drop(app);
        // Since then a was renamed elsewhere, and the phone resumed b.
        let mut now = base.clone();
        now.node_mut("a").unwrap().name = "new".into();
        let (mut app, sent) = start(now);
        assert_eq!(app.doc.node("a").unwrap().name, "new", "a newer rename is not overwritten");
        let notice = app.notice.clone().unwrap();
        assert!(notice.error && notice.text.contains("1 not applied, changed elsewhere since: “new”"), "{}", notice.text);
        assert!(file().load_unsent().is_none(), "handled: the file goes at once");
        // The removals wait for the terminal list: a box that runs again stays.
        assert!(app.doc.node("b").is_some() && app.doc.node("c").is_some());
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "b".into(), alive: true, ..Default::default() }]));
        app.on_tick();
        assert!(app.doc.node("b").is_some() && app.doc.node("c").is_none());
        let forgotten: Vec<String> = sent.try_iter().filter_map(|m| if let ClientMsg::Forget { id } = m { Some(id) } else { None }).collect();
        assert_eq!(forgotten, ["c"]);
        let notice = app.notice.clone().unwrap();
        assert!(notice.text.contains("1 box removal(s) kept from the last run were made; 1 not, the box runs again or changed since: “b”"), "{}", notice.text);
        assert!(app.held_removals.is_none());
        drop(app);

        // A change the service had already (sent, not confirmed): nothing to
        // do, no notice, and the file goes all the same.
        let (mut app, _sent) = start(base.clone());
        app.doc.node_mut("a").unwrap().x = 5;
        app.flush_doc();
        app.connected = false;
        app.save_now();
        assert!(file().load_unsent().is_some_and(|u| u.kept.len() == 1 && u.kept[0].sent));
        drop(app);
        let mut now = base.clone();
        now.node_mut("a").unwrap().x = 5;
        let (mut app, sent) = start(now);
        assert!(app.notice.as_ref().is_none_or(|n| !n.text.contains("change")) && file().load_unsent().is_none());
        app.flush_doc();
        assert!(edits(&sent).is_empty(), "nothing to send");
        drop(app);

        // Kept more than a day ago: dropped, and said so.
        let old = crate::sync::Kept { op: crate::sync::Op::SetField("idle_stop_minutes".into(), serde_json::json!(7)), base: None, sent: false };
        let saved_ms = now_ms() - crate::prefs::UNSENT_MAX_AGE_MS - 3_600_000;
        file().save_unsent(&crate::prefs::Unsent { key: "k".into(), saved_ms, kept: vec![old], ops: Vec::new() }).unwrap();
        let (app, _sent) = start(base.clone());
        assert_ne!(app.doc.idle_stop_minutes, 7);
        let notice = app.notice.clone().unwrap();
        assert!(notice.error && notice.text.contains("1 canvas change(s) kept 1d ago were dropped (older than a day)"), "{}", notice.text);
        assert!(file().load_unsent().is_none());
        drop(app);

        // Removals still waiting when Atelier quits again are kept again, with their age.
        let (mut app, _sent) = start(base.clone());
        app.connected = false;
        app.remove_nodes(&["c".into()]);
        app.save_now();
        drop(app);
        let (mut app, _sent) = start(base.clone());
        let saved = app.held_removals.as_ref().map(|h| h.saved_ms).unwrap();
        app.save_now();
        assert!(file().load_unsent().is_some_and(|u| u.saved_ms == saved && u.kept.len() == 1));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_backends_facts_are_asked_again_until_they_come_and_after_a_reconnect() {
        use crate::rpc::{Request, Response};
        /// Answers `Info` from the second time on.
        struct Late(std::sync::Mutex<u32>);
        impl Backend for Late {
            fn call(&self, request: Request) -> anyhow::Result<Response> {
                let Request::Info = request else { return Err(anyhow::anyhow!("no {}", request.name())) };
                let mut calls = self.0.lock().unwrap();
                *calls += 1;
                match *calls {
                    1 => Err(anyhow::anyhow!("busy")),
                    n => Ok(Response::Info(BackendInfo { home: format!("/h{n}"), ..Default::default() })),
                }
            }
        }
        let backend = Arc::new(Late(std::sync::Mutex::new(0)));
        let (conn, _sent) = detached();
        let (app_tx, events) = unbounded();
        let mut app = App::new(conn, backend.clone(), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("did not answer")));
        let InfoCheck::Retry { at, tries: 1 } = app.info_check else { panic!("{:?}", app.info_check) };
        assert!(at > Instant::now() + INFO_RETRY - Duration::from_secs(1));
        app.on_tick();
        assert_eq!(*backend.0.lock().unwrap(), 1, "not before its time");
        app.info_check = InfoCheck::Retry { at: Instant::now(), tries: 1 };
        app.on_tick();
        let event = events.recv_timeout(Duration::from_secs(5)).expect("asked again");
        app.on_app_event(event);
        assert_eq!((app.info.home.as_str(), app.info_check), ("/h2", InfoCheck::Idle));
        // A reconnect (a restarted service) asks once more.
        let (conn, _sent) = detached();
        app.on_reconnected(Ok(conn));
        app.on_reconnected(Err("again".into()));
        let event = events.recv_timeout(Duration::from_secs(5)).expect("asked after the reconnect");
        app.on_app_event(event);
        assert_eq!(app.info.home, "/h3");
        assert_eq!(*backend.0.lock().unwrap(), 3);
    }

    #[test]
    fn the_backends_facts_come_from_the_last_run_and_are_asked_again_in_the_background() {
        use crate::rpc::{Request, Response};
        struct Facts(std::sync::Mutex<(BackendInfo, u32)>);
        impl Backend for Facts {
            fn call(&self, request: Request) -> anyhow::Result<Response> {
                let mut facts = self.0.lock().unwrap();
                match request {
                    Request::Info => {
                        facts.1 += 1;
                        Ok(Response::Info(facts.0.clone()))
                    }
                    other => Err(anyhow::anyhow!("no {}", other.name())),
                }
            }
        }
        let home = std::env::temp_dir().join(format!("cc-info-{}", uuid::Uuid::new_v4()));
        let prefs = crate::prefs::PrefsFile::path(&home, "default", "k");
        let first = BackendInfo { home: "/h".into(), installed: vec![AgentKind::Copilot], ..Default::default() };
        let backend = Arc::new(Facts(std::sync::Mutex::new((first.clone(), 0))));
        let start = || {
            let (conn, _sent) = detached();
            let (app_tx, app_rx) = unbounded();
            let app = App::new(conn, backend.clone(), Opened::detached(Doc::default()), false, ".".into(), app_tx, Some(prefs.clone()));
            (app, app_rx)
        };
        // The first start asks at once and keeps the answer.
        let (app, _events) = start();
        assert_eq!((app.info.clone(), backend.0.lock().unwrap().1), (first.clone(), 1));
        drop(app);
        // The next one starts with that copy and asks again in the background.
        let now = BackendInfo { installed: vec![AgentKind::Copilot, AgentKind::Claude], ..first.clone() };
        backend.0.lock().unwrap().0 = now.clone();
        let (mut app, events) = start();
        assert_eq!(app.info, first, "no round trip before the first frame");
        let event = events.recv_timeout(std::time::Duration::from_secs(5)).expect("asked in the background");
        app.on_app_event(event);
        assert_eq!((app.info.clone(), app.installed_agents.clone()), (now.clone(), now.installed.clone()));
        assert_eq!(crate::prefs::PrefsFile::new(prefs.clone()).load_info(), Some(now), "kept for the next start");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_services_figures_redraw_only_when_they_change() {
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        let stats = |mb: u64, alive: u32| ServiceMsg::Stats(ServiceStats { private_bytes: mb << 20, alive, terminals: alive, ..Default::default() });
        app.resync_terms = false;
        app.on_service(stats(100, 0));
        assert!(app.needs_draw, "first figures");
        app.needs_draw = false;
        app.on_service(stats(100, 0));
        assert!(!app.needs_draw, "the same figures: no redraw every 3 s (S6-11)");
        app.on_service(stats(140, 0));
        assert!(app.needs_draw, "the status bar changes");
    }

    #[test]
    fn an_earlier_runs_state_never_overwrites_a_newer_one() {
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        let info = |alive: bool, started_ms: u64| TermInfo { id: "t".into(), alive, started_ms, exit_code: (!alive).then_some(0), ..Default::default() };
        app.on_service(ServiceMsg::Terminals(vec![info(true, 200)]));
        // The first run's exit, read before the relaunch and sent after it (R2-A-08).
        app.on_service(ServiceMsg::Terminals(vec![info(false, 100)]));
        assert!(app.owned_alive("t"), "the relaunch stays alive");
        app.on_service(ServiceMsg::Terminals(vec![info(false, 200)]));
        assert!(!app.owned_alive("t"), "its own exit is taken");
        app.on_service(ServiceMsg::Terminals(vec![info(true, 300)]));
        assert!(app.owned_alive("t"));
        // A reconnect's whole list is the service's present, whatever its clock says.
        app.reset_service_state();
        app.on_service(ServiceMsg::Terminals(vec![info(true, 50)]));
        assert_eq!(app.terms.get("t").map(|t| t.started_ms), Some(50));
    }

    #[test]
    fn a_connection_that_went_silent_is_told_from_one_that_was_closed() {
        use std::sync::atomic::Ordering;
        let (conn, _sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        app.conn.wire.bytes_in.store(500, Ordering::Relaxed);
        app.on_tick();
        assert!(!app.went_silent(), "bytes just came");
        app.heard.1 -= app.conn_options.dead_after;
        assert!(app.went_silent(), "nothing for the dead-after time: the watchdog closed it");
        app.conn.wire.bytes_in.fetch_add(40, Ordering::Relaxed);
        assert!(!app.went_silent(), "a heartbeat since: the far side closed it");
        // A large message still arriving: no whole frame for long, but bytes came (R3-C-07).
        app.heard.1 -= app.conn_options.dead_after;
        let liveness = Arc::new(canvas_common::wire::Liveness::standard());
        liveness.received();
        app.conn.liveness = Some(liveness);
        assert!(!app.went_silent(), "a part of a message 0 s ago");
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
        // A resync whose answer never comes asks again.
        app.request_doc();
        assert!(matches!(sent.try_recv(), Ok(ClientMsg::DocOpen { .. })));
        app.on_tick();
        assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::DocOpen { .. })), "not at once");
        app.doc_asked_at = Instant::now() - DOC_RETRY;
        app.on_tick();
        assert!(sent.try_iter().any(|m| matches!(m, ClientMsg::DocOpen { .. })), "asked again");
        app.on_service(ServiceMsg::Doc { key: "k".into(), version: 9, json: serde_json::to_string(&reply).unwrap() });
        assert!(!app.doc_resync);
        // Save errors from the service are shown.
        app.on_service(ServiceMsg::DocSaved { key: "k".into(), version: 9, error: Some("disk full".into()) });
        assert_eq!(app.save_error.as_deref(), Some("the service cannot save the canvas: disk full"));
    }

    #[test]
    fn a_slow_document_asked_for_twice_sends_no_edit_twice_and_a_refused_edit_is_not_sent_again() {
        let (conn, sent) = detached();
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: vec![node("a"), node("b")], ..Doc::default() };
        let opened = Opened { key: "k".into(), version: 5, path: String::new(), client: 7, doc: doc.clone() };
        let mut app = App::new(conn, Arc::new(crate::rpc::Offline), opened, false, ".".into(), app_tx, None);
        let reply = serde_json::to_string(&crate::sync::DocReply { path: "p".into(), client: 7, doc: Some(doc), error: None }).unwrap();
        // Asked for, not there after 10 s: asked again, and later only after a longer wait.
        app.request_doc();
        app.doc_asked_at -= DOC_RETRY;
        app.on_tick();
        assert_eq!(sent.try_iter().filter(|m| matches!(m, ClientMsg::DocOpen { .. })).count(), 2);
        app.doc_asked_at = Instant::now() - DOC_RETRY;
        app.on_tick();
        assert_eq!(sent.try_iter().filter(|m| matches!(m, ClientMsg::DocOpen { .. })).count(), 0, "the next wait is longer");
        // The first answer comes; an edit goes; the second answer (made before the edit) comes.
        app.on_service(ServiceMsg::Doc { key: "k".into(), version: 5, json: reply.clone() });
        app.doc.node_mut("a").unwrap().x = 4;
        app.flush_doc();
        assert_eq!(edits(&sent).len(), 1);
        app.on_service(ServiceMsg::Doc { key: "k".into(), version: 5, json: reply.clone() });
        app.flush_doc();
        assert!(edits(&sent).is_empty(), "the late copy is dropped: the edit is not sent again (R2-D-09)");
        assert_eq!((app.replica.in_flight(), app.doc.node("a").unwrap().x), (1, 4));
        // That edit is refused: the service sends the canvas unasked. The
        // change goes (S8-05), and one not sent yet stays and goes alone.
        app.doc.node_mut("b").unwrap().y = 2;
        app.on_service(ServiceMsg::Doc { key: "k".into(), version: 5, json: reply });
        assert_eq!((app.replica.in_flight(), app.doc.node("a").unwrap().x, app.doc.node("b").unwrap().y), (0, 0, 2));
        app.flush_doc();
        let resent = edits(&sent);
        assert!(matches!(resent.as_slice(), [(5, ops)] if matches!(ops.as_slice(), [crate::sync::Op::NodeFields(n, f)] if n.id == "b" && *f == ["y"])), "{resent:?}");
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

