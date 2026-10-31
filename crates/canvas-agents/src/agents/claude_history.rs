//! Claude Code session history: transcripts in
//! `~/.claude/projects/<dir>/<session>.jsonl` (read-only, tailed like
//! Copilot's `events.jsonl`) and live state from the per-process files
//! `~/.claude/sessions/<pid>.json` (busy / idle / waiting for you).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde_json::Value;

use crate::paths::{now_ms, parse_rfc3339_ms};
use crate::protocol::{Activity, AgentStatus, AgentSummary, SessionSummary};

const INITIAL_TAIL: u64 = 1024 * 1024;
const HEAD: u64 = 64 * 1024;
const TITLE_TAIL: u64 = 64 * 1024;
const MAX_AGENTS: usize = 12;
/// A transcript line longer than this is skipped instead of buffered.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// Bytes from the start of a transcript that identify it (see [`Identity`]).
const IDENTITY_BYTES: u64 = 64;
/// Silence after `end_turn` before the turn counts as finished when no
/// `turn_duration` record follows.
const SETTLE_MS: u64 = 4_000;

/// Claude Code's data folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| super::home().join(".claude"))
}

/// Claude Code's user configuration (`.claude.json`: MCP servers, projects).
pub fn user_config() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => super::home().join(".claude.json"),
    }
}

/// The project folder name Claude Code uses for a working directory.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The transcript of `id` under `dir/projects`, if it exists.
pub fn find_transcript(dir: &Path, id: &str, cwd_hint: Option<&str>) -> Option<PathBuf> {
    let projects = dir.join("projects");
    let file = format!("{id}.jsonl");
    if let Some(cwd) = cwd_hint {
        let path = projects.join(project_dir_name(cwd)).join(&file);
        if path.is_file() {
            return Some(path);
        }
    }
    std::fs::read_dir(&projects).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join(&file)).find(|p| p.is_file())
}

/// `~/.claude/sessions/<pid>.json`: one per running Claude Code process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PidFile {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub started_ms: u64,
    /// Process creation time (FILETIME) recorded by Claude Code.
    pub proc_start: Option<u64>,
    /// busy, idle, waiting or shell (idle with background tasks).
    pub status: String,
    pub waiting_for: String,
    pub updated_ms: u64,
    pub status_ms: u64,
    pub name: String,
}

