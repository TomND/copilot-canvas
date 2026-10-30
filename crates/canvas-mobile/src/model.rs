//! The records the Swift app reads (UniFFI): sessions, groups, screens.

use canvas_model::status::Status;

/// The connection to the service.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConnState {
    /// No transport yet, or the app is reconnecting.
    #[default]
    Connecting,
    /// Authenticated: the service streams state.
    Online,
    /// The transport closed; the app retries.
    Offline,
    /// The service proved this phone was revoked: pair again (no retries).
    Revoked,
}

/// A session's state, as the desktop canvas shows it.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Working,
    /// Waiting for you: a permission prompt or a question.
    NeedsYou,
    /// A turn finished that you have not looked at yet.
    Done,
    Idle,
    Running,
    /// Running in another terminal or VS Code.
    Elsewhere,
    Stopped,
    Failed,
    Preparing,
    Unknown,
}

impl From<Status> for StatusKind {
    fn from(status: Status) -> Self {
        match status {
            Status::Working => StatusKind::Working,
            Status::Waiting => StatusKind::NeedsYou,
            Status::JustFinished => StatusKind::Done,
            Status::Idle => StatusKind::Idle,
            Status::Running => StatusKind::Running,
            Status::External => StatusKind::Elsewhere,
            Status::Stopped => StatusKind::Stopped,
            Status::Failed => StatusKind::Failed,
            Status::Preparing => StatusKind::Preparing,
            Status::Unknown => StatusKind::Unknown,
        }
    }
}

/// What a canvas box is.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    Session,
    /// A subagent of a session (opens its parent).
    Subagent,
    /// A scheduled automation (no terminal).
    Automation,
    /// A passive session (runs on events and schedules).
    Passive,
}

/// A subagent of a session, from its history.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct SubagentItem {
    pub key: String,
    pub name: String,
    pub description: String,
    pub status: StatusKind,
}

/// One box of the canvas (sessions, subagents, automations).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct SessionItem {
    pub id: String,
    pub kind: BoxKind,
    pub name: String,
    /// "copilot", "opencode" or "claude".
    pub agent: String,
    pub status: StatusKind,
    /// "working", "needs you", "done · review", …
    pub status_label: String,
    /// The box's second line: the terminal's title (the agent's intent), an error, the folder.
    pub subtitle: String,
    pub cwd: String,
    pub branch: String,
    /// Parent session (subagents and subsessions).
    pub parent: Option<String>,
    /// The group the box is in (geometric membership).
    pub group: Option<String>,
    /// A terminal of this service runs it now.
    pub alive: bool,
    /// The service can start or resume it (not running elsewhere, not an automation).
    pub can_open: bool,
    /// Last activity (history event or output), ms since the epoch.
    pub last_activity_ms: u64,
    /// "every 15m" for looping sessions.
    pub loop_label: String,
    /// Messages waiting to be typed in (passive sessions' mail).
    pub mailbox: u32,
    pub subagents: Vec<SubagentItem>,
    /// Passive sessions: the key, paused, the headline of the last run, a run
    /// in progress, the next run (ms since the epoch; 0: none).
    pub passive_key: String,
    pub paused: bool,
    pub passive_headline: String,
    pub passive_running: bool,
    pub next_run_ms: u64,
    /// A subsession can fork this session's conversation (`can_fork` checks
    /// Copilot's history on the service's machine).
    pub forkable: bool,
    /// Canvas geometry (cells of the desktop canvas).
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A group box.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct GroupItem {
    pub id: String,
    pub name: String,
    /// Index into the canvas' group palette.
    pub color: u8,
    pub folder: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Everything the session list and the canvas show.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Overview {
    pub state: ConnState,
    /// Why it is offline, or the last notice.
    pub message: String,
    pub sessions: Vec<SessionItem>,
    pub groups: Vec<GroupItem>,
    /// The canvas file on the service's machine.
    pub canvas_path: String,
    /// Agents installed on the service's machine, the default first.
    pub agents: Vec<String>,
    /// Where the folder browser starts (the user's profile on the service's machine).
    pub home_folder: String,
    pub recent_folders: Vec<String>,
}

/// A run of cells with one style. Colours: 0 default, 1–256 palette index + 1,
/// `0x0100_0000 | 0xRRGGBB` true colour. `flags` and `underline` as in
/// protocol 8 (`F_*`, `F8_*`); `link` indexes [`Screen::links`] (0: none).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ScreenRun {
    pub col: u16,
    /// One grapheme cluster per cell.
    pub cells: Vec<String>,
    /// Every cell is two columns wide.
    pub wide: bool,
    pub fg: u32,
    pub bg: u32,
    pub flags: u32,
    pub underline: u32,
    pub link: u32,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ScreenRow {
    pub y: u16,
    pub runs: Vec<ScreenRun>,
}

/// A terminal's screen, or what changed since it was last taken.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Screen {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    /// Every row is included (start over); otherwise only changed rows.
    pub full: bool,
    pub changed: Vec<ScreenRow>,
    /// Rows showing your own prompts (Copilot): tinted like on the desktop.
    pub user_rows: Vec<bool>,
    pub cursor_visible: bool,
    pub cursor_col: u16,
    pub cursor_row: u16,
    /// 0 block, 1 underline, 2 bar, 3 hollow block.
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    /// `MODE_*` bits (mouse reporting, alternate screen, …).
    pub modes: u32,
    /// Lines scrolled back (0: the live screen) and scrollback size.
    pub display_offset: u32,
    pub history: u32,
    /// OSC 8 targets.
    pub links: Vec<String>,
    /// The program's title (the agent's current intent).
    pub title: String,
    pub alive: bool,
}

