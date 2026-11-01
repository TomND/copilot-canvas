//! The canvas's todo boards (docs/NOTEBOOK-TASKS.md, "Tasks"): boards,
//! todos, the operations on them and their rules. Pure data and logic; the
//! service owns the stored book (`<home>/canvas-agents/<canvas id>/todos/
//! book.json`) and is its only writer. Windows apply the same operations
//! optimistically and send them as `{"action": "todos", "op": …}`
//! ([`client_action`]); clients get the book as `bus::Snapshot::todos`.
//!
//! - [`TodoBook`] holds the boards, the todos and the id sequences.
//! - [`TodoOp`] is every change; [`TodoBook::apply`] runs one for an
//!   [`Actor`] under the canvas's rules: humans and the TODO bot may do
//!   everything, agents add, propose, update, claim, note, check, link,
//!   relate and submit (never accept, decline, delete or close unless the
//!   board lets them close; never a proposal, never the status of a todo
//!   another live session claimed).
//! - Queries for the clients: [`TodoBook::board_view`] (columns with
//!   sub-boards as swimlanes), [`TodoBook::ready`], [`TodoBook::needs_you`],
//!   [`TodoBook::group_counts`], [`TodoBook::claimed_by_session`].
//! - Text: [`import_markdown`] / [`mirror_markdown`] (the owner's TODOs.md
//!   format), [`json_canvas`] (JSON Canvas 1.0), [`group_summary`] and
//!   [`canvas_summary`] (the agents' digests), [`demo_book`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

mod export;
mod markdown;
mod ops;
#[cfg(test)]
mod tests;

pub use export::{canvas_summary, client_action, demo_book, group_summary, json_canvas};
pub use markdown::{Imported, import_markdown, mirror_markdown};
pub use ops::{Applied, Ctx, NewTodo, TodoEvent, TodoOp, TodoUpdate};

/// The book's format version. Bump it when a change adds data an older
/// version would drop on its next write (it reads unknown fields as
/// nothing): the service refuses to change a book of a newer version.
pub const VERSION: u32 = 1;
/// `schemaVersion` of `copilot-canvas todo … --format json`.
pub const SCHEMA_VERSION: u32 = 1;
/// Longest title (characters).
pub const MAX_TITLE: usize = 200;
/// Longest body (bytes).
pub const MAX_BODY: usize = 64 * 1024;
/// Longest note, reason or checklist item (characters).
pub const MAX_TEXT: usize = 4_000;
/// Log entries kept per todo (the newest).
pub const MAX_LOG: usize = 200;
/// Bytes of log text kept per todo (the newest entries; at least one).
pub const MAX_LOG_BYTES: usize = 32 * 1024;
pub const MAX_CHECKLIST: usize = 100;
pub const MAX_LABELS: usize = 20;
pub const MAX_LINKS: usize = 50;
pub const MAX_RELATIONS: usize = 100;
pub const MAX_TODOS: usize = 20_000;
pub const MAX_BOARDS: usize = 200;
/// The highest number of an id (ids have at most 9 digits).
pub const MAX_SEQ: u32 = 999_999_999;
/// A new board's size on the todo canvas (world cells).
pub const BOARD_W: i32 = 150;
pub const BOARD_H: i32 = 40;
/// Boards stay within this many world cells of the origin.
pub const MAX_COORD: i32 = 1_000_000;

/// Where a todo stands: fixed categories, in this order (boards may rename them).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// An agent's idea (or an agent's todo on a board whose policy says so):
    /// waits for the user to accept or decline it.
    Proposed,
    #[default]
    Backlog,
    /// Accepted and ready to be worked on (`todo ready` lists these).
    Todo,
    InProgress,
    InReview,
    Done,
    Canceled,
}

impl Status {
    pub const ALL: [Status; 7] = [Status::Proposed, Status::Backlog, Status::Todo, Status::InProgress, Status::InReview, Status::Done, Status::Canceled];

    /// Its JSON and command-line word (`in_progress`).
    pub fn word(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Backlog => "backlog",
            Status::Todo => "todo",
            Status::InProgress => "in_progress",
            Status::InReview => "in_review",
            Status::Done => "done",
            Status::Canceled => "canceled",
        }
    }

    /// For people (`In progress`).
    pub fn label(self) -> &'static str {
        match self {
            Status::Proposed => "Proposed",
            Status::Backlog => "Backlog",
            Status::Todo => "Todo",
            Status::InProgress => "In progress",
            Status::InReview => "In review",
            Status::Done => "Done",
            Status::Canceled => "Canceled",
        }
    }

    /// A status from a word (`in-progress`, `In progress`, `review`, `cancelled`, …).
    pub fn parse(text: &str) -> Option<Status> {
        let word = text.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match word.as_str() {
            "proposed" | "proposal" => Status::Proposed,
            "backlog" => Status::Backlog,
            "todo" | "to_do" | "ready" | "open" => Status::Todo,
            "in_progress" | "inprogress" | "doing" | "wip" | "started" => Status::InProgress,
            "in_review" | "inreview" | "review" => Status::InReview,
            "done" | "closed" | "complete" | "completed" => Status::Done,
            "canceled" | "cancelled" => Status::Canceled,
            _ => return None,
        })
    }

    /// Done or canceled.
    pub fn closed(self) -> bool {
        matches!(self, Status::Done | Status::Canceled)
    }

    /// Accepted and not finished: backlog, todo, in progress, in review.
    pub fn open(self) -> bool {
        matches!(self, Status::Backlog | Status::Todo | Status::InProgress | Status::InReview)
    }
}

