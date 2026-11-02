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
    /// Sessions, statuses, the canvas or the todo boards changed: read
    /// `overview()` (and `todos()` when shown) again.
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

// ---------- todos (docs/NOTEBOOK-TASKS.md §3) ----------

/// Where a todo stands: fixed categories in board order (boards may rename them).
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TodoStatus {
    /// An agent's idea: waits for you to accept or decline it.
    Proposed,
    Backlog,
    /// Accepted and ready to be worked on.
    Todo,
    InProgress,
    InReview,
    Done,
    Canceled,
}

/// What kind of work a todo is.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TodoKind {
    Bug,
    Feature,
    Improvement,
    Optimization,
    Chore,
    Docs,
    Research,
}

/// How urgent a todo is (`NoPriority` sorts last).
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TodoPriority {
    Urgent,
    High,
    Medium,
    Low,
    NoPriority,
}

/// Why a todo waits for you.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TodoNeed {
    /// A proposal: accept or decline it.
    Proposed,
    /// Submitted for review.
    InReview,
    /// Claimed by a session that is gone or stopped.
    StaleClaim,
}

/// A todo as lists and cards show it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoItem {
    /// `DEC-031`.
    pub id: String,
    pub title: String,
    pub kind: TodoKind,
    /// The type's glyph on the desktop's cards (✱ ✚ ▲ » ∷ ¶ ?).
    pub kind_glyph: String,
    pub status: TodoStatus,
    /// Its column's name on its board.
    pub status_label: String,
    pub priority: TodoPriority,
    /// `!!!` urgent, `!!` high, `!` medium.
    pub priority_marks: String,
    /// It cannot go on: a reason, or open todos it waits for (`⚑`).
    pub blocked: bool,
    /// Why: the reason, then "waits for DEC-003".
    pub blocked_reason: String,
    /// It blocks the work of others (`‼`).
    pub blocker: bool,
    /// Open todos waiting for it.
    pub blocks: u32,
    /// Its board or swimlane (id), that board's name and the top-level board (id).
    pub board: String,
    pub board_name: String,
    pub root_board: String,
    /// The swimlane's name (empty: the top-level board's own lane).
    pub lane: String,
    /// Who works on it: the session's box name (else the name it had), "you"; empty: nobody.
    pub claimed_by: String,
    /// The claiming session's box on this canvas (empty: none).
    pub claimed_session: String,
    /// That session works right now (`⟳`).
    pub claim_working: bool,
    /// An agent created it (`◇`).
    pub ai_created: bool,
    /// Why an agent proposed it (empty: not a proposal).
    pub proposal_why: String,
    pub checklist_done: u32,
    pub checklist_total: u32,
    pub labels: Vec<String>,
    /// Its pull requests (web links).
    pub prs: Vec<String>,
    /// Status todo, not blocked, not claimed.
    pub ready: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// A swimlane: the board itself first, then its sub-boards.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoLane {
    pub id: String,
    pub name: String,
}

/// A column's todos in one swimlane (ids in rank order).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoCell {
    pub lane: String,
    pub ids: Vec<String>,
}

/// A column of a board (Proposed only when there are proposals or the
/// board's policy makes agents' todos proposals).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoColumn {
    pub status: TodoStatus,
    /// The board's name for it.
    pub name: String,
    pub count: u32,
    /// In progress: the WIP limit (0: none) and whether it is over it.
    pub wip: u32,
    pub over: bool,
    /// Done: shown collapsed to its count.
    pub collapsed: bool,
    /// One per swimlane, in lane order.
    pub cells: Vec<TodoCell>,
}

/// A top-level board with its swimlanes and columns.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoBoardItem {
    pub id: String,
    pub name: String,
    /// Todo ids are `<PREFIX>-<n>`.
    pub prefix: String,
    /// Index into the canvas' group palette.
    pub color: u8,
    /// Canvas groups (ids) it is linked to.
    pub groups: Vec<String>,
    pub lanes: Vec<TodoLane>,
    pub columns: Vec<TodoColumn>,
    /// Canceled todos (a count only) and open todos that are blocked.
    pub canceled: u32,
    pub blocked: u32,
    /// The progress bar: done, of the accepted todos not canceled.
    pub done: u32,
    pub total: u32,
    /// Every todo an agent creates on it is a proposal.
    pub agents_propose: bool,
}

/// A todo that waits for you, and why.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoNeedItem {
    pub need: TodoNeed,
    pub todo: TodoItem,
}

/// A canvas group's todos (its title chip: `☑ 4 · 2▸ · 1◇`).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoGroupCounts {
    pub group: String,
    /// Accepted and not finished; of those in progress; proposals waiting.
    pub open: u32,
    pub in_progress: u32,
    pub proposed: u32,
}