impl PidFile {
    pub fn parse(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let number = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
        Some(Self {
            pid: u32::try_from(number(&v["pid"])?).ok()?,
            session_id: v["sessionId"].as_str()?.to_owned(),
            cwd: v["cwd"].as_str().unwrap_or_default().to_owned(),
            started_ms: number(&v["startedAt"]).unwrap_or(0),
            proc_start: number(&v["procStart"]),
            status: v["status"].as_str().unwrap_or_default().to_owned(),
            waiting_for: v["waitingFor"].as_str().unwrap_or_default().to_owned(),
            updated_ms: number(&v["updatedAt"]).unwrap_or(0),
            status_ms: number(&v["statusUpdatedAt"]).or_else(|| number(&v["updatedAt"])).unwrap_or(0),
            name: v["name"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// The process is running and is the one that wrote the file (not a recycled pid).
    pub fn alive(&self) -> bool {
        let Some(started) = canvas_sys::process::process_start_ms(self.pid) else { return false };
        match self.proc_start {
            Some(filetime) => started.abs_diff((filetime / 10_000).saturating_sub(11_644_473_600_000)) <= 1,
            None => started <= self.started_ms.saturating_add(2_000),
        }
    }

    /// Waiting for a permission answer, a question or a dialog.
    pub fn needs_you(&self) -> bool {
        self.status == "waiting" || matches!(self.waiting_for.as_str(), "permission prompt" | "input needed")
    }
}

/// Live pid files by session id (the newest process wins). Files that do not
/// parse, name another pid or carry an invalid session id are ignored.
pub fn live_sessions(dir: &Path) -> HashMap<String, PidFile> {
    let mut out: HashMap<String, PidFile> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir.join("sessions")) else { return out };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else { continue };
        if stem.parse::<u32>().is_err() {
            continue;
        }
        let Some(file) = std::fs::read_to_string(entry.path()).ok().and_then(|t| PidFile::parse(&t)) else { continue };
        if file.pid.to_string() != stem || !crate::index::is_session_id(&file.session_id) || !file.alive() {
            continue;
        }
        match out.get(&file.session_id) {
            Some(other) if other.started_ms >= file.started_ms => {}
            _ => {
                out.insert(file.session_id.clone(), file);
            }
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Titles {
    custom: String,
    agent: String,
    ai: String,
    last_prompt: String,
}

impl Titles {
    fn merge(&mut self, newer: &Titles) {
        for (mine, theirs) in [(&mut self.custom, &newer.custom), (&mut self.agent, &newer.agent), (&mut self.ai, &newer.ai), (&mut self.last_prompt, &newer.last_prompt)] {
            if !theirs.is_empty() {
                mine.clone_from(theirs);
            }
        }
    }

    fn best(&self) -> Option<&str> {
        [&self.custom, &self.agent, &self.ai, &self.last_prompt].into_iter().map(|s| s.trim()).find(|s| !s.is_empty())
    }

    /// Takes a title record; true when the line was one.
    fn take(&mut self, kind: &str, v: &Value) -> bool {
        let (slot, key) = match kind {
            "custom-title" => (&mut self.custom, "customTitle"),
            "agent-name" => (&mut self.agent, "agentName"),
            "ai-title" => (&mut self.ai, "aiTitle"),
            "last-prompt" => (&mut self.last_prompt, "lastPrompt"),
            _ => return false,
        };
        if let Some(text) = v[key].as_str().filter(|t| !t.trim().is_empty()) {
            *slot = one_line(text, 100);
        }
        true
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max { format!("{}…", flat.chars().take(max - 1).collect::<String>()) } else { flat }
}

/// Status of one transcript, built by replaying its records.
#[derive(Default, Debug, Clone)]
pub struct ClaudeState {
    pub activity: Activity,
    pub finished_ms: Option<u64>,
    pub last_turn_end_ms: Option<u64>,
    pub last_event_ms: u64,
    /// Open `AskUserQuestion` calls.
    pub pending: HashSet<String>,
    pub open_tools: HashSet<String>,
    pub agents: Vec<AgentSummary>,
    /// The last turn ended because you stopped it (nothing to review).
    pub aborted: bool,
    /// Events before this time are history: never turned into bus events.
    pub since_ms: u64,
    pub detected: Vec<(String, Value)>,
    /// Subagent id (`agentId`) → agent key (the `Agent` tool call id).
    agent_ids: HashMap<String, String>,
    agent_started: HashMap<String, u64>,
    pending_calls: HashMap<String, (String, Value)>,
    titles: Titles,
    branch: String,
    cwd: String,
    first_ms: Option<u64>,
}

/// Tool calls that may become git/PR events.
fn watched(name: &str, input: &Value) -> Option<(String, Value)> {
    if matches!(name, "Bash" | "PowerShell") {
        let command = input["command"].as_str().unwrap_or_default();
        let lower = command.to_ascii_lowercase();
        let watch = lower.contains("git push") || lower.contains("gh pr ") || lower.contains("az repos pr ");
        return watch.then(|| ("bash".to_owned(), serde_json::json!({ "command": command })));
    }
    name.contains("pull_request").then(|| (name.to_owned(), input.clone()))
}

/// Text of a tool result: the tool's stdout and stderr when recorded, else the result content.
pub(crate) fn result_text(block: &Value, tool_use_result: &Value) -> String {
    if let Some(out) = tool_use_result.get("stdout").and_then(|s| s.as_str()) {
        let err = tool_use_result["stderr"].as_str().unwrap_or_default();
        return if err.is_empty() { out.to_owned() } else { format!("{out}\n{err}") };
    }
    match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim())
}

/// A subagent id safe to use in a file name: 1-128 of `[A-Za-z0-9_-]`.
pub(crate) fn is_agent_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl ClaudeState {
    /// Applies one transcript line; false when it is not JSON.
    pub fn apply(&mut self, line: &str) -> bool {
        // Bulky records that never matter for status.
        if line.starts_with("{\"type\":\"api-request-blob\"") || line.starts_with("{\"type\":\"file-history") {
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
        let kind = v["type"].as_str().unwrap_or_default();
        if self.titles.take(kind, &v) {
            return true;
        }
        let time = v["timestamp"].as_str().and_then(parse_rfc3339_ms);
        if let Some(time) = time {
            self.last_event_ms = self.last_event_ms.max(time);
            self.first_ms.get_or_insert(time);
        }
        if let Some(branch) = v["gitBranch"].as_str().filter(|b| !b.is_empty()) {
            branch.clone_into(&mut self.branch);
        }
        if self.cwd.is_empty()
            && let Some(cwd) = v["cwd"].as_str()
        {
            cwd.clone_into(&mut self.cwd);
        }
        // Subagent turns recorded in the main transcript.
        if v["isSidechain"].as_bool() == Some(true) {
            return true;
        }
        match kind {
            "user" => self.user(&v, time),
            "assistant" => self.assistant(&v, time),
            "system" if v["subtype"] == "turn_duration" => {
                if self.activity != Activity::Failed {
                    self.activity = Activity::Idle;
                    self.finished_ms = if self.aborted { None } else { time.or(Some(self.last_event_ms)) };
                }
                self.last_turn_end_ms = None;
                self.open_tools.clear();
                self.pending.clear();
            }
            _ => {}
        }
        true
    }

    fn turn(&mut self) {
        self.activity = Activity::Working;
        self.finished_ms = None;
        self.last_turn_end_ms = None;
        self.aborted = false;
    }

    /// Esc: you stopped it, so there is nothing new to review.
    fn abort(&mut self) {
        self.activity = Activity::Idle;
        self.finished_ms = None;
        self.last_turn_end_ms = None;
        self.aborted = true;
        self.pending.clear();
        self.open_tools.clear();
    }

    fn user(&mut self, v: &Value, time: Option<u64>) {
        if v["isMeta"].as_bool() == Some(true) {
            return;
        }
        let mut texts = Vec::new();
        let mut results = Vec::new();
        match &v["message"]["content"] {
            Value::String(s) => texts.push(s.as_str()),
            Value::Array(blocks) => {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") => texts.extend(block["text"].as_str()),
                        Some("tool_result") => results.push(block),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let mut interrupted = false;
        let mut turn = !results.is_empty();
        for block in &results {
            let id = block["tool_use_id"].as_str().unwrap_or_default().to_owned();
            let is_error = block["is_error"].as_bool() == Some(true);
            self.open_tools.remove(&id);
            self.pending.remove(&id);
            let text = result_text(block, &v["toolUseResult"]);
            if is_error && text.contains("[Request interrupted by user") {
                interrupted = true;
            }
            if let Some(agent) = self.agents.iter_mut().find(|a| a.key == id) {
                let result = &v["toolUseResult"];
                // The id names files below the transcript's folder (see `refresh_tail`).
                if let Some(agent_id) = result["agentId"].as_str().filter(|a| is_agent_id(a)) {
                    self.agent_ids.insert(agent_id.to_owned(), id.clone());
                }
                if result["status"].as_str() != Some("async_launched") {
                    agent.status = if is_error { AgentStatus::Failed } else { AgentStatus::Finished };
                    agent.finished_ms = time;
                }
            }
            if let Some((tool, args)) = self.pending_calls.remove(&id)
                && !is_error
                && time.is_some_and(|t| self.since_ms > 0 && t >= self.since_ms)
                && let Some(found) = crate::index::detect(&tool, &args, &text)
            {
                self.detected.push(found);
            }
        }
        for text in texts {
            let text = text.trim_start();
            if text.starts_with("[Request interrupted by user") {
                interrupted = true;
            } else if text.starts_with("<task-notification>") {
                self.notification(text, time);
                turn = true;
            } else if !(text.starts_with("<command-") || text.starts_with("<local-command") || text.starts_with("<bash-")) {
                turn = true;
            }
        }
        if interrupted {
            self.abort();
        } else if turn {
            self.turn();
        }
    }

    fn notification(&mut self, text: &str, time: Option<u64>) {
        let key = tag(text, "tool-use-id").map(str::to_owned).or_else(|| tag(text, "task-id").and_then(|id| self.agent_ids.get(id).cloned()));
        let Some(agent) = key.and_then(|key| self.agents.iter_mut().find(|a| a.key == key)) else { return };
        let (status, done) = match tag(text, "status").unwrap_or("completed") {
            "completed" => (AgentStatus::Finished, true),
            "failed" => (AgentStatus::Failed, true),
            "running" => (AgentStatus::Working, false),
            _ => (AgentStatus::Unknown, true),
        };
        agent.status = status;
        agent.finished_ms = if done { time } else { None };
    }

    fn assistant(&mut self, v: &Value, time: Option<u64>) {
        if v["isApiErrorMessage"].as_bool() == Some(true) {
            self.activity = Activity::Failed;
            self.finished_ms = None;
            self.last_turn_end_ms = None;
            self.open_tools.clear();
            self.pending.clear();
            return;
        }
        self.turn();
        let message = &v["message"];
        if let Some(blocks) = message["content"].as_array() {
            for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                let name = block["name"].as_str().unwrap_or_default();
                let input = &block["input"];
                if id.is_empty() {
                    continue;
                }
                self.open_tools.insert(id.clone());
                if name == "AskUserQuestion" {
                    self.pending.insert(id.clone());
                }
                if matches!(name, "Agent" | "Task") && !self.agents.iter().any(|a| a.key == id) {
                    let kind = input["subagent_type"].as_str().filter(|s| !s.is_empty()).unwrap_or("Subagent");
                    self.agents.push(AgentSummary {
                        key: id.clone(),
                        name: kind.to_owned(),
                        description: one_line(input["description"].as_str().unwrap_or_default(), 120),
                        status: AgentStatus::Working,
                        finished_ms: None,
                    });
                    self.agent_started.insert(id.clone(), time.unwrap_or(self.last_event_ms));
                    if self.agents.len() > MAX_AGENTS {
                        let excess = self.agents.len() - MAX_AGENTS;
                        for gone in self.agents.drain(..excess) {
                            self.agent_started.remove(&gone.key);
                        }
                    }
                }
                if let Some(call) = watched(name, input) {
                    self.pending_calls.insert(id, call);
                }
            }
        }
        match message["stop_reason"].as_str() {
            None | Some("tool_use") => {}
            Some(_) => self.last_turn_end_ms = time.or(Some(self.last_event_ms)),
        }
    }

    /// Activity including the settle rule (a turn ended and nothing followed).
    pub fn derived(&self, now: u64) -> (Activity, Option<u64>) {
        if self.activity == Activity::Working
            && self.pending.is_empty()
            && self.open_tools.is_empty()
            && let Some(ended) = self.last_turn_end_ms
            && now.saturating_sub(self.last_event_ms.max(ended)) >= SETTLE_MS
        {
            return (Activity::Idle, Some(ended));
        }
        (self.activity, self.finished_ms)
    }

    /// Forgets what a skipped part of the transcript may have left open.
    fn reset_unknown(&mut self) {
        self.pending.clear();
        self.open_tools.clear();
        self.activity = Activity::Unknown;
        for agent in &mut self.agents {
            if agent.status == AgentStatus::Working {
                agent.status = AgentStatus::Unknown;
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
struct Meta {
    cwd: String,
    branch: String,
    created_ms: u64,
    updated_ms: u64,
    first_prompt: String,
    titles: Titles,
    head_done: bool,
}

struct Tail {
    offset: u64,
    partial: Vec<u8>,
    /// Inside a line that is being skipped (too long, or cut by a skip ahead).
    skipping: bool,
    /// The file this tail has read (a replaced file is read again from the start).
    identity: Option<Identity>,
    state: ClaudeState,
    truncated: bool,
}

/// A cheap identity of a transcript file: its creation time and first bytes.
#[derive(Clone, Debug, PartialEq)]
struct Identity {
    created: Option<SystemTime>,
    head: Vec<u8>,
}

impl Identity {
    fn read(path: &Path, metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self { created: metadata.created().ok(), head: read_range(path, 0, IDENTITY_BYTES).ok()? })
    }

    /// The same file, possibly grown since `self` was read.
    fn same_file(&self, newer: &Identity) -> bool {
        self.created == newer.created && newer.head.starts_with(&self.head)
    }
}

struct Entry {
    path: Option<PathBuf>,
    stamp: Option<(Option<SystemTime>, u64)>,
    meta: Meta,
    tail: Option<Tail>,
    pid: Option<PidFile>,
    /// Turn end seen in the pid file (busy → idle) and the status before.
    status_finished: Option<u64>,
    last_status: String,
    sent: Option<SessionSummary>,
}

impl Entry {
    fn new(path: Option<PathBuf>) -> Self {
        Self { path, stamp: None, meta: Meta::default(), tail: None, pid: None, status_finished: None, last_status: String::new(), sent: None }
    }
}

/// Claude Code's sessions as a [`super::History`].
pub struct ClaudeHistory {
    dir: PathBuf,
    entries: HashMap<String, Entry>,
    git_roots: HashMap<String, String>,
    /// Tracked ids without a transcript (other agents' sessions), until the next full scan.
    unknown: HashSet<String>,
    last_full_scan: Option<Instant>,
    pub warnings: Vec<String>,
}

fn stamp(meta: &std::fs::Metadata) -> (Option<SystemTime>, u64) {
    (meta.modified().ok(), meta.len())
}

fn system_ms(time: Option<SystemTime>) -> u64 {
    time.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn read_range(path: &Path, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Complete lines of a chunk; `skip_first` drops a partial first line.
fn lines(chunk: &[u8], skip_first: bool) -> impl Iterator<Item = String> + '_ {
    let mut slice = chunk;
    if skip_first {
        slice = match slice.iter().position(|&b| b == b'\n') {
            Some(pos) => &slice[pos + 1..],
            None => &[],
        };
    }
    let complete = slice.iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
    slice[..complete].split(|&b| b == b'\n').map(|l| String::from_utf8_lossy(l).trim().to_owned()).filter(|l| !l.is_empty())
}

fn first_prompt(v: &Value) -> Option<String> {
    if v["type"] != "user" || v["isMeta"].as_bool() == Some(true) || v["isSidechain"].as_bool() == Some(true) {
        return None;
    }
    let text = match &v["message"]["content"] {
        Value::String(s) => s.as_str(),
        Value::Array(blocks) => blocks.iter().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str())?,
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<') && !text.starts_with('[')).then(|| one_line(text, 100))
}

fn parse_head(path: &Path, meta: &mut Meta) {
    let Ok(chunk) = read_range(path, 0, HEAD) else { return };
    for line in lines(&chunk, false) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if meta.cwd.is_empty()
            && let Some(cwd) = v["cwd"].as_str()
        {
            cwd.clone_into(&mut meta.cwd);
        }
        if meta.branch.is_empty()
            && let Some(branch) = v["gitBranch"].as_str()
        {
            branch.clone_into(&mut meta.branch);
        }
        if meta.created_ms == 0
            && let Some(time) = v["timestamp"].as_str().and_then(parse_rfc3339_ms)
        {
            meta.created_ms = time;
        }
        if meta.first_prompt.is_empty()
            && let Some(prompt) = first_prompt(&v)
        {
            meta.first_prompt = prompt;
        }
    }
    meta.head_done = true;
}

/// The `gitBranch` of a record line without parsing it.
fn branch_of(line: &str) -> Option<&str> {
    const KEY: &str = "\"gitBranch\":\"";
    let start = line.find(KEY)? + KEY.len();
    let end = line[start..].find('"')? + start;
    (end > start).then(|| &line[start..end])
}

/// Titles and the current branch from the end of the transcript.
fn parse_title_tail(path: &Path, size: u64, meta: &mut Meta) {
    for window in [TITLE_TAIL, INITIAL_TAIL] {
        let start = size.saturating_sub(window);
        let Ok(chunk) = read_range(path, start, size - start) else { return };
        let mut titles = Titles::default();
        let mut branch = None;
        for line in lines(&chunk, start > 0) {
            // Title records are small and have no envelope; only they are parsed.
            let title = line.starts_with("{\"type\":\"") && (line.contains("-title\"") || line.contains("\"agent-name\"") || line.contains("\"last-prompt\""));
            if title {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    titles.take(v["type"].as_str().unwrap_or_default(), &v);
                }
            } else if let Some(found) = branch_of(&line) {
                branch = Some(found.to_owned());
            }
        }
        if let Some(branch) = branch {
            meta.branch = branch;
        }
        let found = titles.best().is_some();
        meta.titles.merge(&titles);
        if found || start == 0 {
            return;
        }
    }
}

impl ClaudeHistory {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, entries: HashMap::new(), git_roots: HashMap::new(), unknown: HashSet::new(), last_full_scan: None, warnings: Vec::new() }
    }

    pub fn poll(&mut self, tracked: &HashSet<String>, full_every: std::time::Duration) -> (Vec<SessionSummary>, Vec<String>) {
        let now = now_ms();
        let full = self.last_full_scan.is_none_or(|t| t.elapsed() >= full_every);
        let mut seen = HashSet::new();
        if full {
            self.last_full_scan = Some(Instant::now());
            self.unknown.clear();
            self.scan(tracked, &mut seen);
        }
        let live = live_sessions(&self.dir);
        // Tracked sessions may be new (just launched) and not yet in a full scan.
        for id in tracked.iter().filter(|id| crate::index::is_session_id(id)) {
            let known = self.entries.contains_key(id) || live.contains_key(id);
            if seen.contains(id) || (!known && self.unknown.contains(id)) {
                continue;
            }
            let known_path = self.entries.get(id).and_then(|e| e.path.clone()).filter(|p| p.is_file());
            let path = known_path.or_else(|| find_transcript(&self.dir, id, live.get(id).map(|p| p.cwd.as_str())));
            match (path, live.contains_key(id)) {
                (Some(path), _) => {
                    seen.insert(id.clone());
                    self.refresh_meta(id, Some(path), true);
                }
                (None, true) => {
                    seen.insert(id.clone());
                    self.entries.entry(id.clone()).or_insert_with(|| Entry::new(None));
                }
                (None, false) if self.entries.contains_key(id) => {
                    seen.insert(id.clone());
                }
                (None, false) => {
                    self.unknown.insert(id.clone());
                }
            }
        }
        // Open sessions are listed even before their first prompt.
        for id in live.keys() {
            seen.insert(id.clone());
            self.entries.entry(id.clone()).or_insert_with(|| Entry::new(None));
        }
        let mut removed = Vec::new();
        if full {
            let gone: Vec<String> = self.entries.keys().filter(|id| !seen.contains(*id) && !tracked.contains(*id)).cloned().collect();
            for id in gone {
                self.entries.remove(&id);
                removed.push(id);
            }
        }
        let mut changed = Vec::new();
        let ids: Vec<String> = self.entries.keys().cloned().collect();
        for id in ids {
            let is_tracked = tracked.contains(&id);
            if is_tracked {
                self.refresh_tail(&id);
            } else if let Some(entry) = self.entries.get_mut(&id) {
                entry.tail = None;
            }
            let cwd = {
                let entry = self.entries.get_mut(&id).expect("entry exists");
                entry.pid = live.get(&id).cloned();
                note_status(entry);
                if entry.meta.cwd.is_empty() { entry.pid.as_ref().map(|p| p.cwd.clone()).unwrap_or_default() } else { entry.meta.cwd.clone() }
            };
            let git_root = self.git_root(&cwd);
            let entry = self.entries.get_mut(&id).expect("entry exists");
            let summary = summarize(&id, entry, is_tracked, now, git_root);
            if entry.sent.as_ref() != Some(&summary) {
                entry.sent = Some(summary.clone());
                changed.push(summary);
            }
        }
        (changed, removed)
    }

    /// Detected git/PR actions per session since the last call.
    pub fn take_detected(&mut self) -> Vec<(String, String, Value)> {
        let mut out = Vec::new();
        for (id, entry) in &mut self.entries {
            if let Some(tail) = &mut entry.tail {
                for (kind, data) in tail.state.detected.drain(..) {
                    out.push((id.clone(), kind, data));
                }
            }
        }
        out
    }

    fn scan(&mut self, tracked: &HashSet<String>, seen: &mut HashSet<String>) {
        let Ok(projects) = std::fs::read_dir(self.dir.join("projects")) else { return };
        for project in projects.filter_map(|e| e.ok()) {
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(project.path()) else { continue };
            for file in files.filter_map(|e| e.ok()) {
                let name = file.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".jsonl").filter(|id| crate::index::is_session_id(id)) else { continue };
                seen.insert(id.to_owned());
                let path = file.path();
                self.refresh_meta(id, Some(path), tracked.contains(id));
            }
        }
    }

    fn refresh_meta(&mut self, id: &str, path: Option<PathBuf>, tracked: bool) {
        let entry = self.entries.entry(id.to_owned()).or_insert_with(|| Entry::new(None));
        if path.is_some() {
            entry.path = path;
        }
        let Some(path) = entry.path.clone() else { return };
        let Ok(metadata) = std::fs::metadata(&path) else { return };
        let current = stamp(&metadata);
        if entry.stamp == Some(current) {
            return;
        }
        entry.stamp = Some(current);
        entry.meta.updated_ms = system_ms(current.0);
        if !entry.meta.head_done {
            parse_head(&path, &mut entry.meta);
        }
        // Tracked sessions read their titles while tailing.
        if !tracked {
            parse_title_tail(&path, current.1, &mut entry.meta);
        }
    }

    fn refresh_tail(&mut self, id: &str) {
        let mut warnings = Vec::new();
        let Some(entry) = self.entries.get_mut(id) else { return };
        let tail = entry.tail.get_or_insert_with(new_tail);
        let Some(path) = entry.path.clone() else { return };
        let Ok(metadata) = std::fs::metadata(&path) else { return };
        let size = metadata.len();
        if size != tail.offset {
            // A shrunk or replaced file is read again from the start.
            let identity = Identity::read(&path, &metadata);
            let replaced = match (&tail.identity, &identity) {
                (Some(old), Some(new)) => !old.same_file(new),
                _ => false,
            };
            if size < tail.offset || replaced {
                *tail = new_tail();
            }
            tail.identity = identity;
        }
        if size > tail.offset {
            let start = tail.offset.max(size.saturating_sub(INITIAL_TAIL));
            let skipped = start > tail.offset;
            if skipped {
                tail.partial.clear();
                tail.skipping = true;
                tail.truncated = true;
                tail.state.reset_unknown();
            }
            match read_range(&path, start, size - start) {
                Err(error) => warnings.push(format!("claude {}: {error}", super::short_id(id))),
                Ok(buffer) => {
                    let mut data = std::mem::take(&mut tail.partial);
                    data.extend_from_slice(&buffer);
                    let mut slice = data.as_slice();
                    if tail.skipping {
                        // The rest of a line whose start was not read (or was too long).
                        slice = match slice.iter().position(|&b| b == b'\n') {
                            Some(pos) => {
                                tail.skipping = false;
                                &slice[pos + 1..]
                            }
                            None => &[],
                        };
                    }
                    let complete = slice.iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
                    let mut bad = 0;
                    for line in slice[..complete].split(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(line);
                        let line = line.trim();
                        if !line.is_empty() && !tail.state.apply(line) {
                            bad += 1;
                        }
                    }
                    let rest = &slice[complete..];
                    if rest.len() > MAX_LINE {
                        // Never buffer a line without bound: skip to its end.
                        bad += 1;
                        tail.skipping = true;
                    } else {
                        tail.partial = rest.to_vec();
                    }
                    if bad > 0 {
                        warnings.push(format!("claude {}: {bad} malformed transcript line(s)", super::short_id(id)));
                    }
                    tail.offset = start + buffer.len() as u64;
                }
            }
        }
        // Background subagents record their end in their meta file.
        let folder = path.with_extension("");
        let open: Vec<(String, String)> = tail
            .state
            .agent_ids
            .iter()
            .filter(|(_, key)| tail.state.agents.iter().any(|a| &a.key == *key && a.status == AgentStatus::Working))
            .map(|(agent, key)| (agent.clone(), key.clone()))
            .collect();
        for (agent_id, key) in open {
            let meta_path = folder.join("subagents").join(format!("agent-{agent_id}.meta.json"));
            let Ok(text) = std::fs::read_to_string(&meta_path) else { continue };
            if serde_json::from_str::<Value>(&text).is_ok_and(|v| v["finished"].as_bool() == Some(true))
                && let Some(agent) = tail.state.agents.iter_mut().find(|a| a.key == key)
            {
                agent.status = AgentStatus::Finished;
                agent.finished_ms = std::fs::metadata(&meta_path).ok().map(|m| system_ms(m.modified().ok()));
            }
        }
        for warning in warnings {
            self.warn(warning);
        }
    }

    fn git_root(&mut self, cwd: &str) -> String {
        if cwd.is_empty() {
            return String::new();
        }
        self.git_roots.entry(cwd.to_owned()).or_insert_with(|| find_git_root(Path::new(cwd))).clone()
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < 50 {
            self.warnings.push(message);
        }
    }
}

/// The folder holding `.git` at or above `dir` (empty when none).
pub fn find_git_root(dir: &Path) -> String {
    dir.ancestors().find(|d| d.join(".git").exists()).map(|d| d.display().to_string()).unwrap_or_default()
}

/// Remembers when the pid file reported the end of a turn.
fn note_status(entry: &mut Entry) {
    let Some(pid) = &entry.pid else {
        entry.last_status.clear();
        return;
    };
    let busy = |s: &str| matches!(s, "busy" | "waiting");
    if busy(&pid.status) {
        entry.status_finished = None;
    } else if busy(&entry.last_status) {
        entry.status_finished = Some(if pid.status_ms > 0 { pid.status_ms } else { now_ms() });
    }
    entry.last_status.clone_from(&pid.status);
}

fn new_tail() -> Tail {
    // Only actions from now on become events (the history is replayed for status).
    let state = ClaudeState { since_ms: now_ms().saturating_sub(5_000), ..Default::default() };
    Tail { offset: 0, partial: Vec::new(), skipping: false, identity: None, state, truncated: false }
}

fn summarize(id: &str, entry: &Entry, tracked: bool, now: u64, git_root: String) -> SessionSummary {
    let meta = &entry.meta;
    let mut titles = meta.titles.clone();
    let mut branch = meta.branch.clone();
    let mut cwd = meta.cwd.clone();
    if let Some(tail) = &entry.tail {
        titles.merge(&tail.state.titles);
        if !tail.state.branch.is_empty() {
            branch.clone_from(&tail.state.branch);
        }
        if cwd.is_empty() {
            cwd.clone_from(&tail.state.cwd);
        }
    }
    let pid = entry.pid.as_ref();
    if cwd.is_empty() {
        cwd = pid.map(|p| p.cwd.clone()).unwrap_or_default();
    }
    let name = titles
        .best()
        .map(str::to_owned)
        .or_else(|| (!meta.first_prompt.is_empty()).then(|| meta.first_prompt.clone()))
        .or_else(|| pid.map(|p| p.name.clone()).filter(|n| !n.is_empty()))
        .unwrap_or_else(|| super::short_id(id).to_owned());
    let mut summary = SessionSummary {
        id: id.to_owned(),
        name,
        cwd,
        branch,
        git_root,
        client: "claude-code".to_owned(),
        created_ms: if meta.created_ms > 0 { meta.created_ms } else { pid.map(|p| p.started_ms).unwrap_or(0) },
        updated_ms: meta.updated_ms.max(pid.map(|p| p.updated_ms).unwrap_or(0)),
        tracked,
        live: pid.is_some(),
        ..Default::default()
    };
    let mut pending = 0usize;
    if let Some(tail) = &entry.tail {
        let (activity, finished) = tail.state.derived(now);
        summary.activity = activity;
        summary.finished_ms = finished;
        pending = tail.state.pending.len();
        summary.last_event_ms = tail.state.last_event_ms;
        summary.truncated = tail.truncated;
        summary.agents = tail.state.agents.clone();
        if let Some(pid) = pid {
            // Subagents of an earlier process cannot still be working.
            for agent in summary.agents.iter_mut().filter(|a| a.status == AgentStatus::Working) {
                if tail.state.agent_started.get(&agent.key).is_some_and(|&t| t.saturating_add(2_000) < pid.started_ms) {
                    agent.status = AgentStatus::Unknown;
                }
            }
        }
    }
    // The running process knows best whether a turn is on and whether it waits for you.
    if let Some(pid) = pid {
        let aborted = entry.tail.as_ref().is_some_and(|t| t.state.aborted);
        match pid.status.as_str() {
            "busy" | "waiting" => {
                summary.activity = Activity::Working;
                summary.finished_ms = None;
            }
            "idle" | "shell" => {
                pending = 0;
                if summary.activity == Activity::Working || (summary.activity == Activity::Unknown && entry.status_finished.is_some()) {
                    summary.activity = Activity::Idle;
                    summary.finished_ms = if aborted { None } else { entry.status_finished };
                } else if summary.activity == Activity::Unknown {
                    summary.activity = Activity::Idle;
                }
            }
            _ => {}
        }
        if pid.needs_you() {
            pending = pending.max(1);
        }
    }
    summary.pending = pending.min(u16::MAX as usize) as u16;
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const ID: &str = "12345678-1234-1234-1234-123456789abc";

    fn fixture() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cc-claude-{}", uuid::Uuid::new_v4()));
        let project = dir.join("projects").join(project_dir_name(r"C:\repos\demo"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        (dir, project.join(format!("{ID}.jsonl")))
    }

    fn tracked() -> HashSet<String> {
        [ID.to_owned()].into_iter().collect()
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn rec(kind: &str, time: &str, rest: &str) -> String {
        format!(
            "{{\"parentUuid\":null,\"isSidechain\":false,\"type\":\"{kind}\",\"uuid\":\"u\",\"timestamp\":\"{time}\",\"sessionId\":\"{ID}\",\"cwd\":\"C:\\\\repos\\\\demo\",\"gitBranch\":\"main\"{rest}}}\n"
        )
    }

    fn prompt(time: &str, text: &str) -> String {
        rec("user", time, &format!(",\"message\":{{\"role\":\"user\",\"content\":{}}}", serde_json::to_string(text).unwrap()))
    }

    fn tool_use(time: &str, id: &str, name: &str, input: &str) -> String {
        rec(
            "assistant",
            time,
            &format!(",\"message\":{{\"role\":\"assistant\",\"stop_reason\":\"tool_use\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\",\"input\":{input}}}]}}"),
        )
    }

    fn tool_result(time: &str, id: &str, error: bool, result: &str) -> String {
        rec(
            "user",
            time,
            &format!(",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"{id}\",\"is_error\":{error},\"content\":\"out\"}}]}},\"toolUseResult\":{result}"),
        )
    }

    fn end_turn(time: &str) -> String {
        rec("assistant", time, ",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"Done.\"}]}")
    }

    fn turn_duration(time: &str) -> String {
        rec("system", time, ",\"subtype\":\"turn_duration\",\"durationMs\":1000")
    }

    fn ms(text: &str) -> u64 {
        parse_rfc3339_ms(text).unwrap()
    }

    #[test]
    fn turns_tools_questions_and_turn_end() {
        let mut state = ClaudeState::default();
        state.apply(&prompt("2026-10-27T10:00:00Z", "fix the bug"));
        assert_eq!(state.activity, Activity::Working);
        state.apply(&tool_use("2026-10-27T10:00:01Z", "q1", "AskUserQuestion", "{\"questions\":[]}"));
        assert_eq!(state.pending.len(), 1, "a question waits for you");
        state.apply(&tool_result("2026-10-27T10:00:05Z", "q1", false, "{}"));
        assert!(state.pending.is_empty());
        assert_eq!(state.activity, Activity::Working);
        state.apply(&end_turn("2026-10-27T10:00:06Z"));
        assert_eq!(state.derived(ms("2026-10-27T10:00:07Z")).0, Activity::Working, "settles only after silence");
        assert_eq!(state.derived(ms("2026-10-27T10:00:06Z") + SETTLE_MS), (Activity::Idle, Some(ms("2026-10-27T10:00:06Z"))));
        state.apply(&turn_duration("2026-10-27T10:00:06.500Z"));
        assert_eq!(state.derived(0), (Activity::Idle, Some(ms("2026-10-27T10:00:06.500Z"))));
        // Local commands are not turns; an interrupt has nothing to review.
        state.apply(&prompt("2026-10-27T10:01:00Z", "<command-name>/clear</command-name>"));
        assert_eq!(state.activity, Activity::Idle);
        state.apply(&prompt("2026-10-27T10:02:00Z", "again"));
        state.apply(&prompt("2026-10-27T10:02:05Z", "[Request interrupted by user]"));
        assert_eq!(state.derived(u64::MAX), (Activity::Idle, None));
        state.apply(&prompt("2026-10-27T10:03:00Z", "third"));
        state.apply(&rec("assistant", "2026-10-27T10:03:01Z", ",\"isApiErrorMessage\":true,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"API Error\"}]}"));
        assert_eq!(state.activity, Activity::Failed);
        state.apply(&turn_duration("2026-10-27T10:03:02Z"));
        assert_eq!(state.activity, Activity::Failed, "a failed turn stays failed");
    }

    #[test]
    fn subagents_start_launch_in_background_and_finish() {
        let mut state = ClaudeState::default();
        state.apply(&prompt("2026-10-27T10:00:00Z", "research"));
        state.apply(&tool_use("2026-10-27T10:00:01Z", "t1", "Agent", r#"{"description":"Find callers","subagent_type":"Explore","prompt":"x"}"#));
        state.apply(&tool_use("2026-10-27T10:00:01Z", "t2", "Agent", r#"{"description":"Sync one","prompt":"y"}"#));
        state.apply(&tool_result("2026-10-27T10:00:02Z", "t1", false, r#"{"isAsync":true,"status":"async_launched","agentId":"a1"}"#));
        state.apply(&tool_result("2026-10-27T10:00:09Z", "t2", false, r#"{"status":"completed","agentId":"a2"}"#));
        assert_eq!(state.agents.len(), 2);
        assert_eq!((state.agents[0].name.as_str(), state.agents[0].description.as_str(), state.agents[0].status), ("Explore", "Find callers", AgentStatus::Working));
        assert_eq!((state.agents[1].name.as_str(), state.agents[1].status), ("Subagent", AgentStatus::Finished));
        state.apply(&end_turn("2026-10-27T10:00:10Z"));
        state.apply(&turn_duration("2026-10-27T10:00:10Z"));
        assert_eq!(state.agents[0].status, AgentStatus::Working, "background agent outlives the turn");
        state.apply(&prompt("2026-10-27T10:05:00Z", "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>"));
        assert_eq!(state.agents[0].status, AgentStatus::Finished);
        assert_eq!(state.agents[0].finished_ms, Some(ms("2026-10-27T10:05:00Z")));
        state.apply(&tool_use("2026-10-27T10:05:01Z", "t3", "Agent", r#"{"description":"Bad","prompt":"z"}"#));
        state.apply(&prompt("2026-10-27T10:05:02Z", "<task-notification><tool-use-id>t3</tool-use-id><status>failed</status></task-notification>"));
        assert_eq!(state.agents[2].status, AgentStatus::Failed);
        for i in 0..20 {
            state.apply(&tool_use("2026-10-27T10:06:00Z", &format!("x{i}"), "Task", "{}"));
        }
        assert_eq!(state.agents.len(), MAX_AGENTS);
    }

    #[test]
    fn git_push_and_pr_creation_become_events_after_tracking_started() {
        let mut state = ClaudeState { since_ms: ms("2026-10-27T10:00:00Z"), ..Default::default() };
        state.apply(&tool_use("2026-10-27T10:05:00Z", "p1", "Bash", r#"{"command":"git push -u origin feature/x","description":"push"}"#));
        state.apply(&tool_result(
            "2026-10-27T10:05:03Z",
            "p1",
            false,
            r#"{"stdout":"","stderr":"To https://github.com/o/r.git\n * [new branch]      feature/x -> feature/x","interrupted":false}"#,
        ));
        state.apply(&tool_use("2026-10-27T10:06:00Z", "p2", "Bash", r#"{"command":"gh pr create --fill"}"#));
        state.apply(&tool_result("2026-10-27T10:06:02Z", "p2", false, r#"{"stdout":"https://github.com/o/r/pull/42\n","stderr":""}"#));
        state.apply(&tool_use("2026-10-27T10:07:00Z", "p3", "Bash", r#"{"command":"git push"}"#));
        state.apply(&tool_result("2026-10-27T10:07:01Z", "p3", true, r#"{"stdout":"","stderr":"error: failed to push"}"#));
        assert_eq!(state.detected.len(), 2);
        assert_eq!((state.detected[0].0.as_str(), state.detected[0].1["branch"].as_str()), ("pr.pushed", Some("feature/x")));
        assert_eq!((state.detected[1].0.as_str(), state.detected[1].1["url"].as_str()), ("pr.created", Some("https://github.com/o/r/pull/42")));
        let mut replay = ClaudeState { since_ms: ms("2026-10-27T11:00:00Z"), ..Default::default() };
        replay.apply(&tool_use("2026-10-27T10:05:00Z", "p1", "Bash", r#"{"command":"git push"}"#));
        replay.apply(&tool_result("2026-10-27T10:05:03Z", "p1", false, r#"{"stdout":"To x\n a..b  m -> m"}"#));
        assert!(replay.detected.is_empty(), "history is never re-sent");
    }

    #[test]
    fn lists_sessions_with_titles_and_tails_tracked_ones_incrementally() {
        let (dir, transcript) = fixture();
        append(&transcript, "{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"x\"}\n");
        append(&transcript, &prompt("2026-10-27T10:00:00Z", "Fix the flaky test"));
        append(&transcript, &tool_use("2026-10-27T10:00:01Z", "b1", "Bash", r#"{"command":"cargo test"}"#));
        let mut history = ClaudeHistory::new(dir.clone());
        let (changed, _) = history.poll(&HashSet::new(), std::time::Duration::ZERO);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "Fix the flaky test", "first prompt until a title exists");
        assert_eq!((changed[0].cwd.as_str(), changed[0].branch.as_str(), changed[0].client.as_str()), (r"C:\repos\demo", "main", "claude-code"));
        assert_eq!(changed[0].created_ms, ms("2026-10-27T10:00:00Z"));
        assert_eq!(changed[0].activity, Activity::Unknown, "untracked sessions are not tailed");
        append(&transcript, "{\"type\":\"ai-title\",\"aiTitle\":\"Flaky test fix\",\"sessionId\":\"x\"}\n");
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].name, "Flaky test fix");
        assert_eq!(changed[0].activity, Activity::Working);
        assert!(!changed[0].live);
        // A partial line waits for its end.
        append(&transcript, &tool_result("2026-10-27T10:00:05Z", "b1", false, r#"{"stdout":"ok"}"#));
        let done = end_turn("2026-10-27T10:00:06Z") + &turn_duration("2026-10-27T10:00:06Z");
        let (first, second) = done.split_at(40);
        append(&transcript, first);
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].activity, Activity::Working);
        append(&transcript, second);
        append(&transcript, "{\"type\":\"custom-title\",\"customTitle\":\"My name\",\"sessionId\":\"x\"}\n");
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].activity, Activity::Idle);
        assert_eq!(changed[0].finished_ms, Some(ms("2026-10-27T10:00:06Z")));
        assert_eq!(changed[0].name, "My name", "custom titles win");
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert!(changed.is_empty(), "unchanged sessions are not resent");
        std::fs::remove_file(&transcript).unwrap();
        let (_, removed) = history.poll(&HashSet::new(), std::time::Duration::ZERO);
        assert_eq!(removed, vec![ID.to_owned()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn huge_transcripts_start_near_the_end_and_are_flagged() {
        let (dir, transcript) = fixture();
        let noise = rec("attachment", "2026-10-27T09:00:00Z", &format!(",\"attachment\":{{\"type\":\"x\",\"text\":\"{}\"}}", "y".repeat(2000)));
        append(&transcript, &prompt("2026-10-27T09:00:00Z", "old prompt"));
        append(&transcript, &noise.repeat(700));
        append(&transcript, &tool_use("2026-10-27T10:00:00Z", "a", "Agent", r#"{"description":"Late","subagent_type":"Plan"}"#));
        let mut history = ClaudeHistory::new(dir.clone());
        let (changed, _) = history.poll(&tracked(), std::time::Duration::ZERO);
        assert!(changed[0].truncated);
        assert_eq!(changed[0].agents[0].name, "Plan");
        assert_eq!(changed[0].name, "old prompt", "the head still names it");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pid_files_make_sessions_live_and_report_waiting_and_turn_end() {
        let (dir, transcript) = fixture();
        append(&transcript, &prompt("2026-10-27T10:00:00Z", "go"));
        let me = std::process::id();
        let started = canvas_sys::process::process_start_ms(me).unwrap();
        let filetime = (started + 11_644_473_600_000) * 10_000;
        let write = |status: &str, waiting: &str, at: u64| {
            let text = format!(
                r#"{{"pid":{me},"sessionId":"{ID}","cwd":"C:\\repos\\demo","startedAt":{started},"procStart":"{filetime}","status":"{status}","waitingFor":"{waiting}","updatedAt":{at},"statusUpdatedAt":{at},"name":"demo-1"}}"#
            );
            std::fs::write(dir.join("sessions").join(format!("{me}.json")), text).unwrap();
        };
        // Stale: a pid file whose process start does not match is ignored.
        let stale = format!(r#"{{"pid":{me},"sessionId":"{ID}","cwd":"C:\\x","startedAt":1,"procStart":"1","status":"busy"}}"#);
        std::fs::write(dir.join("sessions").join(format!("{me}.json")), stale).unwrap();
        let mut history = ClaudeHistory::new(dir.clone());
        let (changed, _) = history.poll(&tracked(), std::time::Duration::ZERO);
        assert!(!changed[0].live, "recycled pid");
        assert_eq!(changed[0].activity, Activity::Working);
        write("busy", "", 1_000);
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert!(changed[0].live);
        assert_eq!((changed[0].activity, changed[0].pending), (Activity::Working, 0));
        write("waiting", "permission prompt", 2_000);
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].pending), (Activity::Working, 1), "needs you");
        write("busy", "", 3_000);
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].pending, 0);
        // The process says the turn ended before the transcript does.
        write("idle", "", 4_000);
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].finished_ms), (Activity::Idle, Some(4_000)));
        append(&transcript, &end_turn("2026-10-27T10:00:06Z"));
        append(&transcript, &turn_duration("2026-10-27T10:00:06Z"));
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!((changed[0].activity, changed[0].finished_ms), (Activity::Idle, Some(ms("2026-10-27T10:00:06Z"))));
        // A live session without a transcript yet (no prompt) is listed too.
        std::fs::remove_file(&transcript).unwrap();
        let mut fresh = ClaudeHistory::new(dir.clone());
        let (changed, _) = fresh.poll(&HashSet::new(), std::time::Duration::ZERO);
        assert_eq!((changed[0].name.as_str(), changed[0].live, changed[0].cwd.as_str()), ("demo-1", true, r"C:\repos\demo"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_launched_session_is_followed_from_its_pid_file_to_its_transcript() {
        let (dir, transcript) = fixture();
        let mut history = ClaudeHistory::new(dir.clone());
        let (changed, _) = history.poll(&tracked(), std::time::Duration::ZERO);
        assert!(changed.is_empty(), "not a Claude session (yet)");
        let me = std::process::id();
        let started = canvas_sys::process::process_start_ms(me).unwrap();
        let pid = format!(
            r#"{{"pid":{me},"sessionId":"{ID}","cwd":"C:\\repos\\demo","startedAt":{started},"procStart":"{}","status":"busy","name":"probe"}}"#,
            (started + 11_644_473_600_000) * 10_000
        );
        std::fs::write(dir.join("sessions").join(format!("{me}.json")), pid).unwrap();
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!((changed[0].name.as_str(), changed[0].live, changed[0].activity), ("probe", true, Activity::Working));
        append(&transcript, &prompt("2026-10-27T10:00:00Z", "first prompt"));
        append(&transcript, &tool_use("2026-10-27T10:00:01Z", "q", "AskUserQuestion", "{}"));
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].pending, 1, "the transcript is tailed without waiting for a full scan");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pid_file_parsing_accepts_numbers_or_strings() {
        let file = PidFile::parse(r#"{"pid":"42","sessionId":"s","procStart":134372675241511857,"status":"shell","waitingFor":"dialog open"}"#).unwrap();
        assert_eq!((file.pid, file.proc_start, file.needs_you()), (42, Some(134372675241511857), false));
        assert!(PidFile::parse(r#"{"pid":1}"#).is_none());
        assert!(!PidFile { pid: 0, ..Default::default() }.alive());
        assert!(PidFile::parse(r#"{"pid":4294967297,"sessionId":"s"}"#).is_none(), "no silent truncation to pid 1");
    }

    #[test]
    fn malformed_pid_files_are_ignored_without_panicking() {
        let (dir, _) = fixture();
        let me = std::process::id();
        let started = canvas_sys::process::process_start_ms(me).unwrap();
        let file = |id: &str| {
            format!(r#"{{"pid":{me},"sessionId":{},"cwd":"C:\\x","startedAt":{started},"procStart":"{}","status":"busy"}}"#, serde_json::to_string(id).unwrap(), (started + 11_644_473_600_000) * 10_000)
        };
        let sessions = dir.join("sessions");
        for (i, text) in [
            "not json".to_owned(),
            "{\"pid\":".to_owned(),
            r#"{"pid":18446744073709551615,"sessionId":"x"}"#.to_owned(),
            r#"{"pid":-1,"sessionId":"x"}"#.to_owned(),
            r#"[1,2,3]"#.to_owned(),
            file("abc"),
            file(""),
            file(r"..\..\evil"),
            file("12345678-1234-1234-1234-12345678\u{e9}abc"),
        ]
        .iter()
        .enumerate()
        {
            std::fs::write(sessions.join(format!("{me}.json")), text).unwrap();
            assert!(live_sessions(&dir).is_empty(), "case {i}: {text}");
            let mut history = ClaudeHistory::new(dir.clone());
            let (changed, _) = history.poll(&HashSet::new(), std::time::Duration::ZERO);
            assert!(changed.is_empty(), "case {i}");
        }
        std::fs::write(sessions.join("4294967297.json"), file(ID)).unwrap();
        std::fs::write(sessions.join(format!("{me}.json")), file(ID)).unwrap();
        assert_eq!(live_sessions(&dir).keys().collect::<Vec<_>>(), [ID], "a valid file still counts");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn short_ids_never_panic_in_messages() {
        let entry = Entry::new(None);
        assert_eq!(summarize("abc", &entry, false, 0, String::new()).name, "abc");
        assert_eq!(summarize(ID, &entry, false, 0, String::new()).name, "12345678");
    }

    #[test]
    fn a_replaced_transcript_is_read_again_from_the_start() {
        let (dir, transcript) = fixture();
        append(&transcript, &prompt("2026-10-27T10:00:00Z", "first"));
        append(&transcript, &tool_use("2026-10-27T10:00:01Z", "q1", "AskUserQuestion", "{}"));
        let mut history = ClaudeHistory::new(dir.clone());
        let (changed, _) = history.poll(&tracked(), std::time::Duration::ZERO);
        assert_eq!(changed[0].pending, 1);
        // Same session, rewritten with different (and longer) content.
        let replacement = rec("system", "2026-10-27T11:00:00Z", ",\"subtype\":\"compact\"") + &prompt("2026-10-27T11:00:01Z", "second") + &end_turn("2026-10-27T11:00:02Z") + &turn_duration("2026-10-27T11:00:02Z");
        assert!(replacement.len() as u64 > std::fs::metadata(&transcript).unwrap().len());
        std::fs::write(&transcript, &replacement).unwrap();
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!((changed[0].pending, changed[0].activity), (0, Activity::Idle), "the old question is gone");
        assert_eq!(changed[0].finished_ms, Some(ms("2026-10-27T11:00:02Z")));
        // Growth of the same file is still read incrementally.
        append(&transcript, &prompt("2026-10-27T11:01:00Z", "third"));
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].activity, Activity::Working);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_endless_line_is_skipped_not_buffered() {
        let (dir, transcript) = fixture();
        append(&transcript, &prompt("2026-10-27T10:00:00Z", "go"));
        let mut history = ClaudeHistory::new(dir.clone());
        history.poll(&tracked(), std::time::Duration::ZERO);
        // One line written in pieces smaller than a read, longer than the cap.
        append(&transcript, "{\"type\":\"attachment\",\"text\":\"");
        let piece = "z".repeat(900_000);
        for _ in 0..10 {
            append(&transcript, &piece);
            history.poll(&tracked(), std::time::Duration::from_secs(60));
            let tail = history.entries[ID].tail.as_ref().unwrap();
            assert!(tail.partial.len() <= MAX_LINE, "{} bytes buffered", tail.partial.len());
        }
        assert!(history.entries[ID].tail.as_ref().unwrap().skipping);
        append(&transcript, "\"}\n");
        append(&transcript, &tool_use("2026-10-27T10:00:05Z", "q", "AskUserQuestion", "{}"));
        let (changed, _) = history.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].pending, 1, "lines after the long one are read");
        assert!(std::mem::take(&mut history.warnings).iter().any(|w| w.contains("malformed")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subagent_ids_must_be_safe_file_name_parts() {
        let mut state = ClaudeState::default();
        state.apply(&tool_use("2026-10-27T10:00:01Z", "t1", "Agent", r#"{"description":"x"}"#));
        state.apply(&tool_use("2026-10-27T10:00:01Z", "t2", "Agent", r#"{"description":"y"}"#));
        state.apply(&tool_result("2026-10-27T10:00:02Z", "t1", false, r#"{"status":"async_launched","agentId":"..\\..\\x"}"#));
        state.apply(&tool_result("2026-10-27T10:00:02Z", "t2", false, r#"{"status":"async_launched","agentId":"a_1-B"}"#));
        assert_eq!(state.agent_ids.keys().collect::<Vec<_>>(), ["a_1-B"]);
        assert!(!is_agent_id("") && !is_agent_id("a/b") && !is_agent_id(&"a".repeat(129)) && is_agent_id("abc123"));
    }

    #[test]
    fn project_folder_names_replace_every_other_character() {
        assert_eq!(project_dir_name(r"C:\repos\coop-zombie-extraction"), "C--repos-coop-zombie-extraction");
        assert_eq!(project_dir_name(r"C:\Users\me\My Repo.v2"), "C--Users-me-My-Repo-v2");
    }
}