/// How a closed todo ended.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Done,
    WontDo,
    Duplicate,
    Declined,
    Obsolete,
}

impl Resolution {
    pub fn word(self) -> &'static str {
        match self {
            Resolution::Done => "done",
            Resolution::WontDo => "wont_do",
            Resolution::Duplicate => "duplicate",
            Resolution::Declined => "declined",
            Resolution::Obsolete => "obsolete",
        }
    }

    pub fn parse(text: &str) -> Option<Resolution> {
        let word = text.trim().to_ascii_lowercase().replace(['-', ' ', '\''], "_");
        Some(match word.as_str() {
            "done" => Resolution::Done,
            "wont_do" | "won_t_do" | "wontdo" | "wontfix" | "wont_fix" => Resolution::WontDo,
            "duplicate" | "dup" => Resolution::Duplicate,
            "declined" => Resolution::Declined,
            "obsolete" => Resolution::Obsolete,
            _ => return None,
        })
    }
}

/// What kind of work a todo is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TodoType {
    Bug,
    #[default]
    Feature,
    Improvement,
    Optimization,
    Chore,
    Docs,
    Research,
}

impl TodoType {
    pub const ALL: [TodoType; 7] = [TodoType::Bug, TodoType::Feature, TodoType::Improvement, TodoType::Optimization, TodoType::Chore, TodoType::Docs, TodoType::Research];

    pub fn word(self) -> &'static str {
        match self {
            TodoType::Bug => "bug",
            TodoType::Feature => "feature",
            TodoType::Improvement => "improvement",
            TodoType::Optimization => "optimization",
            TodoType::Chore => "chore",
            TodoType::Docs => "docs",
            TodoType::Research => "research",
        }
    }

    /// A type from a word (also `enhancement`, `perf`, `doc`, `task`, `spike`).
    pub fn parse(text: &str) -> Option<TodoType> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "bug" | "fix" | "defect" => TodoType::Bug,
            "feature" | "feat" => TodoType::Feature,
            "improvement" | "enhancement" | "improve" => TodoType::Improvement,
            "optimization" | "optimisation" | "perf" | "performance" => TodoType::Optimization,
            "chore" | "task" | "maintenance" => TodoType::Chore,
            "docs" | "doc" | "documentation" => TodoType::Docs,
            "research" | "spike" | "investigation" => TodoType::Research,
            _ => return None,
        })
    }
}

/// How urgent a todo is (`None` sorts last).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Urgent,
    High,
    Medium,
    Low,
    #[default]
    None,
}

impl Priority {
    pub const ALL: [Priority; 5] = [Priority::Urgent, Priority::High, Priority::Medium, Priority::Low, Priority::None];

    pub fn word(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
            Priority::None => "none",
        }
    }

    /// A priority from a word (also `p0`…`p4`, `critical`, `normal`).
    pub fn parse(text: &str) -> Option<Priority> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "urgent" | "critical" | "p0" => Priority::Urgent,
            "high" | "p1" => Priority::High,
            "medium" | "normal" | "p2" => Priority::Medium,
            "low" | "p3" => Priority::Low,
            "none" | "no" | "p4" | "" => Priority::None,
            _ => return None,
        })
    }

    /// The card's marks: `!!!` urgent, `!!` high, `!` medium.
    pub fn marks(self) -> &'static str {
        match self {
            Priority::Urgent => "!!!",
            Priority::High => "!!",
            Priority::Medium => "!",
            Priority::Low | Priority::None => "",
        }
    }
}

/// How two todos relate (stored on the first).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RelKind {
    /// It cannot start before the other is done (or canceled).
    BlockedBy,
    Related,
    /// Found while working on the other.
    DiscoveredFrom,
    DuplicateOf,
}

