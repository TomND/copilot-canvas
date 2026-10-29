//! OpenCode session history from its SQLite database. The database can be
//! hundreds of gigabytes: it is opened read-only and only indexed lookups
//! run (sessions by parent, messages by session, parts by message). The
//! event sequence number of a session tells cheaply whether anything changed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use crate::paths::now_ms;
use crate::protocol::{Activity, AgentStatus, AgentSummary, SessionSummary};

use super::opencode::ServerState;
use super::opencode_server::Live;

const LIST_LIMIT: usize = 300;
const MAX_AGENTS: usize = 12;
const RECENT_MESSAGES: usize = 4;

/// OpenCode's database: `OPENCODE_DB` (absolute, or relative to the data
/// folder), else `<data>/opencode.db` with data = `XDG_DATA_HOME/opencode`
/// or `~/.local/share/opencode`.
pub fn db_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| super::home().join(".local").join("share")).join("opencode");
    match std::env::var_os("OPENCODE_DB").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => data.join(path),
        None => data.join("opencode.db"),
    }
}

/// Opens the database read-only, by plain path (no URI parsing).
pub fn open(path: &Path) -> Result<Connection> {
    anyhow::ensure!(path.is_file(), "{} does not exist", path.display());
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("cannot open {}", path.display()))?;
    connection.busy_timeout(Duration::from_millis(200))?;
    connection.pragma_update(None, "query_only", 1)?;
    Ok(connection)
}

/// An index on `table` whose leading columns are `columns`.
fn has_index(connection: &Connection, table: &str, columns: &[&str]) -> bool {
    let list = |sql: String| -> Vec<String> {
        let Ok(mut statement) = connection.prepare(&sql) else { return Vec::new() };
        let rows = statement.query_map([], |r| r.get::<_, Option<String>>("name"));
        rows.map(|rows| rows.filter_map(|r| r.ok().flatten()).collect()).unwrap_or_default()
    };
    list(format!("PRAGMA index_list({table})")).iter().any(|name| {
        let cols = list(format!("PRAGMA index_info(\"{name}\")"));
        cols.len() >= columns.len() && cols.iter().zip(columns).all(|(a, b)| a == b)
    })
}

fn has_table(connection: &Connection, table: &str) -> bool {
    connection.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |_| Ok(())).is_ok()
}

/// Column names of `table`.
fn columns(connection: &Connection, table: &str) -> HashSet<String> {
    let Ok(mut statement) = connection.prepare(&format!("PRAGMA table_info({table})")) else { return HashSet::new() };
    let rows = statement.query_map([], |r| r.get::<_, String>("name"));
    rows.map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
}

/// What the open database supports, checked once per connection.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Schema {
    /// `SELECT` list of a session [`Row`]; missing optional columns read as defaults.
    row: String,
    /// `AND time_archived IS NULL` when the column exists.
    not_archived: &'static str,
    /// The root-session list can use an index.
    list: bool,
    /// Messages and parts can be read by index.
    pub(crate) indexed: bool,
    /// Event sequence numbers can be looked up by index.
    seq: bool,
}

impl Schema {
    pub(crate) fn read(connection: &Connection) -> Self {
        let session = columns(connection, "session");
        let column = |name: &str, default: &str| if session.contains(name) { name.to_owned() } else { format!("{default} AS {name}") };
        let row = [("parent_id", "NULL"), ("directory", "''"), ("title", "''"), ("agent", "NULL"), ("time_created", "0"), ("time_updated", "0")]
            .iter()
            .fold("id".to_owned(), |list, (name, default)| format!("{list}, {}", column(name, default)));
        Schema {
            row,
            not_archived: if session.contains("time_archived") { " AND time_archived IS NULL" } else { "" },
            list: has_index(connection, "session", &["parent_id"]) || has_index(connection, "session", &["time_updated"]),
            indexed: has_index(connection, "message", &["session_id", "time_created"])
                && has_index(connection, "part", &["message_id"])
                && has_index(connection, "session", &["parent_id"]),
            seq: has_table(connection, "event_sequence") && has_index(connection, "event_sequence", &["aggregate_id"]),
        }
    }
}
fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

/// The first top-level session created in `cwd` at or after `since_ms`: the
/// one a freshly launched TUI made for its first prompt.
pub fn discover_new_session(cwd: &str, since_ms: u64) -> Option<String> {
    discover_in(&open(&db_path()).ok()?, cwd, since_ms)
}

/// Every top-level session created in `cwd` at or after `since_ms`, oldest
/// first: (id, created ms).
pub fn discover_new_sessions(cwd: &str, since_ms: u64) -> Vec<(String, u64)> {
    open(&db_path()).map(|c| new_sessions_in(&c, cwd, since_ms)).unwrap_or_default()
}

pub fn discover_in(connection: &Connection, cwd: &str, since_ms: u64) -> Option<String> {
    new_sessions_in(connection, cwd, since_ms).into_iter().next().map(|(id, _)| id)
}

fn new_sessions_in(connection: &Connection, cwd: &str, since_ms: u64) -> Vec<(String, u64)> {
    let Ok(mut statement) = connection
        .prepare_cached("SELECT id, directory, time_created FROM session WHERE parent_id IS NULL AND time_created >= ?1 ORDER BY time_created ASC, id ASC")
    else {
        return Vec::new();
    };
    let row = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?.max(0) as u64));
    let Ok(rows) = statement.query_map([since_ms as i64], row) else { return Vec::new() };
    rows.filter_map(|r| r.ok()).filter(|(_, dir, _)| same_dir(dir, cwd)).map(|(id, _, created)| (id, created)).collect()
}

/// Number of top-level sessions in the database (for `doctor`).
pub fn root_session_count(path: &Path) -> Option<u64> {
    open(path).ok()?.query_row("SELECT count(*) FROM session WHERE parent_id IS NULL", [], |r| r.get::<_, i64>(0)).ok().map(|n| n.max(0) as u64)
}