/// A key (the service encodes it for the program's keyboard mode).
#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Char { text: String },
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F { n: u8 },
}

/// Modifier bits for [`KeyCode`]: shift 1, alt 2, ctrl 4.
pub const MOD_SHIFT: u8 = canvas_common::protocol::MOD_SHIFT;
pub const MOD_ALT: u8 = canvas_common::protocol::MOD_ALT;
pub const MOD_CTRL: u8 = canvas_common::protocol::MOD_CTRL;

/// What the core tells the app (on a background thread).
#[derive(uniffi::Enum, Clone, Debug, PartialEq)]
pub enum CoreEvent {
    /// The connection changed state.
    Connection { state: ConnState, message: String },
    /// Sessions, statuses or the canvas changed: read `overview()` again.
    Overview,
    /// A terminal's screen changed: read `take_screen(id)`.
    Screen { id: String },
    /// Something to show briefly.
    Notice { text: String, error: bool },
    /// A program copied text (OSC 52) after this phone typed in it.
    Clipboard { text: String },
    /// A program sent a desktop notification (OSC 777 / OSC 9).
    Notification { term: String, title: String, body: String },
    /// A session started waiting for you, or finished a turn.
    Attention { id: String, name: String, status: StatusKind },
    /// A fresh Dev Tunnels access token: store it for the next connection.
    TunnelAccess { url: String, token: String, expires_ms: u64 },
    /// The reports inbox changed: read `reports()` again.
    Reports,
    /// New reports came in while the app is open (`more`: others with it).
    ReportArrived { id: String, title: String, source: String, more: u32 },
}

/// How a report is written.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportFormat {
    /// A short plain-text message.
    Message,
    Markdown,
    Html,
}

/// A link a report carries (`http(s)` or `mailto` only).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ReportLink {
    pub title: String,
    pub url: String,
}

/// A report or message an agent published to the inbox.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ReportItem {
    pub id: String,
    pub title: String,
    pub format: ReportFormat,
    /// "info", "ok", "warn", "alert", "queued", "delivered", "failed".
    pub kind: String,
    /// The kind's glyph (◇ ✓ ⚠ ▲ ◌ ✗).
    pub glyph: String,
    /// When it was published (ms since the epoch).
    pub created_ms: u64,
    /// Who published it: its box's name on this canvas, else the name it had.
    pub source_name: String,
    /// Its box on this canvas (empty: not on it).
    pub source_box: String,
    /// A related session it names (and its box on this canvas, if any).
    pub session_name: String,
    pub session_box: String,
    pub group_name: String,
    /// A related pull request (an address).
    pub pr: String,
    pub links: Vec<ReportLink>,
    /// Reports with the same thread key are one row.
    pub thread: String,
    /// The start of its text, one line, without the title.
    pub preview: String,
    /// Body bytes.
    pub size: u64,
    pub unread: bool,
}

/// One row of the inbox: a report, or the reports of one thread (newest first).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ReportThread {
    /// The newest report's id.
    pub id: String,
    pub latest: ReportItem,
    pub items: Vec<ReportItem>,
    pub unread: u32,
}

/// A report the service refused after it was queued.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct RefusedReport {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub ms: u64,
}

/// The reports inbox of a machine.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ReportsOverview {
    /// The service sent its inbox (an older service never does).
    pub loaded: bool,
    pub threads: Vec<ReportThread>,
    /// Rows with something unread (the badge).
    pub unread: u32,
    /// Reports stored on the service.
    pub total: u32,
    pub refused: Vec<RefusedReport>,
}

/// A report's page for the app's locked-down web view: sanitized HTML with
/// a strict Content-Security-Policy (Markdown and messages rendered to it).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ReportPage {
    pub id: String,
    pub format: ReportFormat,
    pub html: String,
}

/// A new session to create and start.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NewSession {
    pub name: String,
    /// "copilot", "opencode", "claude" (empty: the service's default).
    pub agent: String,
    /// Folder on the service's machine.
    pub cwd: String,
    /// First prompt (empty: none).
    pub prompt: String,
    /// Put the box in this group (empty: next to the others).
    pub group: String,
    /// A subsession of this session (empty: none): linked and placed next to it.
    pub parent: String,
    /// Fork the parent's conversation (else start fresh).
    pub fork: bool,
}

/// A tunnel access token that came in a notification.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TunnelAccessInfo {
    pub url: String,
    pub token: String,
    pub expires_ms: u64,
}

/// A paired service, as the pairing gave it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct PairResult {
    pub client_id: String,
    pub secret: Vec<u8>,
}

/// A scanned `ccanvas://pair?…` link.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct PairLinkInfo {
    pub name: String,
    pub url: String,
    pub code: String,
    pub token: String,
    pub token_expires_ms: u64,
}

/// A folder entry of the service's machine.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FolderListing {
    pub folder: String,
    /// "..", then sub-folders (names).
    pub entries: Vec<String>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{message}")]
    Failed { message: String },
}

impl CoreError {
    pub fn msg(message: impl Into<String>) -> Self {
        CoreError::Failed { message: message.into() }
    }
}

impl From<anyhow::Error> for CoreError {
    fn from(error: anyhow::Error) -> Self {
        CoreError::Failed { message: format!("{error:#}") }
    }
}