impl RelKind {
    pub fn word(self) -> &'static str {
        match self {
            RelKind::BlockedBy => "blocked_by",
            RelKind::Related => "related",
            RelKind::DiscoveredFrom => "discovered_from",
            RelKind::DuplicateOf => "duplicate_of",
        }
    }

    /// For people: `blocked by`, `related to`, …
    pub fn label(self) -> &'static str {
        match self {
            RelKind::BlockedBy => "blocked by",
            RelKind::Related => "related to",
            RelKind::DiscoveredFrom => "discovered from",
            RelKind::DuplicateOf => "duplicate of",
        }
    }

    pub fn parse(text: &str) -> Option<RelKind> {
        Some(match text.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "blocked_by" | "blockedby" | "depends_on" => RelKind::BlockedBy,
            "related" | "related_to" | "relates_to" => RelKind::Related,
            "discovered_from" => RelKind::DiscoveredFrom,
            "duplicate_of" | "duplicate" => RelKind::DuplicateOf,
            _ => return None,
        })
    }
}

/// What a todo links to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// A canvas session (its id).
    Session,
    /// A pull request (its web link).
    Pr,
    /// A note of the notebook (its path).
    Note,
    /// A report of the inbox (its id).
    Report,
    File,
    Url,
}

impl LinkKind {
    pub fn word(self) -> &'static str {
        match self {
            LinkKind::Session => "session",
            LinkKind::Pr => "pr",
            LinkKind::Note => "note",
            LinkKind::Report => "report",
            LinkKind::File => "file",
            LinkKind::Url => "url",
        }
    }

    pub fn parse(text: &str) -> Option<LinkKind> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "session" => LinkKind::Session,
            "pr" | "pull_request" | "pull-request" => LinkKind::Pr,
            "note" => LinkKind::Note,
            "report" => LinkKind::Report,
            "file" => LinkKind::File,
            "url" | "link" => LinkKind::Url,
            _ => return None,
        })
    }
}
/// Who did something.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// You (a window, or the command outside a canvas session).
    #[default]
    Human,
    /// A canvas session, passive session or canvas-wide agent.
    Agent,
    /// An automation without a session (the TODO bot, a merged pull request).
    Bot,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct Actor {
    pub kind: ActorKind,
    /// The canvas session (agents).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The session's or bot's name when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Actor {
    /// You.
    pub fn human() -> Actor {
        Actor::default()
    }

    /// A canvas session (`name`: its box's name, may be empty).
    pub fn agent(session: &str, name: &str) -> Actor {
        Actor { kind: ActorKind::Agent, session: Some(session.to_owned()), name: Some(name.trim().to_owned()).filter(|n| !n.is_empty()) }
    }

    /// An automation without a session.
    pub fn bot(name: &str) -> Actor {
        Actor { kind: ActorKind::Bot, session: None, name: Some(name.trim().to_owned()).filter(|n| !n.is_empty()) }
    }

    pub fn is_agent(&self) -> bool {
        self.kind == ActorKind::Agent
    }

    /// Its name for people: the session's name (else its short id), `you`, the bot's name.
    pub fn label(&self) -> String {
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            return name.to_owned();
        }
        match self.kind {
            ActorKind::Human => "you".into(),
            ActorKind::Agent => self.session.as_deref().map(|s| format!("session {}", s.chars().take(8).collect::<String>())).unwrap_or_else(|| "an agent".into()),
            ActorKind::Bot => "a bot".into(),
        }
    }

    /// The same actor: a session by its id; people and bots by kind and name.
    pub fn same(&self, other: &Actor) -> bool {
        match (&self.session, &other.session) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.kind == other.kind && (self.kind == ActorKind::Human || self.name == other.name),
            _ => false,
        }
    }
}

/// Who works on a todo, since when.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Claim {
    pub by: Actor,
    pub ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Link {
    pub kind: LinkKind,
    /// The id, path or web link.
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Relation {
    pub kind: RelKind,
    /// The other todo's id.
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CheckItem {
    pub text: String,
    pub done: bool,
}

/// A log line: a note someone added, or a change the book recorded.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    Note,
    #[default]
    History,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LogEntry {
    pub ms: u64,
    pub by: Actor,
    pub kind: LogKind,
    pub text: String,
}

/// Why an agent proposed a todo (kept after it was accepted or declined).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Proposal {
    pub why: String,
    pub ms: u64,
}

/// What agents may do on a board.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AddPolicy {
    /// Agents' `add` creates the todo (backlog or todo).
    #[default]
    Direct,
    /// Every todo an agent creates is a proposal.
    Proposed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AgentPolicy {
    pub agent_adds: AddPolicy,
    /// Agents may mark todos done or canceled (and reopen them).
    pub agents_may_close: bool,
}

/// A board's own names for the statuses (none: the defaults).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct StatusNames {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backlog: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_progress: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canceled: Option<String>,
}

impl StatusNames {
    pub fn is_empty(&self) -> bool {
        *self == StatusNames::default()
    }