/// The session exists in the database.
pub fn session_exists(path: &Path, id: &str) -> bool {
    open(path).is_ok_and(|c| c.query_row("SELECT 1 FROM session WHERE id = ?1", [id], |_| Ok(())).is_ok())
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Row {
    id: String,
    parent: Option<String>,
    directory: String,
    title: String,
    agent: String,
    created_ms: u64,
    updated_ms: u64,
}

/// A column as text when it holds text (lenient about types and NULL).
pub(crate) fn text(r: &rusqlite::Row, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(r.get_ref(index)?.as_str().ok().map(str::to_owned))
}

/// A column as a non-negative integer when it holds a number.
pub(crate) fn number(r: &rusqlite::Row, index: usize) -> rusqlite::Result<Option<u64>> {
    Ok(match r.get_ref(index)? {
        ValueRef::Integer(n) => Some(n.max(0) as u64),
        ValueRef::Real(n) if n.is_finite() => Some(n.max(0.0) as u64),
        _ => None,
    })
}

/// A session row selected with [`Schema::row`].
fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        parent: text(r, 1)?,
        directory: text(r, 2)?.unwrap_or_default(),
        title: text(r, 3)?.unwrap_or_default(),
        agent: text(r, 4)?.unwrap_or_default(),
        created_ms: number(r, 5)?.unwrap_or(0),
        updated_ms: number(r, 6)?.unwrap_or(0),
    })
}
/// One message (`message.data` holds the JSON).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Msg {
    pub id: String,
    pub role: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub completed_ms: Option<u64>,
    pub finish: String,
    pub error: String,
}

impl Msg {
    pub fn parse(id: String, updated_ms: u64, data: &str) -> Self {
        let v: Value = serde_json::from_str(data).unwrap_or_default();
        Msg {
            id,
            role: v["role"].as_str().unwrap_or_default().to_owned(),
            created_ms: v["time"]["created"].as_u64().unwrap_or(0),
            updated_ms,
            completed_ms: v["time"]["completed"].as_u64(),
            finish: v["finish"].as_str().unwrap_or_default().to_owned(),
            error: v["error"]["name"].as_str().map(str::to_owned).or_else(|| v["error"].is_object().then(|| "Error".to_owned())).unwrap_or_default(),
        }
    }
}

/// Status from the newest messages (newest first) and the newest message's parts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Derived {
    pub activity: Activity,
    pub finished_ms: Option<u64>,
    /// Questions waiting for an answer.
    pub pending: usize,
    pub last_event_ms: u64,
    /// The last turn was stopped by the user.
    pub aborted: bool,
}

/// The fields of a part that status and event detection look at, read with
/// `json_extract` so polls never load whole part blobs (tool output can be large).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartInfo {
    pub id: String,
    /// `type`: "tool", "text", ...
    pub kind: String,
    pub tool: String,
    /// `state.status`: "pending", "running", "completed", "error".
    pub status: String,
    /// `state.time.end`.
    pub end_ms: u64,
}

pub fn derive(messages: &[Msg], parts: &[PartInfo]) -> Derived {
    let mut out = Derived { last_event_ms: messages.iter().map(|m| m.updated_ms.max(m.created_ms)).max().unwrap_or(0), ..Default::default() };
    let Some(latest) = messages.first() else {
        out.activity = Activity::Idle;
        return out;
    };
    match latest.role.as_str() {
        "assistant" if latest.error == "MessageAbortedError" => {
            out.activity = Activity::Idle;
            out.aborted = true;
        }
        "assistant" if !latest.error.is_empty() => out.activity = Activity::Failed,
        "assistant" if latest.completed_ms.is_none() || latest.finish == "tool-calls" => out.activity = Activity::Working,
        "assistant" => {
            out.activity = Activity::Idle;
            out.finished_ms = latest.completed_ms;
        }
        _ => out.activity = Activity::Working,
    }
    for part in parts.iter().filter(|p| p.kind == "tool") {
        if matches!(part.status.as_str(), "pending" | "running") {
            if out.activity != Activity::Failed {
                out.activity = Activity::Working;
                out.finished_ms = None;
            }
            if part.tool == "question" {
                out.pending += 1;
            }
        }
    }
    out
}

fn agent_status(derived: &Derived) -> AgentStatus {
    match derived.activity {
        Activity::Working => AgentStatus::Working,
        Activity::Failed => AgentStatus::Failed,
        Activity::Idle if derived.aborted => AgentStatus::Unknown,
        Activity::Idle => AgentStatus::Finished,
        _ => AgentStatus::Unknown,
    }
}

/// "Fix X (@general subagent)" → ("general", "Fix X").
fn split_subagent_title(title: &str, agent: &str) -> (String, String) {
    if let Some(start) = title.rfind(" (@")
        && let Some(name) = title[start + 3..].strip_suffix(" subagent)")
    {
        return (if agent.is_empty() { name.to_owned() } else { agent.to_owned() }, title[..start].to_owned());
    }
    (if agent.is_empty() { "Subagent".to_owned() } else { agent.to_owned() }, title.to_owned())
}

/// Tool parts that may become git/PR events (their full data is read once).
fn watched_tool(tool: &str) -> bool {
    tool == "bash" || tool.contains("pull_request")
}

/// Tool calls that may become git/PR events.
fn detect_part(part: &Value) -> Option<(String, Value)> {
    let state = &part["state"];
    let tool = part["tool"].as_str().unwrap_or_default();
    let output = state["output"].as_str().unwrap_or_default();
    if tool == "bash" {
        let command = state["input"]["command"].as_str().unwrap_or_default();
        let lower = command.to_ascii_lowercase();
        if !(lower.contains("git push") || lower.contains("gh pr ") || lower.contains("az repos pr ")) {
            return None;
        }
        if state["metadata"]["exit"].as_i64().is_some_and(|code| code != 0) {
            return None;
        }
        return crate::index::detect("bash", &serde_json::json!({ "command": command }), output);
    }
    if tool.contains("pull_request") {
        return crate::index::detect(tool, &state["input"], output);
    }
    None
}

