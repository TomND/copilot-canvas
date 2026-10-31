//! Incremental, read-only index of Copilot CLI session history.
//!
//! Metadata is reparsed only when `workspace.yaml` changes; events are tailed
//! from the last offset and only for sessions a client tracks.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::paths::{now_ms, parse_rfc3339_ms};
use crate::protocol::{Activity, AgentStatus, AgentSummary, SessionSummary};

const INITIAL_TAIL: u64 = 1024 * 1024;
const MAX_AGENTS: usize = 12;
/// Silence after a turn ends before the session counts as finished.
const SETTLE_MS: u64 = 4_000;

pub use canvas_model::agent::is_session_id;

#[derive(Default, Debug, Clone)]
struct Meta {
    name: String,
    cwd: String,
    branch: String,
    repository: String,
    git_root: String,
    client: String,
    created_ms: u64,
    updated_ms: u64,
}

#[derive(Default, Debug, Clone)]
pub struct ActivityState {
    pub activity: Activity,
    pub finished_ms: Option<u64>,
    pub last_turn_end_ms: Option<u64>,
    pub last_event_ms: u64,
    pub pending: HashSet<String>,
    pub open_tools: HashSet<String>,
    pub agents: Vec<AgentSummary>,
    /// Active recurring schedules: id → interval (ms). They live in the CLI process.
    pub schedules: std::collections::BTreeMap<u64, u64>,
    /// Events before this time are history: never turned into bus events.
    pub since_ms: u64,
    /// Started tool calls that may become events (call id → tool, arguments).
    pending_calls: HashMap<String, (String, serde_json::Value)>,
    /// Detected git/PR actions (type, data) waiting for the bus.
    pub detected: Vec<(String, serde_json::Value)>,
}

/// Tool calls worth watching: shell commands with git push / gh pr / az repos pr,
/// and pull request tools (Azure DevOps and GitHub MCP servers).
fn watch_call(tool: &str, args: &serde_json::Value) -> bool {
    let command = args["command"].as_str().unwrap_or_default().to_ascii_lowercase();
    command.contains("git push") || command.contains("gh pr ") || command.contains("az repos pr ") || tool.contains("pull_request")
}

/// The bus event for a successful tool call, if it is a git or PR action.
pub fn detect(tool: &str, args: &serde_json::Value, result: &str) -> Option<(String, serde_json::Value)> {
    use serde_json::json;
    let text = |v: &serde_json::Value| v.as_str().map(str::to_owned).or_else(|| v.as_u64().map(|n| n.to_string())).unwrap_or_default();
    let command = args["command"].as_str().unwrap_or_default();
    let lower = command.to_ascii_lowercase();
    if !command.is_empty() {
        if lower.contains("git push") {
            if result.contains("Everything up-to-date") || result.contains("[rejected]") || result.contains("error: failed to push") {
                return None;
            }
            let remote = result.lines().find_map(|l| l.trim().strip_prefix("To ")).unwrap_or_default().trim().to_owned();
            let line = result.lines().find(|l| l.contains("->")).unwrap_or_default();
            let branch = line.rsplit("->").next().unwrap_or_default().trim().to_owned();
            let commit = line.split_whitespace().find(|w| w.contains("..")).and_then(|r| r.split("..").last()).unwrap_or_default().trim_matches('.').to_owned();
            return Some(("pr.pushed".into(), json!({ "branch": branch, "remote": remote, "commit": commit })));
        }
        let url = result.split_whitespace().find(|w| w.contains("/pull/") || w.contains("/pullrequest/")).unwrap_or_default().to_owned();
        let pr = result.split("\"pullRequestId\":").nth(1).and_then(|r| r.trim().split(|c: char| !c.is_ascii_digit()).next()).unwrap_or_default().to_owned();
        if lower.contains("gh pr create") || lower.contains("az repos pr create") {
            return Some(("pr.created".into(), json!({ "pr": pr, "url": url })));
        }
        if lower.contains("gh pr merge") || (lower.contains("az repos pr update") && lower.contains("--status completed")) {
            return Some(("pr.merged".into(), json!({ "pr": pr, "url": url })));
        }
        if lower.contains("az repos pr update") {
            return Some(("pr.updated".into(), json!({ "pr": pr, "url": url })));
        }
        return None;
    }
    let action = args["action"].as_str().unwrap_or_default();
    let pr = text(&args["pullRequestId"]).max(text(&args["pullNumber"]));
    let repo = text(&args["repositoryId"]).max(text(&args["repo"]));
    if tool.ends_with("pull_request_thread_write") && matches!(action, "create" | "reply") {
        let comment: String = text(&args["content"]).chars().take(300).collect();
        return Some(("pr.commented".into(), json!({ "pr": pr, "repo": repo, "comment": comment })));
    }
    let created = |id: String| {
        let id = if id.is_empty() { result.split("\"pullRequestId\":").nth(1).and_then(|r| r.trim().split(|c: char| !c.is_ascii_digit()).next()).unwrap_or_default().to_owned() } else { id };
        Some((
            "pr.created".to_owned(),
            json!({ "pr": id, "repo": repo, "source_branch": text(&args["sourceRefName"]), "target_branch": text(&args["targetRefName"]), "title": text(&args["title"]) }),
        ))
    };
    if tool.ends_with("repo_pull_request_write") {
        return match action {
            "create" => created(pr),
            "update" if text(&args["status"]).eq_ignore_ascii_case("completed") => Some(("pr.merged".into(), json!({ "pr": pr, "repo": repo }))),
            "update" => {
                let change: Vec<&str> = args.as_object().map(|o| o.keys().map(String::as_str).filter(|k| !matches!(*k, "action" | "pullRequestId" | "repositoryId" | "project")).collect()).unwrap_or_default();
                Some(("pr.updated".into(), json!({ "pr": pr, "repo": repo, "change": change.join(", ") })))
            }
            _ => None,
        };
    }
    if tool.ends_with("create_pull_request") {
        return created(pr);
    }
    if tool.ends_with("merge_pull_request") {
        return Some(("pr.merged".into(), json!({ "pr": pr, "repo": repo })));
    }
    None
}