    pub fn get(&self, status: Status) -> Option<&str> {
        match status {
            Status::Proposed => self.proposed.as_deref(),
            Status::Backlog => self.backlog.as_deref(),
            Status::Todo => self.todo.as_deref(),
            Status::InProgress => self.in_progress.as_deref(),
            Status::InReview => self.in_review.as_deref(),
            Status::Done => self.done.as_deref(),
            Status::Canceled => self.canceled.as_deref(),
        }
        .filter(|n| !n.trim().is_empty())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TodoSettings {
    /// The Markdown mirror the service rewrites after every change (a path on its machine).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
}

/// A board: a frame on the todo canvas with the status columns. A sub-board
/// (`parent` set) is a swimlane of its parent and shares its prefix.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Board {
    /// Stable id (a slug of the first name; sub-boards: `<parent>-<slug>`).
    pub id: String,
    pub name: String,
    /// Todo ids are `<PREFIX>-<n>` (2 to 5 uppercase letters).
    pub prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Place and size on the todo canvas (world cells; sub-boards: unused).
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub color: u8,
    /// Soft limit of todos in progress (a warning, never a refusal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wip: Option<u32>,
    #[serde(skip_serializing_if = "StatusNames::is_empty")]
    pub statuses: StatusNames,
    /// Canvas groups (ids) this board belongs to: their chips count it,
    /// their sessions' `todo` commands default to it, their passive
    /// sessions get its events (the first group).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub canvas_groups: Vec<String>,
    /// A notes folder linked to the board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub policy: AgentPolicy,
    pub created_by: Actor,
    pub created_ms: u64,
}

impl Default for Board {
    fn default() -> Self {
        Board {
            id: String::new(),
            name: String::new(),
            prefix: String::new(),
            parent: None,
            x: 0,
            y: 0,
            w: BOARD_W,
            h: BOARD_H,
            color: 0,
            wip: None,
            statuses: StatusNames::default(),
            canvas_groups: Vec::new(),
            notes: None,
            policy: AgentPolicy::default(),
            created_by: Actor::default(),
            created_ms: 0,
        }
    }
}

impl Board {
    /// A column's name on this board (its own, else the default).
    pub fn status_name(&self, status: Status) -> String {
        self.statuses.get(status).unwrap_or(status.label()).to_owned()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Todo {
    /// `<PREFIX>-<n>`: assigned by the book, never reused.
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: TodoType,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    pub priority: Priority,
    /// Why it cannot go on (a person's or bot's words).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// It blocks the work of others (the card's `‼`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blocker: bool,
    /// The board (or sub-board) id.
    pub board: String,
    /// A parent todo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Order within its column ([`rank_between`]).
    pub rank: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Its spec note (a path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checklist: Vec<CheckItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<LogEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
    pub created_by: Actor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<Claim>,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_ms: Option<u64>,
    /// The book's `rev` when it last changed.
    pub rev: u64,
}

impl Todo {
    /// Checklist items done and in all.
    pub fn checked(&self) -> (usize, usize) {
        (self.checklist.iter().filter(|c| c.done).count(), self.checklist.len())
    }

    /// The session that claimed it.
    pub fn claim_session(&self) -> Option<&str> {
        self.claimed_by.as_ref().and_then(|c| c.by.session.as_deref())
    }

    /// An agent created it (the card's `◇`).
    pub fn ai_created(&self) -> bool {
        self.created_by.is_agent()
    }

    /// Its pull requests' links.
    pub fn prs(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(|l| l.kind == LinkKind::Pr)
    }

    /// The ids it is blocked by (open or not).
    pub fn blocked_by(&self) -> impl Iterator<Item = &str> {
        self.relations.iter().filter(|r| r.kind == RelKind::BlockedBy).map(|r| r.id.as_str())
    }
}

/// Every board and todo of a canvas.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TodoBook {
    pub version: u32,
    /// Counts applied operations.
    pub rev: u64,
    pub boards: Vec<Board>,
    pub todos: Vec<Todo>,
    /// Prefix → the last number given out (ids are never reused).
    pub seq: BTreeMap<String, u32>,
    pub settings: TodoSettings,
}

impl Default for TodoBook {
    fn default() -> Self {
        TodoBook { version: VERSION, rev: 0, boards: Vec::new(), todos: Vec::new(), seq: BTreeMap::new(), settings: TodoSettings::default() }
    }
}
/// A prefix: 2 to 5 uppercase ASCII letters.
pub fn valid_prefix(prefix: &str) -> bool {
    (2..=5).contains(&prefix.len()) && prefix.bytes().all(|b| b.is_ascii_uppercase())
}

/// `DEC-031` → (`DEC`, 31); case-insensitive (`dec-31` too).
pub fn parse_id(text: &str) -> Option<(String, u32)> {
    let (prefix, number) = text.trim().rsplit_once('-')?;
    let prefix = prefix.to_ascii_uppercase();
    if !valid_prefix(&prefix) || number.is_empty() || number.len() > 9 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((prefix, number.parse().ok()?))
}

/// An id as the book writes it: `<PREFIX>-<n, at least 3 digits>`.
pub fn format_id(prefix: &str, number: u32) -> String {
    format!("{prefix}-{number:03}")
}

/// The same todo id (`dec-31` is `DEC-031`).
pub fn same_id(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim()) || matches!((parse_id(a), parse_id(b)), (Some(x), Some(y)) if x == y)
}

/// Lowercase words joined by `-` (board ids).
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').chars().take(40).collect::<String>();
    if out.is_empty() { "board".into() } else { out.trim_end_matches('-').to_owned() }
}

/// The digits of ranks, in ASCII (and so string) order.
const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn digit(byte: u8) -> Option<usize> {
    DIGITS.iter().position(|d| *d == byte)
}

/// A rank: base-62 digits, not empty, not ending in `0` (so there is always room before it).
pub fn valid_rank(rank: &str) -> bool {
    !rank.is_empty() && !rank.ends_with('0') && rank.bytes().all(|b| digit(b).is_some())
}

/// A fractional index strictly between `a` and `b` (`None`: the start or
/// the end of the column); ranks compare as strings. Invalid bounds are
/// ignored (`b` not above `a` too), so the answer is always a valid rank.
/// Appending and prepending step by one digit, so a column grows a digit
/// every 30 cards or so (the book spreads long columns again).
pub fn rank_between(a: Option<&str>, b: Option<&str>) -> String {
    let a = a.filter(|r| valid_rank(r)).unwrap_or("");
    let b = b.filter(|r| valid_rank(r) && *r > a);
    match (a.is_empty(), b) {
        (true, None) => midpoint("", None),
        // After the last: the shortest prefix of `a` whose last digit can grow, grown by one.
        (false, None) => match a.bytes().position(|c| digit(c).is_some_and(|d| d + 1 < DIGITS.len())) {
            Some(i) => format!("{}{}", &a[..i], DIGITS[digit(a.as_bytes()[i]).unwrap_or(0) + 1] as char),
            None => midpoint(a, None),
        },
        // Before the first: the shortest prefix of `b` whose last digit can shrink (not to zero).
        (true, Some(b)) => match b.bytes().position(|c| digit(c).is_some_and(|d| d > 1)) {
            Some(i) => format!("{}{}", &b[..i], DIGITS[digit(b.as_bytes()[i]).unwrap_or(2) - 1] as char),
            None => midpoint("", Some(b)),
        },
        (false, Some(b)) => midpoint(a, Some(b)),
    }
}

fn midpoint(a: &str, b: Option<&str>) -> String {
    if let Some(b) = b {
        // The common prefix (`a` padded with zeros) stays.
        let n = b.bytes().zip(a.bytes().chain(std::iter::repeat(b'0'))).take_while(|(x, y)| x == y).count();
        if n > 0 {
            return format!("{}{}", &b[..n], midpoint(a.get(n..).unwrap_or(""), Some(&b[n..])));
        }
    }
    let da = a.bytes().next().and_then(digit).unwrap_or(0);
    let db = b.and_then(|b| b.bytes().next()).and_then(digit).unwrap_or(DIGITS.len());
    if db > da + 1 {
        return (DIGITS[(da + db).div_ceil(2)] as char).to_string();
    }
    match b {
        Some(b) if b.len() > 1 => b[..1].to_owned(),
        _ => format!("{}{}", DIGITS[da] as char, midpoint(a.get(1..).unwrap_or(""), None)),
    }
}

/// `n` short ranks spread evenly (imports, and columns whose ranks grew long).
pub fn rank_sequence(n: usize) -> Vec<String> {
    let base = DIGITS.len() as u128;
    let (mut len, mut span) = (1usize, base);
    while span < (n as u128 + 1) * 2 {
        span *= base;
        len += 1;
    }
    (1..=n as u128)
        .map(|i| {
            let mut value = span * i / (n as u128 + 1);
            let mut digits = vec![b'0'; len];
            for slot in digits.iter_mut().rev() {
                *slot = DIGITS[(value % base) as usize];
                value /= base;
            }
            String::from_utf8(digits).unwrap_or_default().trim_end_matches('0').to_owned()
        })
        .collect()
}

/// A group's todos for its title chip (`☑ 4 · 2▸ · 1◇`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TodoCounts {
    /// Accepted todos not finished (backlog, todo, in progress, in review).
    pub open: usize,
    /// Of those, in progress.
    pub in_progress: usize,
    /// Proposals waiting for the user.
    pub proposed: usize,
}