struct Tracked {
    seq: Option<i64>,
    refreshed: bool,
    derived: Derived,
    agents: Vec<AgentSummary>,
    /// Subagent sessions: sequence number when last read, and their summary.
    children: HashMap<String, (Option<i64>, AgentSummary)>,
    since_ms: u64,
    last_refresh_ms: u64,
    reported: VecDeque<String>,
    detected: Vec<(String, Value)>,
    server_busy: bool,
    server_finished: Option<u64>,
}

impl Tracked {
    fn new(now: u64) -> Self {
        Self {
            seq: None,
            refreshed: false,
            derived: Derived::default(),
            agents: Vec::new(),
            children: HashMap::new(),
            since_ms: now.saturating_sub(5_000),
            last_refresh_ms: 0,
            reported: VecDeque::new(),
            detected: Vec::new(),
            server_busy: false,
            server_finished: None,
        }
    }
}

struct Entry {
    row: Row,
    tracked: Option<Tracked>,
    sent: Option<SessionSummary>,
}

/// What the running TUIs' servers say about a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    pub busy: bool,
    pub waiting: usize,
    pub live: bool,
}

/// OpenCode's sessions as a [`super::History`].
pub struct OpenCodeHistory {
    path: PathBuf,
    connection: Option<Connection>,
    schema: Schema,
    retry_at: Option<Instant>,
    entries: HashMap<String, Entry>,
    parents: HashMap<String, Option<String>>,
    git_roots: HashMap<String, String>,
    last_full_scan: Option<Instant>,
    /// Warnings already given once (missing indexes): not repeated on reconnects.
    noted: HashSet<String>,
    /// Ask the canvas-started TUIs' servers for live state.
    pub use_servers: bool,
    pub warnings: Vec<String>,
}

/// Wait after a failed open before trying again.
const OPEN_BACKOFF: Duration = Duration::from_secs(10);
/// Wait after a failed query (schema change, locked database) before reopening.
const QUERY_BACKOFF: Duration = Duration::from_secs(30);
/// Live server state older than this is ignored.
const LIVE_FRESH_MS: u64 = 5_000;