/// The canvas's todo boards.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodosOverview {
    /// The service sent its boards (an older service never does).
    pub loaded: bool,
    /// Top-level boards in book order (sub-boards are their swimlanes).
    pub boards: Vec<TodoBoardItem>,
    /// Every todo, in book order.
    pub todos: Vec<TodoItem>,
    /// Proposals (oldest first), todos in review, then stale claims.
    pub needs_you: Vec<TodoNeedItem>,
    /// Ready todos (ids) by priority, then rank.
    pub ready: Vec<String>,
    /// Open todos claimed by sessions (ids), the newest claim first.
    pub claimed: Vec<String>,
    /// Canvas groups with a linked board.
    pub groups: Vec<TodoGroupCounts>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoCheck {
    pub text: String,
    pub done: bool,
}

/// What a todo links to.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoLink {
    /// "session", "pr", "note", "report", "file", "url".
    pub kind: String,
    /// A session id, a web link, a path or a report id.
    pub target: String,
    pub title: String,
}

/// How a todo relates to another.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoRelation {
    /// "blocked by", "related to", "discovered from", "duplicate of".
    pub kind: String,
    pub id: String,
    /// The other todo's title (empty: not in the book).
    pub title: String,
    /// The other todo is done or canceled.
    pub closed: bool,
}

/// A line of a todo's log: a note someone added, or a change.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoLogLine {
    pub ms: u64,
    pub by: String,
    /// A note (else a recorded change).
    pub note: bool,
    pub text: String,
}

/// Everything about one todo (its details pane).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoDetail {
    pub item: TodoItem,
    /// Markdown, and as a page for the locked-down web view (empty without a body).
    pub body: String,
    pub body_html: String,
    pub checklist: Vec<TodoCheck>,
    pub relations: Vec<TodoRelation>,
    pub links: Vec<TodoLink>,
    /// The newest first.
    pub log: Vec<TodoLogLine>,
    /// How it ended: "done", "wont_do", "duplicate", "declined", "obsolete" (empty: open).
    pub resolution: String,
    pub created_by: String,
    /// Its spec note (a path on the service's machine) and parent todo (empty: none).
    pub note: String,
    pub parent: String,
    /// The blocked reason as written (empty: none; `item.blocked_reason`
    /// adds the todos it waits for).
    pub blocked_text: String,
    pub actions: TodoActions,
}

/// A todo to add (`Client::add_todo`).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoDraft {
    /// A board id, name or prefix (`Decomps/Parser`: a swimlane).
    pub board: String,
    pub title: String,
    pub kind: TodoKind,
    pub priority: TodoPriority,
    /// Backlog when none.
    pub status: Option<TodoStatus>,
    /// Markdown (empty: none).
    pub body: String,
    /// It blocks the work of others (`‼`).
    pub blocker: bool,
}

/// Field changes to a todo (`Client::update_todo`); `None` keeps a field.
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq)]
pub struct TodoEdit {
    pub title: Option<String>,
    pub kind: Option<TodoKind>,
    pub priority: Option<TodoPriority>,
    /// Markdown (empty: none).
    pub body: Option<String>,
    /// Replaces the labels.
    pub labels: Option<Vec<String>>,
    pub blocker: Option<bool>,
    /// Why it cannot go on; empty: not blocked any more.
    pub blocked: Option<String>,
}

/// What you may do with a todo now (the desktop's buttons for it).
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq)]
pub struct TodoActions {
    /// A proposal: accept it (into a status) or decline it.
    pub accept: bool,
    pub decline: bool,
    pub done: bool,
    pub cancel: bool,
    /// Done or canceled (a declined proposal reopens as a proposal).
    pub reopen: bool,
    /// Claimed: release the claim.
    pub release: bool,
    /// Columns it can move to (not its own; Done and Canceled have their actions).
    pub moves: Vec<TodoStatus>,
}

/// What a todo action did (already shown here; the service confirms it).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct TodoChange {
    /// The todo it changed or created.
    pub id: String,
    /// For people, with warnings (over a WIP limit, …).
    pub message: String,
}

// ---------- notebook (docs/NOTEBOOK-TASKS.md §2) ----------

/// Whom a note is for.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteAudience {
    /// For you (`●`).
    Human,
    /// For agents (`◇`).
    Ai,
}

/// Who wrote a note.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteAuthor {
    Human,
    Ai,
    /// An agent's note you edited.
    Mixed,
}

/// What a notebook entry is.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteKind {
    Folder,
    Markdown,
    Html,
    /// Other known text files.
    Text,
    /// `png jpg jpeg gif svg webp`.
    Image,
    Other,
}

/// Which notes a listing shows (folders always show).
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteFilter {
    All,
    /// Notes for you.
    Human,
    /// Notes for agents.
    Ai,
}