/// One column of a board ([`TodoBook::board_view`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Column<'a> {
    pub status: Status,
    /// The board's name for it.
    pub name: String,
    /// Per lane (as [`BoardView::lanes`]): its todos in rank order.
    pub cells: Vec<Vec<&'a Todo>>,
    pub count: usize,
    /// In progress: the board's WIP limit, and whether it is over it (gold).
    pub wip: Option<u32>,
    pub over: bool,
    /// Done: drawn collapsed to its count.
    pub collapsed: bool,
}

/// A board as the todo canvas draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct BoardView<'a> {
    pub board: &'a Board,
    /// The board itself, then its sub-boards (the swimlanes).
    pub lanes: Vec<&'a Board>,
    /// Proposed (when any, or the policy makes agents' todos proposals),
    /// Backlog, Todo, In progress, In review, Done.
    pub columns: Vec<Column<'a>>,
    /// Canceled todos (a count in the header only).
    pub canceled: usize,
    /// Open todos that are blocked.
    pub blocked: usize,
}

impl BoardView<'_> {
    /// (done, accepted and not canceled) for the progress bar.
    pub fn progress(&self) -> (usize, usize) {
        let count = |s: Status| self.columns.iter().find(|c| c.status == s).map_or(0, |c| c.count);
        let done = count(Status::Done);
        (done, done + [Status::Backlog, Status::Todo, Status::InProgress, Status::InReview].into_iter().map(count).sum::<usize>())
    }
}