impl OpenCodeHistory {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            connection: None,
            schema: Schema::default(),
            retry_at: None,
            entries: HashMap::new(),
            parents: HashMap::new(),
            git_roots: HashMap::new(),
            last_full_scan: None,
            noted: HashSet::new(),
            use_servers: true,
            warnings: Vec::new(),
        }
    }

    fn connect(&mut self) -> bool {
        if self.connection.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        match open(&self.path) {
            Ok(connection) => {
                self.schema = Schema::read(&connection);
                if !self.schema.list {
                    self.note_once("opencode: the session table lacks an index on parent_id; only tracked sessions are listed".into());
                }
                if !self.schema.indexed {
                    self.note_once("opencode: the database lacks the expected indexes; only listing sessions".into());
                }
                if !self.schema.seq {
                    self.note_once("opencode: no indexed event_sequence table; tracked sessions are re-read every poll".into());
                }
                self.connection = Some(connection);
                self.retry_at = None;
                true
            }
            Err(error) => {
                if self.path.exists() {
                    self.warn(format!("opencode: {error:#}"));
                }
                self.retry_at = Some(Instant::now() + OPEN_BACKOFF);
                false
            }
        }
    }

    pub fn poll(&mut self, tracked: &HashSet<String>, full_every: Duration) -> (Vec<SessionSummary>, Vec<String>) {
        if !self.connect() {
            return (Vec::new(), Vec::new());
        }
        match self.poll_inner(tracked, full_every) {
            Ok(result) => result,
            Err(error) => {
                // Reopen after a pause (schema change, locked database, …), not every poll.
                self.warn(format!("opencode: {error:#}"));
                self.connection = None;
                self.retry_at = Some(Instant::now() + QUERY_BACKOFF);
                (Vec::new(), Vec::new())
            }
        }
    }

    fn poll_inner(&mut self, tracked: &HashSet<String>, full_every: Duration) -> Result<(Vec<SessionSummary>, Vec<String>)> {
        let now = now_ms();
        let full = self.last_full_scan.is_none_or(|t| t.elapsed() >= full_every);
        let mut seen = HashSet::new();
        {
            let connection = self.connection.as_ref().context("not connected")?;
            let schema = &self.schema;
            if full {
                self.last_full_scan = Some(Instant::now());
                if schema.list {
                    let mut statement = connection.prepare_cached(&format!(
                        "SELECT {} FROM session WHERE parent_id IS NULL{} ORDER BY time_updated DESC LIMIT {LIST_LIMIT}",
                        schema.row, schema.not_archived
                    ))?;
                    for found in statement.query_map([], row)? {
                        let found = found?;
                        seen.insert(found.id.clone());
                        upsert(&mut self.entries, found);
                    }
                }
            }
            // Tracked sessions are read every time (a primary-key lookup).
            let mut statement = connection.prepare_cached(&format!("SELECT {} FROM session WHERE id = ?1", schema.row))?;
            let missing: Vec<&String> = tracked.iter().filter(|id| super::opencode::is_session_id(id) && !seen.contains(*id)).collect();
            for id in missing {
                if let Some(found) = statement.query_row([id], row).optional()? {
                    seen.insert(found.id.clone());
                    upsert(&mut self.entries, found);
                }
            }
        }
        let mut removed = Vec::new();
        if full {
            let gone: Vec<String> = self.entries.keys().filter(|id| !seen.contains(*id) && !tracked.contains(*id)).cloned().collect();
            for id in gone {
                self.entries.remove(&id);
                removed.push(id);
            }
        }
        let overlay = if self.use_servers { self.server_overlay(now)? } else { HashMap::new() };
        let mut changed = Vec::new();
        let ids: Vec<String> = self.entries.keys().cloned().collect();
        for id in ids {
            let is_tracked = tracked.contains(&id);
            if is_tracked {
                self.refresh(&id, now)?;
            } else if let Some(entry) = self.entries.get_mut(&id) {
                entry.tracked = None;
            }
            let cwd = self.entries[&id].row.directory.replace('/', "\\");
            let git_root = self.git_roots.entry(cwd.clone()).or_insert_with(|| super::claude_history::find_git_root(Path::new(&cwd))).clone();
            let entry = self.entries.get_mut(&id).expect("entry exists");
            let summary = summarize(entry, is_tracked, overlay.get(&id), git_root, now);
            if entry.sent.as_ref() != Some(&summary) {
                entry.sent = Some(summary.clone());
                changed.push(summary);
            }
        }
        Ok((changed, removed))
    }

    /// Detected git/PR actions per session since the last call.
    pub fn take_detected(&mut self) -> Vec<(String, String, Value)> {
        let mut out = Vec::new();
        for (id, entry) in &mut self.entries {
            if let Some(state) = &mut entry.tracked {
                for (kind, data) in state.detected.drain(..) {
                    out.push((id.clone(), kind, data));
                }
            }
        }
        out
    }

    fn seq(&self, id: &str) -> Result<Option<i64>> {
        if !self.schema.seq {
            return Ok(None);
        }
        let connection = self.connection.as_ref().context("not connected")?;
        Ok(connection.prepare_cached("SELECT seq FROM event_sequence WHERE aggregate_id = ?1")?.query_row([id], |r| r.get(0)).optional()?)
    }

    /// The newest messages of a session, newest first (only the fields status needs).
    fn messages(&self, session: &str, limit: usize) -> Result<Vec<Msg>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement = connection.prepare_cached(
            "SELECT id, time_updated, json_extract(d, '$.role'), json_extract(d, '$.time.created'), json_extract(d, '$.time.completed'), \
             json_extract(d, '$.finish'), json_extract(d, '$.error.name'), json_type(d, '$.error') \
             FROM (SELECT id, time_created, time_updated, CASE WHEN json_valid(data) THEN data ELSE '{}' END AS d \
                   FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2) \
             ORDER BY time_created DESC, id DESC",
        )?;
        let rows = statement.query_map(params![session, limit as i64], |r| {
            let error = match (text(r, 6)?, text(r, 7)?.as_deref()) {
                (Some(name), _) => name,
                (None, Some("object")) => "Error".to_owned(),
                _ => String::new(),
            };
            Ok(Msg {
                id: r.get(0)?,
                updated_ms: number(r, 1)?.unwrap_or(0),
                role: text(r, 2)?.unwrap_or_default(),
                created_ms: number(r, 3)?.unwrap_or(0),
                completed_ms: number(r, 4)?,
                finish: text(r, 5)?.unwrap_or_default(),
                error,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn parts(&self, message: &str) -> Result<Vec<PartInfo>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement = connection.prepare_cached(
            "SELECT id, json_extract(d, '$.type'), json_extract(d, '$.tool'), json_extract(d, '$.state.status'), json_extract(d, '$.state.time.end') \
             FROM (SELECT id, CASE WHEN json_valid(data) THEN data ELSE '{}' END AS d FROM part WHERE message_id = ?1) ORDER BY id",
        )?;
        let rows = statement.query_map([message], |r| {
            Ok(PartInfo {
                id: r.get(0)?,
                kind: text(r, 1)?.unwrap_or_default(),
                tool: text(r, 2)?.unwrap_or_default(),
                status: text(r, 3)?.unwrap_or_default(),
                end_ms: number(r, 4)?.unwrap_or(0),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The full JSON of one part.
    fn part_data(&self, id: &str) -> Result<Value> {
        let connection = self.connection.as_ref().context("not connected")?;
        let data: Option<String> = connection.prepare_cached("SELECT data FROM part WHERE id = ?1")?.query_row([id], |r| text(r, 0)).optional()?.flatten();
        Ok(data.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or_default())
    }

    fn children(&self, session: &str) -> Result<Vec<Row>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement =
            connection.prepare_cached(&format!("SELECT {} FROM session WHERE parent_id = ?1 ORDER BY time_created DESC LIMIT {MAX_AGENTS}", self.schema.row))?;
        let mut rows: Vec<Row> = statement.query_map([session], row)?.collect::<rusqlite::Result<_>>()?;
        rows.reverse();
        Ok(rows)
    }
    /// Re-reads a tracked session when its event sequence moved, and its
    /// subagents when theirs did (each child is re-read only when it moved).
    fn refresh(&mut self, id: &str, now: u64) -> Result<()> {
        if !self.schema.indexed {
            return Ok(());
        }
        let seq = self.seq(id)?;
        let (refreshed, last_seq, children, since_ms, last_refresh_ms, reported) = {
            let state = self.entries.get_mut(id).context("no entry")?.tracked.get_or_insert_with(|| Tracked::new(now));
            (state.refreshed, state.seq, state.children.clone(), state.since_ms, state.last_refresh_ms, state.reported.clone())
        };
        let parent_moved = !refreshed || seq.is_none() || seq != last_seq;
        let mut children_moved = false;
        for (child, (old, agent)) in &children {
            if agent.status == AgentStatus::Working && self.seq(child)? != *old {
                children_moved = true;
                break;
            }
        }
        if !parent_moved && !children_moved {
            return Ok(());
        }
        let mut parent = None;
        if parent_moved {
            let messages = self.messages(id, RECENT_MESSAGES)?;
            let mut latest_parts = Vec::new();
            let mut found = Vec::new();
            let mut checked = Vec::new();
            for (index, message) in messages.iter().enumerate() {
                let recent = index == 0 || message.updated_ms.saturating_add(5_000) >= last_refresh_ms;
                if message.role != "assistant" || !recent {
                    continue;
                }
                let parts = self.parts(&message.id)?;
                for part in &parts {
                    if part.kind == "tool" && part.status == "completed" && part.end_ms >= since_ms && !reported.contains(&part.id) {
                        checked.push(part.id.clone());
                        // Only candidates are read in full, and each only once.
                        if watched_tool(&part.tool)
                            && let Some(event) = detect_part(&self.part_data(&part.id)?)
                        {
                            found.push((part.id.clone(), event));
                        }
                    }
                }
                if index == 0 {
                    latest_parts = parts;
                }
            }
            parent = Some((derive(&messages, &latest_parts), found, checked));
        }
        let mut agents = Vec::new();
        let mut cache = HashMap::new();
        for child in self.children(id)? {
            let child_seq = self.seq(&child.id)?;
            let agent = match children.get(&child.id) {
                Some((old, agent)) if child_seq.is_some() && *old == child_seq => agent.clone(),
                _ => {
                    let child_messages = self.messages(&child.id, 1)?;
                    let child_derived = derive(&child_messages, &[]);
                    // A child session is created just before its first message.
                    let status = if child_messages.is_empty() { AgentStatus::Working } else { agent_status(&child_derived) };
                    let (name, description) = split_subagent_title(&child.title, &child.agent);
                    let finished_ms = if status == AgentStatus::Working { None } else { child_derived.finished_ms.or(Some(child.updated_ms)) };
                    AgentSummary { key: child.id.clone(), name, description, status, finished_ms }
                }
            };
            cache.insert(child.id.clone(), (child_seq, agent.clone()));
            agents.push(agent);
        }
        let state = self.entries.get_mut(id).context("no entry")?.tracked.as_mut().context("not tracked")?;
        if let Some((derived, found, checked)) = parent {
            for (part_id, event) in found {
                if !state.reported.contains(&part_id) {
                    state.detected.push(event);
                }
            }
            for part_id in checked {
                if !state.reported.contains(&part_id) {
                    state.reported.push_back(part_id);
                }
            }
            while state.reported.len() > 500 {
                state.reported.pop_front();
            }
            state.derived = derived;
            state.last_refresh_ms = now;
        }
        state.seq = seq;
        state.refreshed = true;
        state.agents = agents;
        state.children = cache;
        Ok(())
    }

    /// Busy, waiting and open sessions according to the canvas-started TUIs.
    fn server_overlay(&mut self, now: u64) -> Result<HashMap<String, Overlay>> {
        super::opencode::watch_servers();
        let states: Vec<ServerState> = super::opencode::servers().into_iter().map(|(_, state)| state).collect();
        let mut roots = HashMap::new();
        for id in fresh(&states, now).flat_map(|live| live.waiting()) {
            // A subagent's prompt blocks its root session.
            roots.insert(id.to_owned(), self.root_of(id)?);
        }
        Ok(overlay(&states, now, &roots))
    }
    fn root_of(&mut self, id: &str) -> Result<String> {
        let mut current = id.to_owned();
        for _ in 0..4 {
            let parent = match self.parents.get(&current) {
                Some(parent) => parent.clone(),
                None => {
                    let connection = self.connection.as_ref().context("not connected")?;
                    let parent: Option<String> =
                        connection.prepare_cached("SELECT parent_id FROM session WHERE id = ?1")?.query_row([&current], |r| r.get(0)).optional()?.flatten();
                    self.parents.insert(current.clone(), parent.clone());
                    parent
                }
            };
            match parent {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(current)
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < 50 && !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    /// A warning given once per history (not again after a reconnect).
    fn note_once(&mut self, message: String) {
        if self.noted.insert(message.clone()) {
            self.warn(message);
        }
    }
}

fn upsert(entries: &mut HashMap<String, Entry>, found: Row) {
    match entries.get_mut(&found.id) {
        Some(entry) => entry.row = found,
        None => {
            entries.insert(found.id.clone(), Entry { row: found, tracked: None, sent: None });
        }
    }
}

/// Server states whose last answer is at most [`LIVE_FRESH_MS`] old: a
/// server that stopped answering (or whose watcher ended) says nothing.
fn fresh(states: &[ServerState], now: u64) -> impl Iterator<Item = &Live> {
    states.iter().filter(move |s| now.saturating_sub(s.ok_ms) <= LIVE_FRESH_MS).filter_map(|s| s.live.as_ref())
}

/// The overlay from fresh server states; `roots` maps waiting sessions to their root.
fn overlay(states: &[ServerState], now: u64, roots: &HashMap<String, String>) -> HashMap<String, Overlay> {
    let mut overlay: HashMap<String, Overlay> = HashMap::new();
    for state in states.iter().filter(|s| s.live.is_some() && now.saturating_sub(s.ok_ms) <= LIVE_FRESH_MS) {
        for id in &state.sessions {
            overlay.entry(id.clone()).or_default().live = true;
        }
        let Some(live) = &state.live else { continue };
        for id in live.status.keys() {
            let entry = overlay.entry(id.clone()).or_default();
            entry.busy = true;
            entry.live = true;
        }
        for id in live.waiting() {
            let entry = overlay.entry(roots.get(id).cloned().unwrap_or_else(|| id.to_owned())).or_default();
            entry.waiting += 1;
            entry.live = true;
        }
    }
    overlay
}

fn summarize(entry: &mut Entry, tracked: bool, overlay: Option<&Overlay>, git_root: String, now: u64) -> SessionSummary {
    let row = &entry.row;
    let mut summary = SessionSummary {
        id: row.id.clone(),
        name: if row.title.trim().is_empty() { row.id.clone() } else { row.title.clone() },
        cwd: row.directory.replace('/', "\\"),
        git_root,
        client: "opencode".to_owned(),
        created_ms: row.created_ms,
        updated_ms: row.updated_ms,
        tracked,
        live: overlay.is_some_and(|o| o.live),
        ..Default::default()
    };
    let Some(state) = entry.tracked.as_mut() else { return summary };
    let derived = &state.derived;
    summary.activity = derived.activity;
    summary.finished_ms = derived.finished_ms;
    summary.last_event_ms = derived.last_event_ms;
    summary.agents = state.agents.clone();
    let mut pending = derived.pending;
    if let Some(overlay) = overlay.filter(|o| o.live) {
        if overlay.busy {
            state.server_busy = true;
            state.server_finished = None;
            summary.activity = Activity::Working;
            summary.finished_ms = None;
        } else {
            if state.server_busy {
                state.server_busy = false;
                state.server_finished = Some(now);
            }
            // The TUI is idle: an unfinished last message was interrupted.
            if summary.activity == Activity::Working {
                summary.activity = Activity::Idle;
                summary.finished_ms = if derived.aborted { None } else { state.server_finished };
            }
        }
        // The TUI's own list of permission prompts and questions is authoritative.
        pending = pending.max(overlay.waiting);
    }
    summary.pending = pending.min(u16::MAX as usize) as u16;
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = "
        CREATE TABLE session (id text PRIMARY KEY, project_id text NOT NULL, parent_id text, slug text NOT NULL, directory text NOT NULL,
            title text NOT NULL, version text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, time_archived integer, agent text);
        CREATE INDEX session_parent_idx ON session (parent_id);
        CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
        CREATE INDEX message_session_time_created_id_idx ON message (session_id, time_created, id);
        CREATE TABLE part (id text PRIMARY KEY, message_id text NOT NULL, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
        CREATE INDEX part_message_id_id_idx ON part (message_id, id);
        CREATE TABLE event_sequence (aggregate_id text PRIMARY KEY, seq integer NOT NULL);
    ";

    struct Db {
        path: PathBuf,
        connection: Connection,
        seq: i64,
    }

    impl Db {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cc-oc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("opencode.db");
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(SCHEMA).unwrap();
            Db { path, connection, seq: 0 }
        }

        fn bump(&mut self, session: &str) {
            self.seq += 1;
            self.connection.execute("INSERT OR REPLACE INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)", params![session, self.seq]).unwrap();
        }

        fn session(&mut self, id: &str, parent: Option<&str>, title: &str, created: u64) {
            self.connection
                .execute(
                    "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, agent) VALUES (?1, 'p', ?2, 's', 'C:/repos/demo', ?3, '1', ?4, ?4, ?5)",
                    params![id, parent, title, created as i64, if parent.is_some() { Some("general") } else { None }],
                )
                .unwrap();
            self.bump(id);
        }

        fn message(&mut self, session: &str, id: &str, created: u64, data: Value) {
            self.connection
                .execute(
                    "INSERT OR REPLACE INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?3, ?4)",
                    params![id, session, created as i64, data.to_string()],
                )
                .unwrap();
            self.bump(session);
        }

        fn part(&mut self, session: &str, message: &str, id: &str, data: Value) {
            self.connection
                .execute(
                    "INSERT OR REPLACE INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 0, 0, ?4)",
                    params![id, message, session, data.to_string()],
                )
                .unwrap();
            self.bump(session);
        }

        fn history(&self) -> OpenCodeHistory {
            let mut history = OpenCodeHistory::new(self.path.clone());
            history.use_servers = false;
            history
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    const SES: &str = "ses_a1";
    const CHILD: &str = "ses_c1";

    fn tracked() -> HashSet<String> {
        [SES.to_owned()].into_iter().collect()
    }

    fn user(created: u64) -> Value {
        serde_json::json!({ "role": "user", "time": { "created": created } })
    }

    fn assistant(created: u64, completed: Option<u64>, finish: Option<&str>) -> Value {
        let mut v = serde_json::json!({ "role": "assistant", "time": { "created": created }, "path": { "cwd": "C:\\repos\\demo" } });
        if let Some(c) = completed {
            v["time"]["completed"] = c.into();
        }
        if let Some(f) = finish {
            v["finish"] = f.into();
        }
        v
    }

    fn tool(name: &str, status: &str, input: Value, output: &str, end: u64) -> Value {
        serde_json::json!({ "type": "tool", "tool": name, "callID": "c", "state": { "status": status, "input": input, "output": output, "metadata": { "exit": 0 }, "time": { "start": end, "end": end } } })
    }

    #[test]
    fn lists_top_level_sessions_and_tracks_working_waiting_and_done() {
        let mut db = Db::new();
        let now = now_ms();
        db.session(SES, None, "Fix the build", now - 60_000);
        db.session("ses_old", None, "Older", now - 120_000);
        db.session(CHILD, Some(SES), "Explore code (@general subagent)", now - 30_000);
        let mut history = db.history();
        let (changed, _) = history.poll(&HashSet::new(), Duration::ZERO);
        assert_eq!(changed.len(), 2, "subagent sessions are not listed");
        let mine = changed.iter().find(|s| s.id == SES).unwrap();
        assert_eq!((mine.name.as_str(), mine.cwd.as_str(), mine.client.as_str()), ("Fix the build", r"C:\repos\demo", "opencode"));
        assert_eq!(mine.activity, Activity::Unknown);
        // Working: a prompt, then a streaming answer.
        db.message(SES, "msg_1", now - 10_000, user(now - 10_000));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!(changed[0].activity, Activity::Working);
        assert!(changed[0].tracked);
        db.message(SES, "msg_2", now - 9_000, assistant(now - 9_000, None, None));
        db.part(SES, "msg_2", "prt_1", tool("question", "running", serde_json::json!({}), "", 0));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].pending), (Activity::Working, 1), "a question waits for you");
        db.part(SES, "msg_2", "prt_1", tool("question", "completed", serde_json::json!({}), "yes", now - 8_000));
        db.message(SES, "msg_2", now - 9_000, assistant(now - 9_000, Some(now - 7_000), Some("tool-calls")));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].pending), (Activity::Working, 0), "the loop continues after tool calls");
        db.message(SES, "msg_3", now - 6_000, assistant(now - 6_000, Some(now - 5_000), Some("stop")));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].finished_ms), (Activity::Idle, Some(now - 5_000)));
        assert_eq!(changed[0].agents.len(), 1);
        assert_eq!((changed[0].agents[0].name.as_str(), changed[0].agents[0].description.as_str()), ("general", "Explore code"));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert!(changed.is_empty(), "unchanged sessions are not resent");
        // Errors fail the session; an abort is idle with nothing to review.
        let mut failed = assistant(now - 4_000, Some(now - 3_000), None);
        failed["error"] = serde_json::json!({ "name": "APIError", "data": { "message": "boom" } });
        db.message(SES, "msg_4", now - 4_000, failed);
        assert_eq!(history.poll(&tracked(), Duration::from_secs(60)).0[0].activity, Activity::Failed);
        let mut aborted = assistant(now - 2_000, Some(now - 1_000), None);
        aborted["error"] = serde_json::json!({ "name": "MessageAbortedError", "data": { "message": "Aborted" } });
        db.message(SES, "msg_5", now - 2_000, aborted);
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].finished_ms), (Activity::Idle, None));
    }

    #[test]
    fn subagents_follow_their_child_sessions() {
        let mut db = Db::new();
        let now = now_ms();
        db.session(SES, None, "Parent", now - 60_000);
        db.message(SES, "msg_1", now - 50_000, user(now - 50_000));
        db.message(SES, "msg_2", now - 49_000, assistant(now - 49_000, None, None));
        db.part(SES, "msg_2", "prt_t", tool("task", "running", serde_json::json!({ "description": "Explore" }), "", 0));
        db.session(CHILD, Some(SES), "Explore (@explore subagent)", now - 48_000);
        db.message(CHILD, "msg_c1", now - 48_000, user(now - 48_000));
        let mut history = db.history();
        let (changed, _) = history.poll(&tracked(), Duration::ZERO);
        assert_eq!(changed[0].activity, Activity::Working);
        assert_eq!(changed[0].agents[0].status, AgentStatus::Working);
        // Only the child moves; the parent waits in its task tool.
        db.message(CHILD, "msg_c2", now - 47_000, assistant(now - 47_000, Some(now - 40_000), Some("stop")));
        let (changed, _) = history.poll(&tracked(), Duration::from_secs(60));
        assert_eq!(changed[0].agents[0].status, AgentStatus::Finished);
        assert_eq!(changed[0].agents[0].finished_ms, Some(now - 40_000));
    }

    #[test]
    fn git_push_from_bash_parts_becomes_an_event_once() {
        let mut db = Db::new();
        let now = now_ms();
        db.session(SES, None, "Ship it", now - 60_000);
        db.message(SES, "msg_1", now - 50_000, user(now - 50_000));
        db.message(SES, "msg_2", now - 49_000, assistant(now - 49_000, Some(now - 48_000), Some("tool-calls")));
        // History from before tracking started is never re-sent.
        db.part(SES, "msg_2", "prt_old", tool("bash", "completed", serde_json::json!({ "command": "git push" }), "To x\n a..b  m -> m", now - 48_500));
        let mut history = db.history();
        history.poll(&tracked(), Duration::ZERO);
        assert!(history.take_detected().is_empty());
        db.message(SES, "msg_3", now, assistant(now, None, None));
        let push = "To https://github.com/o/r.git\n   1a2b3c4..5d6e7f8  feature -> feature";
        db.part(SES, "msg_3", "prt_new", tool("bash", "completed", serde_json::json!({ "command": "git push origin feature" }), push, now + 1));
        let mut failing = tool("bash", "completed", serde_json::json!({ "command": "git push" }), "rejected", now + 1);
        failing["state"]["metadata"]["exit"] = 1.into();
        db.part(SES, "msg_3", "prt_bad", failing);
        history.poll(&tracked(), Duration::from_secs(60));
        let detected = history.take_detected();
        assert_eq!(detected.len(), 1);
        assert_eq!((detected[0].0.as_str(), detected[0].1.as_str(), detected[0].2["commit"].as_str()), (SES, "pr.pushed", Some("5d6e7f8")));
        db.part(SES, "msg_3", "prt_other", tool("read", "completed", serde_json::json!({}), "", now + 2));
        history.poll(&tracked(), Duration::from_secs(60));
        assert!(history.take_detected().is_empty(), "reported once");
    }

    #[test]
    fn new_sessions_are_found_by_directory_and_time() {
        let mut db = Db::new();
        db.session("ses_before", None, "a", 1_000);
        db.session("ses_mine", None, "b", 5_000);
        db.session("ses_child", Some("ses_mine"), "c", 6_000);
        db.session("ses_later", None, "d", 7_000);
        let connection = open(&db.path).unwrap();
        assert_eq!(discover_in(&connection, r"C:\Repos\demo\", 2_000).as_deref(), Some("ses_mine"));
        assert_eq!(new_sessions_in(&connection, r"C:\Repos\demo\", 2_000), [("ses_mine".to_owned(), 5_000), ("ses_later".to_owned(), 7_000)]);
        assert_eq!(root_session_count(&db.path), Some(3));
        assert_eq!(discover_in(&connection, r"C:\other", 0), None);
        assert!(session_exists(&db.path, "ses_mine"));
        assert!(!session_exists(&db.path, "ses_nope"));
    }

    #[test]
    fn server_overlay_marks_busy_waiting_and_turn_end() {
        let mut entry = Entry {
            row: Row { id: SES.into(), directory: "C:/r".into(), title: "t".into(), ..Default::default() },
            tracked: Some(Tracked::new(0)),
            sent: None,
        };
        entry.tracked.as_mut().unwrap().derived = Derived { activity: Activity::Working, ..Default::default() };
        let busy = Overlay { busy: true, waiting: 1, live: true };
        let summary = summarize(&mut entry, true, Some(&busy), String::new(), 10);
        assert_eq!((summary.activity, summary.pending, summary.live), (Activity::Working, 1, true));
        let idle = Overlay { busy: false, waiting: 0, live: true };
        let summary = summarize(&mut entry, true, Some(&idle), String::new(), 20);
        assert_eq!((summary.activity, summary.finished_ms), (Activity::Idle, Some(20)), "the TUI says the turn ended");
        assert!(!summarize(&mut entry, true, None, String::new(), 30).live);
    }

    #[test]
    fn databases_without_indexes_only_list() {
        let dir = std::env::temp_dir().join(format!("cc-oc-noidx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(&SCHEMA.replace("CREATE INDEX part_message_id_id_idx ON part (message_id, id);", "")).unwrap();
        connection.execute("INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_x', 'p', 's', 'C:/a', 'X', '1', 1, 1)", []).unwrap();
        connection.execute("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('m', 'ses_x', 1, 1, '{\"role\":\"user\"}')", []).unwrap();
        drop(connection);
        let mut history = OpenCodeHistory::new(path);
        history.use_servers = false;
        let (changed, _) = history.poll(&["ses_x".to_owned()].into_iter().collect(), Duration::ZERO);
        assert_eq!((changed[0].name.as_str(), changed[0].activity), ("X", Activity::Unknown));
        assert!(history.warnings.iter().any(|w| w.contains("indexes")));
        drop(history);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subagent_titles_split_into_name_and_description() {
        assert_eq!(split_subagent_title("Find callers (@explore subagent)", ""), ("explore".into(), "Find callers".into()));
        assert_eq!(split_subagent_title("Plain", "general"), ("general".into(), "Plain".into()));
    }

    /// A database at `name` in a fresh folder, built from `schema`.
    fn custom_db(schema: &str) -> (PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!("cc-oc-schema-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(schema).unwrap();
        (path, connection)
    }

    #[test]
    fn older_schemas_without_optional_columns_still_list() {
        let schema = SCHEMA.replace(", time_archived integer, agent text", "");
        let (path, connection) = custom_db(&schema);
        connection.execute("INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_x', 'p', 's', 'C:/a', 'X', '1', 1, 2)", []).unwrap();
        connection.execute("INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_k', 'p', 'ses_x', 's', 'C:/a', 'Kid (@explore subagent)', '1', 1, 2)", []).unwrap();
        let mut history = OpenCodeHistory::new(path.clone());
        history.use_servers = false;
        let (changed, _) = history.poll(&["ses_x".to_owned()].into_iter().collect(), Duration::ZERO);
        assert_eq!(changed.len(), 1, "{:?}", history.warnings);
        assert_eq!((changed[0].name.as_str(), changed[0].agents[0].name.as_str()), ("X", "explore"));
        assert!(history.warnings.is_empty(), "{:?}", history.warnings);
        drop((history, connection));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_list_and_sequence_indexes_are_noted_once_and_degrade() {
        let schema = SCHEMA
            .replace("CREATE INDEX session_parent_idx ON session (parent_id);", "")
            .replace("CREATE TABLE event_sequence (aggregate_id text PRIMARY KEY, seq integer NOT NULL);", "CREATE TABLE event_sequence (aggregate_id text, seq integer);");
        let (path, connection) = custom_db(&schema);
        connection.execute("INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_x', 'p', 's', 'C:/a', 'X', '1', 1, 2)", []).unwrap();
        connection.execute("INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_y', 'p', 's', 'C:/a', 'Y', '1', 1, 2)", []).unwrap();
        let mut history = OpenCodeHistory::new(path.clone());
        history.use_servers = false;
        let (changed, _) = history.poll(&["ses_x".to_owned()].into_iter().collect(), Duration::ZERO);
        assert_eq!(changed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["ses_x"], "no unindexed list scan; tracked sessions still read");
        let warnings = std::mem::take(&mut history.warnings);
        assert!(warnings.iter().any(|w| w.contains("parent_id")) && warnings.iter().any(|w| w.contains("event_sequence")), "{warnings:?}");
        // Reconnecting does not repeat them.
        history.connection = None;
        history.poll(&["ses_x".to_owned()].into_iter().collect(), Duration::ZERO);
        assert!(history.warnings.is_empty(), "{:?}", history.warnings);
        drop((history, connection));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn query_errors_back_off_instead_of_reopening_every_poll() {
        // Indexed, but `message` has no `data` column: reading a tracked session fails.
        let schema = SCHEMA.replace("time_updated integer NOT NULL, data text NOT NULL);\n        CREATE INDEX message_session", "time_updated integer NOT NULL);\n        CREATE INDEX message_session");
        assert_ne!(schema, SCHEMA);
        let (path, connection) = custom_db(&schema);
        connection.execute("INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_x', 'p', 's', 'C:/a', 'X', '1', 1, 2)", []).unwrap();
        let mut history = OpenCodeHistory::new(path.clone());
        history.use_servers = false;
        let tracked: HashSet<String> = ["ses_x".to_owned()].into_iter().collect();
        assert!(history.poll(&tracked, Duration::ZERO).0.is_empty());
        assert!(history.connection.is_none() && history.retry_at.is_some_and(|t| t > Instant::now() + Duration::from_secs(25)));
        history.warnings.clear();
        assert!(history.poll(&tracked, Duration::ZERO).0.is_empty());
        assert!(history.connection.is_none() && history.warnings.is_empty(), "no reopen and no new error within the pause");
        drop((history, connection));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_rows_do_not_break_a_poll() {
        let mut db = Db::new();
        let now = now_ms();
        db.session(SES, None, "S", now - 60_000);
        db.connection.execute("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_bad', ?1, 1, 1, '{not json')", [SES]).unwrap();
        db.message(SES, "msg_2", now - 9_000, assistant(now - 9_000, None, None));
        db.connection.execute("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('prt_bad', 'msg_2', ?1, 0, 0, 'garbage')", [SES]).unwrap();
        db.part(SES, "msg_2", "prt_q", tool("question", "running", serde_json::json!({}), "", 0));
        let mut history = db.history();
        let (changed, _) = history.poll(&tracked(), Duration::ZERO);
        assert_eq!((changed[0].activity, changed[0].pending), (Activity::Working, 1), "{:?}", history.warnings);
    }

    #[test]
    fn stale_server_state_is_ignored() {
        let live = |busy: &str| ServerState {
            live: Some(Live { status: [(busy.to_owned(), "busy".to_owned())].into_iter().collect(), permissions: vec![("ses_child".into(), "per".into())], ..Default::default() }),
            ok_ms: 10_000,
            sessions: [busy.to_owned()].into_iter().collect(),
            directory: None,
        };
        let roots: HashMap<String, String> = [("ses_child".to_owned(), "ses_root".to_owned())].into_iter().collect();
        let fresh_overlay = overlay(&[live("ses_a")], 10_000 + LIVE_FRESH_MS, &roots);
        assert!(fresh_overlay["ses_a"].busy && fresh_overlay["ses_a"].live);
        assert_eq!(fresh_overlay["ses_root"].waiting, 1, "a subagent's prompt counts for its root");
        assert!(overlay(&[live("ses_a")], 10_000 + LIVE_FRESH_MS + 1, &roots).is_empty(), "too old");
        assert!(overlay(&[ServerState { live: None, ..live("ses_a") }], 10_000, &roots).is_empty(), "not answering");
        assert_eq!(fresh(&[live("ses_a")], 20_000).count(), 0);
    }
}