/// What an explorer root is.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotebookRootKind {
    /// The canvas's notebook root.
    Notebook,
    /// A group's folder outside the root.
    Group,
    /// The canvas-wide folder (written by and for agents).
    CanvasWide,
    /// An extra folder mounted in the notebook.
    Mount,
}

/// Whom a note is for and who wrote it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteProvenance {
    pub audience: NoteAudience,
    pub author: NoteAuthor,
    /// The audience was inferred (folder, file type), not the note's own metadata.
    pub inferred: bool,
    /// A file of the canvas's own (listings hide it unless asked).
    pub internal_file: bool,
    /// A passive session's memory.
    pub passive: bool,
    /// The agent that wrote it ("copilot", "opencode", "claude"; empty: unknown).
    pub agent: String,
    /// The session that wrote it (its id) and its box's name on this canvas (empty: not on it).
    pub session: String,
    pub session_name: String,
    /// When it was written, as the note says.
    pub created: String,
    pub reviewed: bool,
}

/// One root of the notebook's explorer.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NotebookRoot {
    pub kind: NotebookRootKind,
    /// "Notebook", "◫ Payments", "Canvas (canvas-wide)", a mount's folder name.
    pub label: String,
    /// A folder on the service's machine.
    pub path: String,
    /// The group whose folder it is (empty: none).
    pub group: String,
}

/// The canvas's notebook on the service's machine.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NotebookOverview {
    /// The notebook root (the service creates it with `Groups`, `Inbox`, `Todos`).
    pub root: String,
    /// The canvas-wide folder (empty: the canvas has no id).
    pub canvas_dir: String,
    /// The explorer's roots: the notebook, group folders outside it, the
    /// canvas-wide folder, mounts.
    pub roots: Vec<NotebookRoot>,
}

/// A file or folder of the notebook.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteEntry {
    pub name: String,
    pub path: String,
    /// Relative to its root, `/` separators.
    pub rel: String,
    pub kind: NoteKind,
    /// Bytes (0 for folders).
    pub size: u64,
    pub modified_ms: u64,
    /// Its metadata title, first heading, else its name without the extension.
    pub title: String,
    pub provenance: NoteProvenance,
}

/// A notebook folder as the phone lists it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteListing {
    pub folder: String,
    /// Folders first, then files, by name.
    pub entries: Vec<NoteEntry>,
    /// Notes the filter hid, and the canvas's own files left out.
    pub hidden: u32,
    pub internal_files: u32,
}

/// A note's page for the locked-down web view.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NotePage {
    pub path: String,
    pub title: String,
    pub kind: NoteKind,
    /// Its revision (`note_revisions` tells when it changed).
    pub rev: String,
    /// Its text (empty for pictures and other files).
    pub text: String,
    /// One sanitized page: Markdown rendered, HTML sanitized, text as a
    /// block, pictures (and pictures of its folder) inline.
    pub html: String,
    pub provenance: NoteProvenance,
}

/// A note's text for the source editor (`Client::note_source`).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteSource {
    pub path: String,
    pub kind: NoteKind,
    pub text: String,
    /// Its revision: the base `save_note` writes on.
    pub rev: String,
    /// Markdown, HTML and text files inside the notebook's roots.
    pub editable: bool,
    /// Why it cannot be edited here (empty: it can).
    pub why_not: String,
}

/// What `Client::save_note` wrote.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteSaved {
    /// The new revision (the next save's base).
    pub rev: String,
    /// The text written: yours, and `author: mixed` when an agent wrote the note.
    pub text: String,
}

/// A notebook search hit.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct NoteHit {
    pub path: String,
    /// 1-based line of a text hit; 0: the file's name matched.
    pub line: u32,
    /// The line around the match, or the file's name.
    pub text: String,
}

/// What a link of a note page opens in the app (`#note:` / `#todo:` links).
#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum NoteLink {
    /// A `[[Name]]` link (the name as written, maybe with `#heading`): `resolve_note` finds it.
    Note { name: String },
    /// A todo id (`DEC-031`).
    Todo { id: String },
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
    /// A note changed elsewhere since the revision a save was based on:
    /// nothing was written; its text and revision now.
    #[error("{message}")]
    Conflict { message: String, text: String, rev: String },
}

impl CoreError {
    pub fn msg(message: impl Into<String>) -> Self {
        CoreError::Failed { message: message.into() }
    }

    /// What to tell people.
    pub fn message(&self) -> &str {
        match self {
            CoreError::Failed { message } | CoreError::Conflict { message, .. } => message,
        }
    }
}

impl From<anyhow::Error> for CoreError {
    fn from(error: anyhow::Error) -> Self {
        CoreError::Failed { message: format!("{error:#}") }
    }
}