#[derive(Deserialize, Default)]
struct RawEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "agentId", default)]
    agent_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    data: Option<RawData>,
}

#[derive(Deserialize, Default)]
struct RawData {
    #[serde(rename = "toolCallId", default)]
    tool_call_id: Option<String>,
    #[serde(rename = "toolName", default)]
    tool_name: Option<String>,
    #[serde(rename = "requestId", default)]
    request_id: Option<String>,
    #[serde(rename = "agentName", default)]
    agent_name: Option<String>,
    #[serde(rename = "agentDisplayName", default)]
    agent_display_name: Option<String>,
    #[serde(rename = "agentDescription", default)]
    agent_description: Option<String>,
    #[serde(rename = "agentId", default)]
    agent_id: Option<String>,
}

impl ActivityState {
    pub fn apply(&mut self, line: &str) -> bool {
        let Ok(event) = serde_json::from_str::<RawEvent>(line) else { return false };
        let data = event.data.unwrap_or_default();
        let time = event.timestamp.as_deref().and_then(parse_rfc3339_ms);
        if let Some(time) = time {
            self.last_event_ms = self.last_event_ms.max(time);
        }
        let agent_key = event.agent_id.clone().or(data.agent_id.clone()).or(data.tool_call_id.clone());
        // Git and PR actions finished after tracking began become bus events.
        if time.is_some_and(|t| self.since_ms > 0 && t >= self.since_ms) {
            match event.kind.as_str() {
                "tool.execution_start" => {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                    let tool = value["data"]["toolName"].as_str().unwrap_or_default().to_owned();
                    let args = value["data"]["arguments"].clone();
                    if let Some(id) = &data.tool_call_id
                        && watch_call(&tool, &args)
                    {
                        self.pending_calls.insert(id.clone(), (tool, args));
                    }
                }
                "tool.execution_complete" => {
                    if let Some((tool, args)) = data.tool_call_id.as_ref().and_then(|id| self.pending_calls.remove(id)) {
                        let value: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                        let ok = value["data"]["success"].as_bool().unwrap_or(true) && value["data"]["shellExecution"]["exitCode"].as_i64().unwrap_or(0) == 0;
                        let result = match &value["data"]["result"] {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        if ok && let Some(found) = detect(&tool, &args, &result) {
                            self.detected.push(found);
                        }
                    }
                }
                _ => {}
            }
        }
        match event.kind.as_str() {
            "subagent.started" => {
                if let Some(key) = agent_key {
                    let name = data.agent_display_name.or(data.agent_name).unwrap_or_else(|| "Subagent".into());
                    let description = data.agent_description.unwrap_or_default();
                    match self.agents.iter_mut().find(|a| a.key == key) {
                        Some(agent) => {
                            agent.status = AgentStatus::Working;
                            agent.finished_ms = None;
                        }
                        None => self.agents.push(AgentSummary { key, name, description, status: AgentStatus::Working, finished_ms: None }),
                    }
                    if self.agents.len() > MAX_AGENTS {
                        let excess = self.agents.len() - MAX_AGENTS;
                        self.agents.drain(..excess);
                    }
                }
                return true;
            }
            "subagent.completed" | "subagent.failed" => {
                let failed = event.kind == "subagent.failed";
                let found = self.agents.iter_mut().find(|a| Some(&a.key) == agent_key.as_ref());
                if let Some(agent) = found {
                    agent.status = if failed { AgentStatus::Failed } else { AgentStatus::Finished };
                    agent.finished_ms = time;
                }
                return true;
            }
            _ => {}
        }
        // Child-agent turns must not change the parent's own activity.
        if event.agent_id.is_some() {
            return true;
        }
        match event.kind.as_str() {
            "user.message" | "assistant.turn_start" => {
                self.activity = Activity::Working;
                self.finished_ms = None;
                self.last_turn_end_ms = None;
            }
            "assistant.turn_end" => self.last_turn_end_ms = time.or(Some(self.last_event_ms)),
            // Esc: you stopped it, so there is nothing new to review.
            "abort" => {
                self.activity = Activity::Idle;
                self.finished_ms = None;
                self.last_turn_end_ms = None;
                self.pending.clear();
                self.open_tools.clear();
            }
            "session.schedule_created" => {
                let value: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                let data = &value["data"];
                if let Some(id) = data["id"].as_u64()
                    && data["recurring"].as_bool() != Some(false)
                {
                    self.schedules.insert(id, data["intervalMs"].as_u64().unwrap_or(0));
                }
            }
            "session.schedule_cancelled" => {
                let value: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                if let Some(id) = value["data"]["id"].as_u64() {
                    self.schedules.remove(&id);
                }
            }
            "session.start" | "session.resume" => {
                self.activity = Activity::Idle;
                self.finished_ms = None;
                self.pending.clear();
                self.open_tools.clear();
                self.schedules.clear();
                // Subagents of an earlier run cannot still be working.
                for agent in self.agents.iter_mut().filter(|a| a.status == AgentStatus::Working) {
                    agent.status = AgentStatus::Unknown;
                }
            }
            "permission.requested" => {
                if let Some(id) = data.request_id.or(event.id) {
                    self.pending.insert(id);
                }
            }
            "permission.completed" => {
                if let Some(id) = data.request_id {
                    self.pending.remove(&id);
                }
            }
            "tool.execution_start" => {
                if let Some(id) = data.tool_call_id {
                    let tool = data.tool_name.unwrap_or_default();
                    if tool == "ask_user" || tool.ends_with(".ask_user") || tool.ends_with(":ask_user") {
                        self.pending.insert(id.clone());
                    }
                    self.open_tools.insert(id);
                }
            }
            "tool.execution_complete" => {
                if let Some(id) = data.tool_call_id {
                    self.pending.remove(&id);
                    self.open_tools.remove(&id);
                }
            }
            "session.task_complete" | "session.idle" => {
                self.activity = Activity::Idle;
                self.finished_ms = time.or(Some(self.last_event_ms));
                self.last_turn_end_ms = None;
            }
            "session.error" => self.activity = Activity::Failed,
            "session.shutdown" => {
                self.activity = Activity::Stopped;
                self.schedules.clear();
                self.pending.clear();
                self.open_tools.clear();
            }
            _ => {}
        }
        true
    }

    /// Activity including the time-based "turn ended and nothing followed" rule;
    /// Copilot does not always persist an idle event.
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
}

struct Tail {
    offset: u64,
    partial: Vec<u8>,
    state: ActivityState,
    truncated: bool,
    modified: Option<SystemTime>,
}

struct Entry {
    meta: Meta,
    yaml_stamp: (Option<SystemTime>, u64),
    tail: Option<Tail>,
    live: bool,
    sent: Option<SessionSummary>,
}

pub struct Index {
    root: PathBuf,
    entries: HashMap<String, Entry>,
    last_full_scan: Option<std::time::Instant>,
    pub warnings: Vec<String>,
}

fn stamp(path: &Path) -> Option<(Option<SystemTime>, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

fn parse_meta(text: &str) -> Option<Meta> {
    let docs = yaml_rust2::YamlLoader::load_from_str(text).ok()?;
    let doc = docs.first()?;
    let get = |key: &str| -> String {
        match &doc[key] {
            yaml_rust2::Yaml::String(s) => s.clone(),
            yaml_rust2::Yaml::Integer(i) => i.to_string(),
            yaml_rust2::Yaml::Real(r) => r.clone(),
            yaml_rust2::Yaml::Boolean(b) => b.to_string(),
            _ => String::new(),
        }
    };
    Some(Meta {
        name: get("name"),
        cwd: get("cwd"),
        branch: get("branch"),
        repository: get("repository"),
        git_root: get("git_root"),
        client: get("client_name"),
        created_ms: parse_rfc3339_ms(&get("created_at")).unwrap_or(0),
        updated_ms: parse_rfc3339_ms(&get("updated_at")).unwrap_or(0),
    })
}

impl Index {
    pub fn new(root: PathBuf) -> Self {
        Self { root, entries: HashMap::new(), last_full_scan: None, warnings: Vec::new() }
    }

    /// Scans metadata (everything every `full_every`, tracked sessions always)
    /// and tails tracked event logs. Returns changed summaries and removed ids.
    pub fn poll(&mut self, tracked: &HashSet<String>, full_every: std::time::Duration) -> (Vec<SessionSummary>, Vec<String>) {
        let now = now_ms();
        let full = self.last_full_scan.is_none_or(|t| t.elapsed() >= full_every);
        let mut seen = HashSet::new();
        let ids: Vec<String> = if full {
            self.last_full_scan = Some(std::time::Instant::now());
            match std::fs::read_dir(&self.root) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .filter(|name| is_session_id(name))
                    .collect(),
                Err(_) => Vec::new(),
            }
        } else {
            tracked.iter().cloned().collect()
        };
        for id in &ids {
            seen.insert(id.clone());
            self.refresh_meta(id);
        }
        // Tracked sessions may be new (just launched) and not yet in a full scan.
        for id in tracked {
            if !seen.contains(id) && is_session_id(id) {
                self.refresh_meta(id);
            }
        }
        let mut removed = Vec::new();
        if full {
            let gone: Vec<String> =
                self.entries.keys().filter(|id| !seen.contains(*id) && !tracked.contains(*id)).cloned().collect();
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
                self.refresh_live(&id);
            } else if let Some(entry) = self.entries.get_mut(&id) {
                entry.tail = None;
            }
            let entry = self.entries.get_mut(&id).expect("entry exists");
            let summary = summarize(&id, entry, is_tracked, now);
            if entry.sent.as_ref() != Some(&summary) {
                entry.sent = Some(summary.clone());
                changed.push(summary);
            }
        }
        (changed, removed)
    }

