//! OpenCode session history from its SQLite database. The database can be
//! hundreds of gigabytes: it is opened read-only and only indexed lookups
//! run (sessions by parent, messages by session, parts by message). The
//! event sequence number of a session tells cheaply whether anything changed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use crate::paths::now_ms;
use crate::protocol::{Activity, AgentStatus, AgentSummary, SessionSummary};

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

/// Opens the database read-only.
pub fn open(path: &Path) -> Result<Connection> {
    anyhow::ensure!(path.is_file(), "{} does not exist", path.display());
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let connection = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)
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

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

/// The first top-level session created in `cwd` at or after `since_ms`: the
/// one a freshly launched TUI made for its first prompt.
pub fn discover_new_session(cwd: &str, since_ms: u64) -> Option<String> {
    discover_in(&open(&db_path()).ok()?, cwd, since_ms)
}

pub fn discover_in(connection: &Connection, cwd: &str, since_ms: u64) -> Option<String> {
    let mut statement = connection
        .prepare_cached("SELECT id, directory FROM session WHERE parent_id IS NULL AND time_created >= ?1 ORDER BY time_created ASC, id ASC")
        .ok()?;
    let rows = statement.query_map([since_ms as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok()?;
    rows.filter_map(|r| r.ok()).find(|(_, dir)| same_dir(dir, cwd)).map(|(id, _)| id)
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

const ROW: &str = "id, parent_id, directory, title, agent, time_created, time_updated";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        parent: r.get(1)?,
        directory: r.get(2)?,
        title: r.get(3)?,
        agent: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        created_ms: r.get::<_, i64>(5)?.max(0) as u64,
        updated_ms: r.get::<_, i64>(6)?.max(0) as u64,
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

pub fn derive(messages: &[Msg], parts: &[Value]) -> Derived {
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
    for part in parts.iter().filter(|p| p["type"] == "tool") {
        let status = part["state"]["status"].as_str().unwrap_or_default();
        if matches!(status, "pending" | "running") {
            if out.activity != Activity::Failed {
                out.activity = Activity::Working;
                out.finished_ms = None;
            }
            if part["tool"] == "question" {
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
    indexed: bool,
    has_seq: bool,
    retry_at: Option<Instant>,
    entries: HashMap<String, Entry>,
    parents: HashMap<String, Option<String>>,
    git_roots: HashMap<String, String>,
    last_full_scan: Option<Instant>,
    /// Ask the canvas-started TUIs' servers for live state.
    pub use_servers: bool,
    pub warnings: Vec<String>,
}

impl OpenCodeHistory {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            connection: None,
            indexed: false,
            has_seq: false,
            retry_at: None,
            entries: HashMap::new(),
            parents: HashMap::new(),
            git_roots: HashMap::new(),
            last_full_scan: None,
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
                self.indexed = has_index(&connection, "message", &["session_id", "time_created"])
                    && has_index(&connection, "part", &["message_id"])
                    && has_index(&connection, "session", &["parent_id"]);
                if !self.indexed {
                    self.warn("opencode: the database lacks the expected indexes; only listing sessions".into());
                }
                self.has_seq = has_table(&connection, "event_sequence");
                self.connection = Some(connection);
                self.retry_at = None;
                true
            }
            Err(error) => {
                if self.path.exists() {
                    self.warn(format!("opencode: {error:#}"));
                }
                self.retry_at = Some(Instant::now() + Duration::from_secs(10));
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
                // Reopen next time (schema change, locked database, …).
                self.warn(format!("opencode: {error:#}"));
                self.connection = None;
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
            if full {
                self.last_full_scan = Some(Instant::now());
                let mut statement = connection.prepare_cached(&format!(
                    "SELECT {ROW} FROM session WHERE parent_id IS NULL AND time_archived IS NULL ORDER BY time_updated DESC LIMIT {LIST_LIMIT}"
                ))?;
                for found in statement.query_map([], row)? {
                    let found = found?;
                    seen.insert(found.id.clone());
                    upsert(&mut self.entries, found);
                }
            }
            // Tracked sessions are read every time (a primary-key lookup).
            let mut statement = connection.prepare_cached(&format!("SELECT {ROW} FROM session WHERE id = ?1"))?;
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
        let overlay = if self.use_servers { self.server_overlay()? } else { HashMap::new() };
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
        if !self.has_seq {
            return Ok(None);
        }
        let connection = self.connection.as_ref().context("not connected")?;
        Ok(connection.prepare_cached("SELECT seq FROM event_sequence WHERE aggregate_id = ?1")?.query_row([id], |r| r.get(0)).optional()?)
    }

    fn messages(&self, session: &str, limit: usize) -> Result<Vec<Msg>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement =
            connection.prepare_cached("SELECT id, time_updated, data FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2")?;
        let rows = statement.query_map(params![session, limit as i64], |r| Ok(Msg::parse(r.get(0)?, r.get::<_, i64>(1)?.max(0) as u64, &r.get::<_, String>(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn parts(&self, message: &str) -> Result<Vec<(String, Value)>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement = connection.prepare_cached("SELECT id, data FROM part WHERE message_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([message], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).map(|(id, data)| (id, serde_json::from_str(&data).unwrap_or_default())).collect())
    }

    fn children(&self, session: &str) -> Result<Vec<Row>> {
        let connection = self.connection.as_ref().context("not connected")?;
        let mut statement = connection.prepare_cached(&format!("SELECT {ROW} FROM session WHERE parent_id = ?1 ORDER BY time_created DESC LIMIT {MAX_AGENTS}"))?;
        let mut rows: Vec<Row> = statement.query_map([session], row)?.collect::<rusqlite::Result<_>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Re-reads a tracked session when its event sequence moved, and its
    /// subagents when theirs did (each child is re-read only when it moved).
    fn refresh(&mut self, id: &str, now: u64) -> Result<()> {
        if !self.indexed {
            return Ok(());
        }
        let seq = self.seq(id)?;
        let (refreshed, last_seq, children, since_ms, last_refresh_ms) = {
            let state = self.entries.get_mut(id).context("no entry")?.tracked.get_or_insert_with(|| Tracked::new(now));
            (state.refreshed, state.seq, state.children.clone(), state.since_ms, state.last_refresh_ms)
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
                let recent = index == 0 || message.updated_ms + 5_000 >= last_refresh_ms;
                if message.role != "assistant" || !recent {
                    continue;
                }
                let parts = self.parts(&message.id)?;
                for (part_id, part) in &parts {
                    let ended = part["state"]["time"]["end"].as_u64().unwrap_or(0);
                    if part["type"] == "tool" && part["state"]["status"] == "completed" && ended >= since_ms {
                        checked.push(part_id.clone());
                        if let Some(event) = detect_part(part) {
                            found.push((part_id.clone(), event));
                        }
                    }
                }
                if index == 0 {
                    latest_parts = parts.into_iter().map(|(_, p)| p).collect();
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
    fn server_overlay(&mut self) -> Result<HashMap<String, Overlay>> {
        super::opencode::watch_servers();
        let mut overlay: HashMap<String, Overlay> = HashMap::new();
        for (_, state) in super::opencode::servers() {
            let Some(live) = &state.live else { continue };
            for id in &state.sessions {
                overlay.entry(id.clone()).or_default().live = true;
            }
            for id in live.status.keys() {
                let entry = overlay.entry(id.clone()).or_default();
                entry.busy = true;
                entry.live = true;
            }
            for id in live.waiting() {
                // A subagent's prompt blocks its root session.
                let root = self.root_of(id)?;
                let entry = overlay.entry(root).or_default();
                entry.waiting += 1;
                entry.live = true;
            }
        }
        Ok(overlay)
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
}

fn upsert(entries: &mut HashMap<String, Entry>, found: Row) {
    match entries.get_mut(&found.id) {
        Some(entry) => entry.row = found,
        None => {
            entries.insert(found.id.clone(), Entry { row: found, tracked: None, sent: None });
        }
    }
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
}