/// Why a todo waits for the user ([`TodoBook::needs_you`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// A proposal: accept or decline it.
    Proposed,
    /// Submitted for review.
    InReview,
    /// Claimed by a session that is gone or stopped.
    StaleClaim,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NeedsYou<'a> {
    pub todo: &'a Todo,
    pub need: Need,
}

/// Rank order within a column (then creation, then id).
fn by_rank(a: &&Todo, b: &&Todo) -> std::cmp::Ordering {
    a.rank.cmp(&b.rank).then(a.created_ms.cmp(&b.created_ms)).then(a.id.cmp(&b.id))
}
impl TodoBook {
    pub fn board(&self, id: &str) -> Option<&Board> {
        self.boards.iter().find(|b| b.id == id)
    }

    /// A board by id, name (case-insensitive; `Parent/Child` for a
    /// sub-board) or, for top-level boards, prefix.
    pub fn find_board(&self, text: &str) -> Option<&Board> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Some(board) = self.board(text) {
            return Some(board);
        }
        if let Some((parent, child)) = text.split_once('/')
            && let Some(parent) = self.find_board(parent)
        {
            return self.boards.iter().find(|b| b.parent.as_deref() == Some(parent.id.as_str()) && b.name.eq_ignore_ascii_case(child.trim()));
        }
        let top = |b: &&Board| b.parent.is_none();
        self.boards
            .iter()
            .filter(top)
            .find(|b| b.name.eq_ignore_ascii_case(text))
            .or_else(|| self.boards.iter().find(|b| b.name.eq_ignore_ascii_case(text)))
            .or_else(|| self.boards.iter().filter(top).find(|b| b.prefix.eq_ignore_ascii_case(text)))
    }

    /// The top-level board of a board (itself, or its parent).
    pub fn root(&self, board: &str) -> Option<&Board> {
        let found = self.board(board)?;
        match &found.parent {
            Some(parent) => self.board(parent).or(Some(found)),
            None => Some(found),
        }
    }

    /// A board's sub-boards (its swimlanes), in book order.
    pub fn sub_boards(&self, board: &str) -> Vec<&Board> {
        self.boards.iter().filter(|b| b.parent.as_deref() == Some(board)).collect()
    }

    /// A board and its sub-boards (ids).
    pub fn lane_ids(&self, board: &str) -> Vec<String> {
        let mut ids = vec![board.to_owned()];
        ids.extend(self.sub_boards(board).into_iter().map(|b| b.id.clone()));
        ids
    }

    /// A todo by id (`dec-31` finds `DEC-031`).
    pub fn todo(&self, id: &str) -> Option<&Todo> {
        self.todos.iter().find(|t| t.id.eq_ignore_ascii_case(id.trim())).or_else(|| self.todos.iter().find(|t| same_id(&t.id, id)))
    }

    /// The todos of a board and its sub-boards.
    pub fn todos_in<'a>(&'a self, board: &str) -> impl Iterator<Item = &'a Todo> + 'a {
        let lanes = self.lane_ids(board);
        self.todos.iter().filter(move |t| lanes.contains(&t.board))
    }

    /// The open todos it waits for (`BlockedBy` relations to todos not done or canceled).
    pub fn blockers(&self, todo: &Todo) -> Vec<&Todo> {
        todo.blocked_by().filter_map(|id| self.todo(id)).filter(|t| !t.status.closed()).collect()
    }

    /// Blocked: a `blocked` reason, or an open todo it is blocked by.
    pub fn is_blocked(&self, todo: &Todo) -> bool {
        todo.blocked.as_deref().is_some_and(|b| !b.trim().is_empty()) || !self.blockers(todo).is_empty()
    }

    /// Open todos blocked by this one (the card's `blocks N`).
    pub fn blocking(&self, id: &str) -> Vec<&Todo> {
        self.todos.iter().filter(|t| !t.status.closed() && t.blocked_by().any(|b| same_id(b, id))).collect()
    }

    /// Ready to be worked on: status todo, not blocked, not claimed.
    pub fn is_ready(&self, todo: &Todo) -> bool {
        todo.status == Status::Todo && todo.claimed_by.is_none() && !self.is_blocked(todo)
    }

    /// The ready todos (of a board and its sub-boards, or all), by priority, then rank.
    pub fn ready(&self, board: Option<&str>) -> Vec<&Todo> {
        let lanes = board.map(|b| self.lane_ids(b));
        let mut ready: Vec<&Todo> = self.todos.iter().filter(|t| lanes.as_ref().is_none_or(|l| l.contains(&t.board)) && self.is_ready(t)).collect();
        ready.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| by_rank(a, b)));
        ready
    }

    /// The ready todos of the boards linked to a group (all boards when none is).
    pub fn ready_for_group(&self, group: Option<&str>) -> Vec<&Todo> {
        let ids = group.map(|g| self.group_board_ids(g)).filter(|ids| !ids.is_empty());
        let mut ready: Vec<&Todo> = self.todos.iter().filter(|t| ids.as_ref().is_none_or(|ids| ids.contains(&t.board)) && self.is_ready(t)).collect();
        ready.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| by_rank(a, b)));
        ready
    }

    /// Boards linked to a canvas group (any level), in book order.
    pub fn boards_of_group(&self, group: &str) -> Vec<&Board> {
        self.boards.iter().filter(|b| b.canvas_groups.iter().any(|g| g == group)).collect()
    }

    /// Where a group's sessions add todos: the first board linked to it.
    pub fn default_board(&self, group: Option<&str>) -> Option<&Board> {
        group.and_then(|g| self.boards_of_group(g).into_iter().next())
    }

    /// The boards a group sees: those linked to it and their sub-boards.
    pub fn group_board_ids(&self, group: &str) -> BTreeSet<String> {
        self.boards_of_group(group).into_iter().flat_map(|b| self.lane_ids(&b.id)).collect()
    }

    /// A group's chip: `None` when no board is linked to it.
    pub fn group_counts(&self, group: &str) -> Option<TodoCounts> {
        let ids = self.group_board_ids(group);
        if ids.is_empty() {
            return None;
        }
        let mut counts = TodoCounts::default();
        for todo in self.todos.iter().filter(|t| ids.contains(&t.board)) {
            counts.open += usize::from(todo.status.open());
            counts.in_progress += usize::from(todo.status == Status::InProgress);
            counts.proposed += usize::from(todo.status == Status::Proposed);
        }
        Some(counts)
    }

    /// The todo a session claimed (not closed; its newest claim), for `☑ DEC-031` on its box.
    pub fn claimed_by_session(&self, session: &str) -> Option<&Todo> {
        self.todos.iter().filter(|t| !t.status.closed() && t.claim_session() == Some(session)).max_by_key(|t| t.claimed_by.as_ref().map_or(0, |c| c.ms))
    }

    /// The canvas group a board's events go to: its first linked group, else its parent's.
    pub fn event_group(&self, board: &str) -> Option<&str> {
        let found = self.board(board)?;
        found.canvas_groups.first().or_else(|| found.parent.as_deref().and_then(|p| self.board(p)).and_then(|p| p.canvas_groups.first())).map(String::as_str)
    }

    /// Todos in progress on a board and its sub-boards.
    pub fn in_progress(&self, board: &str) -> usize {
        self.todos_in(board).filter(|t| t.status == Status::InProgress).count()
    }

    /// A top-level board as the todo canvas draws it (a sub-board: its parent's view).
    pub fn board_view(&self, board: &str) -> Option<BoardView<'_>> {
        let root = self.root(board)?;
        let mut lanes = vec![root];
        lanes.extend(self.sub_boards(&root.id));
        let todos: Vec<&Todo> = self.todos_in(&root.id).collect();
        let any = |s: Status| todos.iter().any(|t| t.status == s);
        let mut statuses = Vec::new();
        if any(Status::Proposed) || root.policy.agent_adds == AddPolicy::Proposed {
            statuses.push(Status::Proposed);
        }
        statuses.extend([Status::Backlog, Status::Todo, Status::InProgress, Status::InReview, Status::Done]);
        let columns = statuses
            .into_iter()
            .map(|status| {
                let cells: Vec<Vec<&Todo>> = lanes
                    .iter()
                    .map(|lane| {
                        let mut cell: Vec<&Todo> = todos.iter().copied().filter(|t| t.status == status && t.board == lane.id).collect();
                        cell.sort_by(by_rank);
                        cell
                    })
                    .collect();
                let count = cells.iter().map(Vec::len).sum();
                let wip = (status == Status::InProgress).then_some(root.wip).flatten();
                Column { status, name: root.status_name(status), cells, count, wip, over: wip.is_some_and(|w| count > w as usize), collapsed: status == Status::Done }
            })
            .collect();
        Some(BoardView {
            board: root,
            lanes,
            columns,
            canceled: todos.iter().filter(|t| t.status == Status::Canceled).count(),
            blocked: todos.iter().filter(|t| !t.status.closed() && self.is_blocked(t)).count(),
        })
    }

    /// What waits for the user: proposals (oldest first), todos in review,
    /// then claims by sessions `gone` says are gone or stopped.
    pub fn needs_you(&self, gone: impl Fn(&str) -> bool) -> Vec<NeedsYou<'_>> {
        let mut out: Vec<NeedsYou> = Vec::new();
        let mut proposed: Vec<&Todo> = self.todos.iter().filter(|t| t.status == Status::Proposed).collect();
        proposed.sort_by_key(|t| (t.proposal.as_ref().map_or(t.created_ms, |p| p.ms), t.created_ms));
        out.extend(proposed.into_iter().map(|todo| NeedsYou { todo, need: Need::Proposed }));
        out.extend(self.todos.iter().filter(|t| t.status == Status::InReview).map(|todo| NeedsYou { todo, need: Need::InReview }));
        out.extend(self.todos.iter().filter(|t| !t.status.closed() && t.claim_session().is_some_and(&gone)).map(|todo| NeedsYou { todo, need: Need::StaleClaim }));
        out
    }

    /// A top-level board already uses this prefix (other than `except`).
    pub fn prefix_taken(&self, prefix: &str, except: Option<&str>) -> bool {
        self.boards.iter().any(|b| b.parent.is_none() && Some(b.id.as_str()) != except && b.prefix.eq_ignore_ascii_case(prefix))
    }

    /// A free prefix for a new board from its name: initials of several
    /// words, else its first 3 to 5 letters (`Decomps` → `DEC`).
    pub fn suggest_prefix(&self, name: &str) -> String {
        let words: Vec<String> = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .map(|w| w.chars().filter(char::is_ascii_alphabetic).collect::<String>().to_ascii_uppercase())
            .filter(|w| !w.is_empty())
            .collect();
        let letters: String = words.concat();
        let mut candidates: Vec<String> = Vec::new();
        if words.len() >= 2 {
            let initials: String = words.iter().filter_map(|w| w.chars().next()).take(5).collect();
            candidates.push(initials);
        }
        for n in 3..=5 {
            if letters.len() >= n {
                candidates.push(letters[..n].to_owned());
            }
        }
        if letters.len() == 2 {
            candidates.push(letters.clone());
        }
        let stem: String = letters.chars().chain("TD".chars()).take(2).collect();
        for c in b'A'..=b'Z' {
            candidates.push(format!("{stem}{}", c as char));
        }
        for a in b'A'..=b'Z' {
            for b in b'A'..=b'Z' {
                candidates.push(format!("{stem}{}{}", a as char, b as char));
            }
        }
        candidates.into_iter().find(|p| valid_prefix(p) && !self.prefix_taken(p, None)).unwrap_or_else(|| "TODO".into())
    }

    /// The next id of a prefix (never one given out before, never one in
    /// use); an error once the prefix's numbers ([`MAX_SEQ`]) are used up.
    pub fn next_id(&mut self, prefix: &str) -> Result<String, String> {
        let mut number = self.seq.get(prefix).copied().unwrap_or(0);
        loop {
            number = number.checked_add(1).filter(|n| *n <= MAX_SEQ).ok_or_else(|| format!("the prefix {prefix} has used all its numbers: give the board another prefix"))?;
            let id = format_id(prefix, number);
            if self.todo(&id).is_none() {
                self.seq.insert(prefix.to_owned(), number);
                return Ok(id);
            }
        }
    }

    /// Records ids that came from elsewhere (imports) so new ones come after them.
    pub fn note_ids(&mut self, ids: &[String]) {
        for (prefix, number) in ids.iter().filter_map(|id| parse_id(id)) {
            let seq = self.seq.entry(prefix).or_insert(0);
            *seq = (*seq).max(number);
        }
    }
}