    /// Detected git/PR actions per session since the last call.
    pub fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)> {
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

    fn refresh_meta(&mut self, id: &str) {
        let path = self.root.join(id).join("workspace.yaml");
        let Some(current) = stamp(&path) else {
            return;
        };
        if self.entries.get(id).is_some_and(|e| e.yaml_stamp == current) {
            return;
        }
        let meta = match std::fs::read_to_string(&path).ok().and_then(|text| parse_meta(&text)) {
            Some(meta) => meta,
            None => {
                self.warn(format!("{}: unreadable workspace.yaml", &id[..8]));
                Meta::default()
            }
        };
        let entry = self.entries.entry(id.to_owned()).or_insert_with(|| Entry {
            meta: Meta::default(),
            yaml_stamp: (None, 0),
            tail: None,
            live: false,
            sent: None,
        });
        entry.meta = meta;
        entry.yaml_stamp = current;
    }

    fn refresh_live(&mut self, id: &str) {
        let dir = self.root.join(id);
        let live = std::fs::read_dir(&dir)
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.strip_prefix("inuse.")
                        .and_then(|rest| rest.strip_suffix(".lock"))
                        .and_then(|pid| pid.parse::<u32>().ok())
                        .is_some_and(|pid| canvas_sys::process::lock_owner_alive(pid, &e.path()))
                })
            })
            .unwrap_or(false);
        if let Some(entry) = self.entries.get_mut(id) {
            entry.live = live;
        }
    }

    fn refresh_tail(&mut self, id: &str) {
        let path = self.root.join(id).join("events.jsonl");
        let mut warnings = Vec::new();
        let Some(entry) = self.entries.get_mut(id) else { return };
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                entry.tail.get_or_insert_with(new_tail);
                return;
            }
        };
        let size = metadata.len();
        let modified = metadata.modified().ok();
        let tail = entry.tail.get_or_insert_with(new_tail);
        if size < tail.offset {
            // Truncated or replaced history: start over.
            *tail = new_tail();
        }
        if size == tail.offset {
            tail.modified = modified;
            return;
        }
        let start = tail.offset.max(size.saturating_sub(INITIAL_TAIL));
        let skipped = start > tail.offset;
        if skipped {
            tail.partial.clear();
            tail.truncated = true;
            // A dropped completion must not leave stale prompts or working agents.
            tail.state.pending.clear();
            tail.state.open_tools.clear();
            tail.state.activity = Activity::Unknown;
            for agent in &mut tail.state.agents {
                if agent.status == AgentStatus::Working {
                    agent.status = AgentStatus::Unknown;
                }
            }
        }
        let mut buffer = Vec::with_capacity((size - start) as usize);
        let read = File::open(&path).and_then(|mut file| {
            file.seek(SeekFrom::Start(start))?;
            file.take(size - start).read_to_end(&mut buffer)
        });
        if let Err(error) = read {
            warnings.push(format!("{}: {error}", &id[..8]));
        } else {
            let mut data = std::mem::take(&mut tail.partial);
            data.extend_from_slice(&buffer);
            let mut slice = data.as_slice();
            if skipped {
                match slice.iter().position(|&b| b == b'\n') {
                    Some(pos) => slice = &slice[pos + 1..],
                    None => slice = &[],
                }
            }
            let complete = match slice.iter().rposition(|&b| b == b'\n') {
                Some(pos) => pos + 1,
                None => 0,
            };
            for line in slice[..complete].split(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(line);
                let line = line.trim();
                if !line.is_empty() && !tail.state.apply(line) {
                    warnings.push(format!("{}: malformed event line", &id[..8]));
                }
            }
            tail.partial = slice[complete..].to_vec();
            tail.offset = start + buffer.len() as u64;
            tail.modified = modified;
        }
        for warning in warnings {
            self.warn(warning);
        }
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < 50 {
            self.warnings.push(message);
        }
    }
}

fn new_tail() -> Tail {
    // Only actions from now on become events (the history is replayed for status).
    let state = ActivityState { since_ms: now_ms().saturating_sub(5_000), ..Default::default() };
    Tail { offset: 0, partial: Vec::new(), state, truncated: false, modified: None }
}

fn summarize(id: &str, entry: &Entry, tracked: bool, now: u64) -> SessionSummary {
    let meta = &entry.meta;
    let mut summary = SessionSummary {
        id: id.to_owned(),
        name: if meta.name.is_empty() { id[..8].to_owned() } else { meta.name.clone() },
        cwd: meta.cwd.clone(),
        branch: meta.branch.clone(),
        repository: meta.repository.clone(),
        git_root: meta.git_root.clone(),
        client: meta.client.clone(),
        created_ms: meta.created_ms,
        updated_ms: meta.updated_ms,
        tracked,
        live: tracked && entry.live,
        ..Default::default()
    };
    if let Some(tail) = &entry.tail {
        let (activity, finished) = tail.state.derived(now);
        summary.activity = activity;
        summary.finished_ms = finished;
        summary.pending = tail.state.pending.len().min(u16::MAX as usize) as u16;
        summary.last_event_ms = tail.state.last_event_ms;
        summary.truncated = tail.truncated;
        summary.agents = tail.state.agents.clone();
        summary.loop_ms = tail.state.schedules.values().copied().filter(|ms| *ms > 0).min().unwrap_or(if tail.state.schedules.is_empty() { 0 } else { 60_000 });
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const ID: &str = "12345678-1234-1234-1234-123456789abc";

    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("cc-index-{}", uuid::Uuid::new_v4()));
        let dir = root.join(ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workspace.yaml"), "id: x\nname: \"Fixture: one\"\ncwd: C:\\repo\nclient_name: vscode-agent-host\n").unwrap();
        (root, dir)
    }

    fn tracked() -> HashSet<String> {
        [ID.to_owned()].into_iter().collect()
    }

    #[test]
    fn child_turns_do_not_change_parent_and_agents_complete() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"user.message","data":{}}"#);
        state.apply(r#"{"type":"subagent.started","agentId":"c","data":{"toolCallId":"t","agentName":"Explorer"}}"#);
        state.apply(r#"{"type":"session.idle","agentId":"c","data":{}}"#);
        state.apply(r#"{"type":"subagent.completed","agentId":"c","timestamp":"2026-10-20T20:00:00Z","data":{"toolCallId":"t"}}"#);
        assert_eq!(state.activity, Activity::Working);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].status, AgentStatus::Finished);
        assert_eq!(state.agents[0].name, "Explorer");
    }

    #[test]
    fn turn_end_settles_to_idle_only_after_silence_and_without_open_tools() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"assistant.turn_start","timestamp":"2026-10-20T20:00:00Z","data":{}}"#);
        state.apply(r#"{"type":"tool.execution_start","timestamp":"2026-10-20T20:00:01Z","data":{"toolCallId":"a","toolName":"shell"}}"#);
        state.apply(r#"{"type":"assistant.turn_end","timestamp":"2026-10-20T20:00:02Z","data":{}}"#);
        let t = parse_rfc3339_ms("2026-10-20T20:00:02Z").unwrap();
        assert_eq!(state.derived(t + 60_000).0, Activity::Working, "open tool keeps it working");
        state.apply(r#"{"type":"tool.execution_complete","timestamp":"2026-10-20T20:00:02Z","data":{"toolCallId":"a"}}"#);
        assert_eq!(state.derived(t + 1_000).0, Activity::Working);
        assert_eq!(state.derived(t + SETTLE_MS).0, Activity::Idle);
        assert_eq!(state.derived(t + SETTLE_MS).1, Some(t));
    }

    #[test]
    fn git_and_pr_actions_become_events_only_after_tracking_started() {
        let mut state = ActivityState { since_ms: parse_rfc3339_ms("2026-10-27T10:00:00Z").unwrap(), ..Default::default() };
        let push_start = r#"{"type":"tool.execution_start","timestamp":"2026-10-27T10:05:00Z","data":{"toolCallId":"p1","toolName":"powershell","arguments":{"command":"git push -u origin user/me/feature"}}}"#;
        let push_done = r#"{"type":"tool.execution_complete","timestamp":"2026-10-27T10:05:03Z","data":{"toolCallId":"p1","success":true,"shellExecution":{"exitCode":0},"result":"To https://dev.azure.com/org/_git/repo\n   1a2b3c4..5d6e7f8  user/me/feature -> user/me/feature"}}"#;
        state.apply(push_start);
        state.apply(push_done);
        assert_eq!(state.detected.len(), 1);
        assert_eq!(state.detected[0].0, "pr.pushed");
        assert_eq!(state.detected[0].1["branch"], "user/me/feature");
        assert_eq!(state.detected[0].1["commit"], "5d6e7f8");
        assert_eq!(state.detected[0].1["remote"], "https://dev.azure.com/org/_git/repo");
        // Older history is replayed for status but never re-sent.
        let mut replay = ActivityState { since_ms: parse_rfc3339_ms("2026-10-27T11:00:00Z").unwrap(), ..Default::default() };
        replay.apply(push_start);
        replay.apply(push_done);
        assert!(replay.detected.is_empty());
        // Failed calls are not events.
        let mut failed = ActivityState { since_ms: 1, ..Default::default() };
        failed.apply(push_start);
        failed.apply(&push_done.replace(r#""exitCode":0"#, r#""exitCode":1"#));
        assert!(failed.detected.is_empty());
    }

    #[test]
    fn pull_request_tools_map_to_events() {
        use serde_json::json;
        let merged = detect("ado-onedrive-repo_pull_request_write", &json!({ "action": "update", "pullRequestId": 2445894, "status": "completed" }), "{}");
        assert_eq!(merged.unwrap().0, "pr.merged");
        let created = detect("ado-repo_pull_request_write", &json!({ "action": "create", "sourceRefName": "refs/heads/a", "targetRefName": "refs/heads/main", "title": "T" }), r#"{"pullRequestId": 77}"#).unwrap();
        assert_eq!((created.0.as_str(), created.1["pr"].as_str()), ("pr.created", Some("77")));
        let comment = detect("ado-onedrive-repo_pull_request_thread_write", &json!({ "action": "create", "pullRequestId": 5, "content": "nit" }), "{}").unwrap();
        assert_eq!((comment.0.as_str(), comment.1["comment"].as_str()), ("pr.commented", Some("nit")));
        assert!(detect("ado-repo_pull_request_thread", &json!({ "action": "list" }), "{}").is_none(), "reads are not events");
        assert_eq!(detect("powershell", &json!({ "command": "gh pr merge 12 --squash" }), "Merged https://github.com/o/r/pull/12").unwrap().0, "pr.merged");
        assert!(detect("powershell", &json!({ "command": "git push" }), "Everything up-to-date").is_none());
    }

    #[test]
    fn recurring_schedules_make_a_loop_until_cancelled_or_restarted() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"session.schedule_created","timestamp":"2026-10-20T20:00:00Z","data":{"id":1,"intervalMs":900000,"prompt":"check","recurring":true,"origin":"model"}}"#);
        state.apply(r#"{"type":"session.schedule_created","timestamp":"2026-10-20T20:00:01Z","data":{"id":2,"intervalMs":60000,"prompt":"once","recurring":false}}"#);
        assert_eq!(state.schedules.len(), 1, "one-shot schedules are not loops");
        state.apply(r#"{"type":"session.schedule_cancelled","timestamp":"2026-10-20T20:01:00Z","data":{"id":1}}"#);
        assert!(state.schedules.is_empty());
        state.apply(r#"{"type":"session.schedule_created","timestamp":"2026-10-20T20:02:00Z","data":{"id":3,"intervalMs":600000,"recurring":true}}"#);
        state.apply(r#"{"type":"session.shutdown","timestamp":"2026-10-20T20:03:00Z","data":{}}"#);
        assert!(state.schedules.is_empty(), "schedules end with the CLI process");
    }

    #[test]
    fn resume_stops_subagents_of_the_previous_run() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"subagent.started","timestamp":"2026-10-20T20:00:00Z","data":{"toolCallId":"a","agentDisplayName":"Explorer"}}"#);
        assert_eq!(state.agents[0].status, AgentStatus::Working);
        state.apply(r#"{"type":"session.resume","timestamp":"2026-10-20T21:00:00Z","data":{}}"#);
        assert_eq!(state.agents[0].status, AgentStatus::Unknown);
    }

    #[test]
    fn abort_is_idle_with_nothing_to_review() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"assistant.turn_start","timestamp":"2026-10-20T20:00:00Z","data":{}}"#);
        state.apply(r#"{"type":"tool.execution_start","timestamp":"2026-10-20T20:00:01Z","data":{"toolCallId":"a","toolName":"shell"}}"#);
        state.apply(r#"{"type":"assistant.turn_end","timestamp":"2026-10-20T20:00:02Z","data":{}}"#);
        state.apply(r#"{"type":"abort","timestamp":"2026-10-20T20:00:02Z","data":{"reason":"user_abort"}}"#);
        let t = parse_rfc3339_ms("2026-10-20T20:00:02Z").unwrap();
        assert_eq!(state.derived(t + 60_000), (Activity::Idle, None));
    }

    #[test]
    fn permission_and_ask_user_mark_pending() {
        let mut state = ActivityState::default();
        state.apply(r#"{"type":"permission.requested","data":{"requestId":"p"}}"#);
        state.apply(r#"{"type":"tool.execution_start","data":{"toolCallId":"q","toolName":"functions.ask_user"}}"#);
        assert_eq!(state.pending.len(), 2);
        state.apply(r#"{"type":"permission.completed","data":{"requestId":"p"}}"#);
        state.apply(r#"{"type":"tool.execution_complete","data":{"toolCallId":"q"}}"#);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn tails_incrementally_and_waits_for_complete_lines() {
        let (root, dir) = fixture();
        let events = dir.join("events.jsonl");
        std::fs::write(&events, "{\"type\":\"assistant.turn_start\",\"data\":{}}\n{\"type\":\"session.task_").unwrap();
        let mut index = Index::new(root.clone());
        let (changed, _) = index.poll(&tracked(), std::time::Duration::ZERO);
        assert_eq!(changed[0].name, "Fixture: one");
        assert_eq!(changed[0].client, "vscode-agent-host");
        assert_eq!(changed[0].activity, Activity::Working);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&events)
            .unwrap()
            .write_all(b"complete\",\"timestamp\":\"2026-10-20T20:00:00Z\",\"data\":{}}\n")
            .unwrap();
        let (changed, _) = index.poll(&tracked(), std::time::Duration::from_secs(60));
        assert_eq!(changed[0].activity, Activity::Idle);
        let (changed, _) = index.poll(&tracked(), std::time::Duration::from_secs(60));
        assert!(changed.is_empty(), "unchanged sessions are not resent");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn huge_histories_are_bounded_and_flagged() {
        let (root, dir) = fixture();
        let noise = format!("{{\"type\":\"assistant.message\",\"data\":{{\"content\":\"{}\"}}}}\n", "x".repeat(2000));
        let mut text = noise.repeat(700);
        text.push_str("{\"type\":\"subagent.started\",\"agentId\":\"a\",\"data\":{\"agentName\":\"Explorer\"}}\n");
        std::fs::write(dir.join("events.jsonl"), text).unwrap();
        let mut index = Index::new(root.clone());
        let (changed, _) = index.poll(&tracked(), std::time::Duration::ZERO);
        assert!(changed[0].truncated);
        assert_eq!(changed[0].agents[0].name, "Explorer");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_locks_are_not_live_and_untracked_sessions_skip_events() {
        let (root, dir) = fixture();
        std::fs::write(dir.join("inuse.4294967294.lock"), "x").unwrap();
        std::fs::write(dir.join("events.jsonl"), "{\"type\":\"user.message\",\"data\":{}}\n").unwrap();
        let mut index = Index::new(root.clone());
        let (changed, _) = index.poll(&HashSet::new(), std::time::Duration::ZERO);
        assert_eq!(changed[0].activity, Activity::Unknown);
        assert!(!changed[0].tracked);
        let (changed, _) = index.poll(&tracked(), std::time::Duration::ZERO);
        assert!(!changed[0].live);
        assert_eq!(changed[0].activity, Activity::Working);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removed_sessions_are_reported() {
        let (root, dir) = fixture();
        let mut index = Index::new(root.clone());
        index.poll(&HashSet::new(), std::time::Duration::ZERO);
        std::fs::remove_dir_all(&dir).unwrap();
        let (_, removed) = index.poll(&HashSet::new(), std::time::Duration::ZERO);
        assert_eq!(removed, vec![ID.to_owned()]);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod status_audit {
    /// Manual: `cargo test --release status_audit -- --ignored --nocapture`.
    /// Replays recent sessions' histories (read-only) and prints the derived
    /// status with the evidence, to find wrong states.
    #[test]
    #[ignore]
    fn status_audit() {
        let root = dirs::home_dir().unwrap().join(".copilot").join("session-state");
        let now = crate::paths::now_ms();
        let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.path().join("events.jsonl").is_file()).collect();
        dirs.sort_by_key(|e| std::cmp::Reverse(std::fs::metadata(e.path().join("events.jsonl")).and_then(|m| m.modified()).ok()));
        for entry in dirs.into_iter().take(40) {
            let id = entry.file_name().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(entry.path().join("events.jsonl")).unwrap_or_default();
            let mut state = super::ActivityState::default();
            let mut kinds = Vec::new();
            for line in text.lines() {
                state.apply(line);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let agent = if v.get("agentId").is_some_and(|a| !a.is_null()) { "*" } else { "" };
                    kinds.push(format!("{}{agent}", v["type"].as_str().unwrap_or("?")));
                }
            }
            let (activity, finished) = state.derived(now);
            let live = super::super::service::session_live(&root, &id);
            let age = |ms: u64| if ms == 0 { "-".to_owned() } else { format!("{}s", now.saturating_sub(ms) / 1000) };
            println!(
                "{} live={live:<5} {activity:?} pending={} open_tools={} last_event={} finished={} tail=[{}]",
                &id[..8],
                state.pending.len(),
                state.open_tools.len(),
                age(state.last_event_ms),
                finished.map(age).unwrap_or("-".into()),
                kinds.iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join(" > ")
            );
        }
    }
}
