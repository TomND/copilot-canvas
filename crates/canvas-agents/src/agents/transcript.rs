//! Read-only transcripts for the inspector (`Request::Transcript`). Reads are
//! bounded: the newest 2 MiB of a JSONL history (Copilot's `events.jsonl`,
//! Claude Code's transcript and its subagent files) and indexed lookups in
//! OpenCode's database (the newest messages and their parts, large fields
//! cut in SQL). Nothing is written and nothing is started.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use canvas_model::transcript::{Entry, EntryKind, MAX_ARGS, MAX_TEXT, Transcript, clip, short_args};
use rusqlite::OptionalExtension;
use serde_json::Value;

use super::AgentKind;
use crate::paths::{now_ms, parse_rfc3339_ms};
use crate::protocol::{Activity, AgentStatus};

/// Bytes read from the end of a JSONL history.
const TAIL: u64 = 2 << 20;
/// OpenCode messages read at most (the newest).
const OPENCODE_MESSAGES: usize = 150;

/// The transcript a `Request::Transcript` asks for (Copilot's history is in `sessions_dir`).
pub fn read(sessions_dir: &Path, agent: AgentKind, session: &str, parent: Option<&str>, agent_key: Option<&str>, cwd: Option<&str>, limit: usize) -> Result<Transcript> {
    let mut transcript = match agent {
        AgentKind::Copilot => copilot(sessions_dir, session, agent_key)?,
        AgentKind::OpenCode => opencode(&super::opencode_history::db_path(), session, parent)?,
        AgentKind::Claude => claude(&super::claude_history::claude_dir(), session, agent_key, cwd)?,
    };
    transcript.keep_newest(limit);
    Ok(transcript)
}

/// The complete lines of the last `TAIL` bytes of a file; true when its start was skipped.
fn tail_lines(path: &Path) -> Result<(Vec<String>, bool)> {
    let mut file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.take(len - start).read_to_end(&mut bytes)?;
    let mut slice = bytes.as_slice();
    if start > 0 {
        // The first line was cut.
        slice = slice.iter().position(|&b| b == b'\n').map(|p| &slice[p + 1..]).unwrap_or(&[]);
    }
    let lines = slice.split(|&b| b == b'\n').map(|l| String::from_utf8_lossy(l).trim().to_owned()).filter(|l| !l.is_empty()).collect();
    Ok((lines, start > 0))
}

fn text_entry(kind: EntryKind, text: &str, ms: u64) -> Option<Entry> {
    let text = clip(text, MAX_TEXT, false);
    (!text.is_empty()).then(|| Entry { kind, text, ms, ..Default::default() })
}

fn note(text: impl Into<String>, ms: u64, failed: bool) -> Entry {
    Entry { kind: EntryKind::Note, text: text.into(), ms, failed, ..Default::default() }
}

/// The text of a result: a string, its `content` / `text` / `output`
/// fields, or the texts of an array of blocks.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(value_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Object(map) => ["content", "text", "detailedContent", "output", "message"].iter().find_map(|k| map.get(*k)).map(value_text).unwrap_or_default(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The first non-empty lines of a result, after a non-zero exit code.
fn short_result(text: &str, exit: Option<i64>) -> String {
    let body = text.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect::<Vec<_>>().join(" ↵ ");
    match exit {
        Some(code) if code != 0 && body.is_empty() => format!("exit {code}"),
        Some(code) if code != 0 => format!("exit {code} · {body}"),
        _ => body,
    }
}

/// Copilot: `events.jsonl` of `session`. With `agent_key` only that
/// subagent's events (those carrying its `agentId`); without, the session's
/// own (subagent turns left out, their start and end noted).
pub fn copilot(sessions_dir: &Path, session: &str, agent_key: Option<&str>) -> Result<Transcript> {
    anyhow::ensure!(crate::index::is_session_id(session), "not a Copilot session id: {session}");
    let path = sessions_dir.join(session).join("events.jsonl");
    if !path.is_file() {
        return Ok(Transcript { note: "No history yet: it starts with the first prompt.".into(), ..Default::default() });
    }
    let (lines, truncated) = tail_lines(&path)?;
    let mut out = Transcript { truncated, ..Default::default() };
    let mut state = crate::index::ActivityState::default();
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut own_steps = 0;
    for line in &lines {
        state.apply(line);
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v["type"].as_str().unwrap_or_default();
        let data = &v["data"];
        let ms = v["timestamp"].as_str().and_then(parse_rfc3339_ms).unwrap_or(0);
        let agent_id = v["agentId"].as_str().or(data["agentId"].as_str());
        let marker = matches!(kind, "subagent.started" | "subagent.completed" | "subagent.failed");
        match agent_key {
            Some(key) => {
                let mine = agent_id == Some(key) || (marker && data["toolCallId"].as_str() == Some(key));
                if !mine {
                    continue;
                }
                own_steps += usize::from(!marker);
            }
            // A subagent's own turns belong to its transcript.
            None if v["agentId"].is_string() && !marker => continue,
            None => {}
        }
        match kind {
            "user.message" => out.entries.extend(text_entry(EntryKind::User, data["content"].as_str().unwrap_or_default(), ms)),
            "assistant.message" => out.entries.extend(text_entry(EntryKind::Assistant, data["content"].as_str().unwrap_or_default(), ms)),
            "tool.execution_start" => {
                if let Some(id) = data["toolCallId"].as_str() {
                    calls.insert(id.to_owned(), out.entries.len());
                }
                let tool = data["toolName"].as_str().filter(|t| !t.is_empty()).unwrap_or("tool").to_owned();
                out.entries.push(Entry { kind: EntryKind::Tool, ms, tool, args: short_args(&data["arguments"]), running: true, ..Default::default() });
            }
            "tool.execution_complete" => {
                if let Some(index) = data["toolCallId"].as_str().and_then(|id| calls.remove(id)) {
                    let exit = data["shellExecution"]["exitCode"].as_i64();
                    let failed = data["success"].as_bool() == Some(false) || exit.is_some_and(|c| c != 0);
                    out.finish_tool(index, &short_result(&value_text(&data["result"]), exit), failed);
                }
            }
            "subagent.started" => {
                let name = data["agentDisplayName"].as_str().or(data["agentName"].as_str()).unwrap_or("subagent");
                let what = data["agentDescription"].as_str().filter(|d| !d.is_empty()).map(|d| format!(": {}", clip(d, 120, true))).unwrap_or_default();
                out.entries.push(note(format!("subagent {name} started{what}"), ms, false));
            }
            "subagent.completed" => out.entries.push(note("subagent finished", ms, false)),
            "subagent.failed" => out.entries.push(note("subagent failed", ms, true)),
            "session.task_complete" => out.entries.extend(text_entry(EntryKind::Assistant, data["summary"].as_str().unwrap_or_default(), ms)),
            "session.error" => out.entries.push(note(format!("error: {}", clip(data["message"].as_str().unwrap_or("the session failed"), 200, true)), ms, true)),
            "abort" => out.entries.push(note("stopped (Esc)", ms, false)),
            _ => {}
        }
    }
    out.running = match agent_key {
        Some(key) => state.agents.iter().any(|a| a.key == key && a.status == AgentStatus::Working),
        None => state.derived(now_ms()).0 == Activity::Working,
    };
    if agent_key.is_some() && own_steps == 0 {
        out.note = "This Copilot version records only the subagent's start and end, not its own steps.".into();
    }
    Ok(out)
}

/// OpenCode: the newest messages of `session` with their parts, by index
/// only (the database can be huge); large text and tool output are cut in
/// SQL. `parent`: the session it should be a child of.
pub fn opencode(db: &Path, session: &str, parent: Option<&str>) -> Result<Transcript> {
    use super::opencode_history::{number, text};
    if !super::opencode::is_session_id(session) {
        return Ok(Transcript { note: "No conversation yet: OpenCode creates it with the first prompt.".into(), ..Default::default() });
    }
    let connection = super::opencode_history::open(db)?;
    anyhow::ensure!(
        super::opencode_history::Schema::read(&connection).indexed,
        "OpenCode's database lacks the indexes the canvas reads by; the transcript is not read (it would scan the whole database)"
    );
    let found: Option<(Option<String>, Option<String>)> =
        connection.prepare_cached("SELECT title, parent_id FROM session WHERE id = ?1")?.query_row([session], |r| Ok((text(r, 0)?, text(r, 1)?))).optional()?;
    let (title, parent_id) = found.with_context(|| format!("OpenCode has no session {session}"))?;
    let about = match (&parent_id, parent) {
        (Some(actual), Some(expected)) if actual.as_str() != expected => format!("A subagent session of {actual} (not of {expected})."),
        (Some(_), _) => format!("Subagent session “{}” (OpenCode task tool).", title.unwrap_or_default()),
        (None, _) => String::new(),
    };
    let mut out = Transcript { note: about, ..Default::default() };
    let mut messages = connection.prepare_cached(
        "SELECT id, time_created, json_extract(d, '$.role'), json_extract(d, '$.time.completed'), json_extract(d, '$.finish'), \
         json_extract(d, '$.error.name'), substr(json_extract(d, '$.error.data.message'), 1, 300) \
         FROM (SELECT id, time_created, CASE WHEN json_valid(data) THEN data ELSE '{}' END AS d \
               FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2) \
         ORDER BY time_created ASC, id ASC",
    )?;
    struct Msg {
        id: String,
        created: u64,
        role: String,
        completed: Option<u64>,
        finish: String,
        error: String,
        message: String,
    }
    let rows: Vec<Msg> = messages
        .query_map(rusqlite::params![session, OPENCODE_MESSAGES as i64], |r| {
            Ok(Msg {
                id: r.get(0)?,
                created: number(r, 1)?.unwrap_or(0),
                role: text(r, 2)?.unwrap_or_default(),
                completed: number(r, 3)?,
                finish: text(r, 4)?.unwrap_or_default(),
                error: text(r, 5)?.unwrap_or_default(),
                message: text(r, 6)?.unwrap_or_default(),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    out.truncated = rows.len() >= OPENCODE_MESSAGES;
    let mut parts = connection.prepare_cached(&format!(
        "SELECT json_extract(d, '$.type'), substr(json_extract(d, '$.text'), 1, {MAX_TEXT}), json_extract(d, '$.tool'), json_extract(d, '$.state.status'), \
         substr(coalesce(json_extract(d, '$.state.input.command'), json_extract(d, '$.state.input.filePath'), json_extract(d, '$.state.input.path'), \
                         json_extract(d, '$.state.input.pattern'), json_extract(d, '$.state.input.url'), json_extract(d, '$.state.input.description'), \
                         json_extract(d, '$.state.input.query'), json_extract(d, '$.state.input.prompt'), json_extract(d, '$.state.input')), 1, {MAX_ARGS}), \
         substr(json_extract(d, '$.state.output'), 1, 600), json_extract(d, '$.state.metadata.exit'), substr(json_extract(d, '$.state.error'), 1, 300), \
         json_extract(d, '$.synthetic'), json_extract(d, '$.state.time.start') \
         FROM (SELECT id, CASE WHEN json_valid(data) THEN data ELSE '{{}}' END AS d FROM part WHERE message_id = ?1) ORDER BY id"
    ))?;
    let mut last_running = false;
    for message in &rows {
        let mut running_tools = false;
        let found = parts.query_map([&message.id], |r| {
            Ok((
                text(r, 0)?.unwrap_or_default(),
                text(r, 1)?.unwrap_or_default(),
                text(r, 2)?.unwrap_or_default(),
                text(r, 3)?.unwrap_or_default(),
                text(r, 4)?.unwrap_or_default(),
                text(r, 5)?.unwrap_or_default(),
                r.get_ref(6)?.as_i64().ok(),
                text(r, 7)?.unwrap_or_default(),
                r.get_ref(8)?.as_i64().ok() == Some(1),
                number(r, 9)?.unwrap_or(0),
            ))
        })?;
        for (kind, body, tool, status, args, output, exit, error, synthetic, started) in found.filter_map(|p| p.ok()) {
            let ms = if started > 0 { started } else { message.created };
            match kind.as_str() {
                "text" if synthetic => {}
                "text" => out.entries.extend(text_entry(if message.role == "user" { EntryKind::User } else { EntryKind::Assistant }, &body, ms)),
                "tool" => {
                    let running = matches!(status.as_str(), "pending" | "running");
                    running_tools |= running;
                    let (result, failed) = match status.as_str() {
                        "error" => (error, true),
                        _ => (short_result(&output, exit), exit.is_some_and(|c| c != 0)),
                    };
                    out.entries.push(Entry { kind: EntryKind::Tool, ms, tool, args: clip(&args, MAX_ARGS, true), result: clip(&result, canvas_model::transcript::MAX_RESULT, true), failed, running, ..Default::default() });
                }
                "subtask" => out.entries.push(note("handed a task to a subagent", ms, false)),
                _ => {}
            }
        }
        match (message.role.as_str(), message.error.as_str()) {
            ("assistant", "MessageAbortedError") => out.entries.push(note("stopped (Esc)", message.completed.unwrap_or(message.created), false)),
            ("assistant", error) if !error.is_empty() => {
                let detail = if message.message.is_empty() { String::new() } else { format!(": {}", clip(&message.message, 200, true)) };
                out.entries.push(note(format!("error: {error}{detail}"), message.completed.unwrap_or(message.created), true));
            }
            _ => {}
        }
        last_running = match message.role.as_str() {
            "assistant" if !message.error.is_empty() => false,
            "assistant" => message.completed.is_none() || message.finish == "tool-calls" || running_tools,
            _ => true,
        };
    }
    out.running = last_running;
    Ok(out)
}

/// Claude Code transcript lines as entries; `main`: the session's own
/// transcript, whose subagent (sidechain) records are left out.
fn claude_entries(lines: &[String], main: bool) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut calls: HashMap<String, usize> = HashMap::new();
    for line in lines {
        if line.starts_with("{\"type\":\"api-request-blob\"") || line.starts_with("{\"type\":\"file-history") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if (main && v["isSidechain"].as_bool() == Some(true)) || v["isMeta"].as_bool() == Some(true) {
            continue;
        }
        let ms = v["timestamp"].as_str().and_then(parse_rfc3339_ms).unwrap_or(0);
        let content = &v["message"]["content"];
        match v["type"].as_str().unwrap_or_default() {
            "user" => {
                let blocks: Vec<&Value> = match content {
                    Value::Array(blocks) => blocks.iter().collect(),
                    other => vec![other],
                };
                for block in blocks {
                    let text = block.as_str().or_else(|| (block["type"] == "text").then(|| block["text"].as_str()).flatten());
                    if let Some(text) = text {
                        let trimmed = text.trim_start();
                        if trimmed.starts_with("[Request interrupted by user") {
                            entries.push(note("stopped (Esc)", ms, false));
                        } else if trimmed.starts_with("<task-notification>") {
                            entries.push(note("a background subagent reported back", ms, false));
                        } else if !(trimmed.starts_with("<command-") || trimmed.starts_with("<local-command") || trimmed.starts_with("<bash-")) {
                            entries.extend(text_entry(EntryKind::User, text, ms));
                        }
                    } else if block["type"] == "tool_result"
                        && let Some(index) = block["tool_use_id"].as_str().and_then(|id| calls.remove(id))
                        && let Some(entry) = entries.get_mut(index)
                    {
                        let result = super::claude_history::result_text(block, &v["toolUseResult"]);
                        entry.result = clip(&short_result(&result, None), canvas_model::transcript::MAX_RESULT, true);
                        entry.failed = block["is_error"].as_bool() == Some(true);
                        entry.running = false;
                    }
                }
            }
            "assistant" if v["isApiErrorMessage"].as_bool() == Some(true) => {
                entries.push(note(format!("error: {}", clip(&value_text(content), 200, true)), ms, true));
            }
            "assistant" => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => entries.extend(text_entry(EntryKind::Assistant, block["text"].as_str().unwrap_or_default(), ms)),
                        Some("tool_use") => {
                            if let Some(id) = block["id"].as_str() {
                                calls.insert(id.to_owned(), entries.len());
                            }
                            let tool = block["name"].as_str().unwrap_or("tool").to_owned();
                            entries.push(Entry { kind: EntryKind::Tool, ms, tool, args: short_args(&block["input"]), running: true, ..Default::default() });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    entries
}

/// Claude Code: the session's transcript, or with `agent_key` (its `Agent`
/// tool call id) that subagent's own file `subagents/agent-<id>.jsonl`
/// beside it (older versions recorded only the task and its answer).
pub fn claude(dir: &Path, session: &str, agent_key: Option<&str>, cwd: Option<&str>) -> Result<Transcript> {
    anyhow::ensure!(crate::index::is_session_id(session), "not a Claude Code session id: {session}");
    let Some(path) = super::claude_history::find_transcript(dir, session, cwd) else {
        return Ok(Transcript { note: "No transcript yet: Claude Code writes it with the first prompt.".into(), ..Default::default() });
    };
    let (lines, truncated) = tail_lines(&path)?;
    let Some(key) = agent_key else {
        let mut state = super::claude_history::ClaudeState::default();
        for line in &lines {
            state.apply(line);
        }
        let running = state.derived(now_ms()).0 == Activity::Working;
        return Ok(Transcript { entries: claude_entries(&lines, true), truncated, running, note: String::new() });
    };
    // The task, its answer and the subagent's id from the parent's transcript.
    let (mut prompt, mut answer, mut agent_id, mut launched, mut failed) = (None, None, None, false, false);
    for line in &lines {
        if !line.contains(key) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_use" && block["id"] == key {
                let input = &block["input"];
                prompt = input["prompt"].as_str().or(input["description"].as_str()).map(str::to_owned);
            } else if block["type"] == "tool_result" && block["tool_use_id"] == key {
                let result = &v["toolUseResult"];
                agent_id = result["agentId"].as_str().filter(|id| super::claude_history::is_agent_id(id)).map(str::to_owned);
                launched = result["status"].as_str() == Some("async_launched");
                failed = block["is_error"].as_bool() == Some(true);
                if !launched {
                    answer = Some(super::claude_history::result_text(block, result));
                }
            }
        }
    }
    let folder = path.with_extension("").join("subagents");
    let finished = agent_id.as_ref().is_some_and(|id| {
        std::fs::read_to_string(folder.join(format!("agent-{id}.meta.json"))).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|m| m["finished"].as_bool() == Some(true))
    });
    let running = !failed && !finished && (launched || answer.is_none());
    if let Some(file) = agent_id.as_ref().map(|id| folder.join(format!("agent-{id}.jsonl"))).filter(|f| f.is_file()) {
        let (lines, truncated) = tail_lines(&file)?;
        return Ok(Transcript { entries: claude_entries(&lines, false), truncated, running, note: String::new() });
    }
    let mut entries = Vec::new();
    entries.extend(prompt.as_deref().and_then(|p| text_entry(EntryKind::User, p, 0)));
    entries.extend(answer.as_deref().and_then(|a| text_entry(EntryKind::Assistant, a, 0)));
    Ok(Transcript { entries, truncated: false, running, note: "Only the task and its answer: this subagent's own steps are not recorded beside the transcript.".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-transcript-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const ID: &str = "12345678-1234-1234-1234-123456789abc";

    #[test]
    fn copilot_session_and_subagent_transcripts() {
        let root = temp("copilot");
        let dir = root.join(ID);
        std::fs::create_dir_all(&dir).unwrap();
        let events = [
            r#"{"type":"session.start","timestamp":"2026-10-20T20:00:00Z","data":{}}"#,
            r#"{"type":"user.message","timestamp":"2026-10-20T20:00:01Z","data":{"content":"Fix the build"}}"#,
            r#"{"type":"assistant.turn_start","timestamp":"2026-10-20T20:00:02Z","data":{}}"#,
            r#"{"type":"assistant.message","timestamp":"2026-10-20T20:00:03Z","data":{"content":"Running the tests."}}"#,
            r#"{"type":"tool.execution_start","timestamp":"2026-10-20T20:00:04Z","data":{"toolCallId":"t1","toolName":"powershell","arguments":{"command":"cargo test","description":"tests"}}}"#,
            r#"{"type":"tool.execution_complete","timestamp":"2026-10-20T20:00:09Z","data":{"toolCallId":"t1","success":false,"shellExecution":{"exitCode":101},"result":{"content":"error[E0599]: no method\n\nmore"}}}"#,
            r#"{"type":"subagent.started","agentId":"r1","timestamp":"2026-10-20T20:00:10Z","data":{"toolCallId":"t2","agentDisplayName":"Reviewer","agentDescription":"Review the diff"}}"#,
            r#"{"type":"user.message","agentId":"r1","timestamp":"2026-10-20T20:00:11Z","data":{"content":"Review the diff of src/"}}"#,
            r#"{"type":"tool.execution_start","agentId":"r1","timestamp":"2026-10-20T20:00:12Z","data":{"toolCallId":"r1t","toolName":"view","arguments":{"path":"src/main.rs"}}}"#,
            r#"{"type":"tool.execution_complete","agentId":"r1","timestamp":"2026-10-20T20:00:13Z","data":{"toolCallId":"r1t","success":true,"result":"fn main() {}"}}"#,
            r#"{"type":"assistant.message","agentId":"r1","timestamp":"2026-10-20T20:00:14Z","data":{"content":"Looks good."}}"#,
            r#"{"type":"subagent.completed","agentId":"r1","timestamp":"2026-10-20T20:00:15Z","data":{"toolCallId":"t2"}}"#,
            r#"{"type":"tool.execution_start","timestamp":"2026-10-20T20:00:16Z","data":{"toolCallId":"t3","toolName":"edit","arguments":{"path":"src/lib.rs"}}}"#,
        ];
        std::fs::write(dir.join("events.jsonl"), events.join("\n") + "\n").unwrap();
        let session = copilot(&root, ID, None).unwrap();
        let kinds: Vec<(EntryKind, &str)> = session.entries.iter().map(|e| (e.kind, if e.kind == EntryKind::Tool { e.tool.as_str() } else { e.text.as_str() })).collect();
        assert_eq!(
            kinds,
            [
                (EntryKind::User, "Fix the build"),
                (EntryKind::Assistant, "Running the tests."),
                (EntryKind::Tool, "powershell"),
                (EntryKind::Note, "subagent Reviewer started: Review the diff"),
                (EntryKind::Note, "subagent finished"),
                (EntryKind::Tool, "edit"),
            ],
            "the subagent's own turns stay out of the parent's transcript"
        );
        let test = &session.entries[2];
        assert_eq!((test.args.as_str(), test.result.as_str(), test.failed, test.running), ("cargo test", "exit 101 · error[E0599]: no method ↵ more", true, false));
        assert!(session.entries[5].running && session.running, "an open tool call: it runs");
        let reviewer = copilot(&root, ID, Some("r1")).unwrap();
        let texts: Vec<&str> = reviewer.entries.iter().map(|e| if e.kind == EntryKind::Tool { e.args.as_str() } else { e.text.as_str() }).collect();
        assert_eq!(texts, ["subagent Reviewer started: Review the diff", "Review the diff of src/", "src/main.rs", "Looks good.", "subagent finished"]);
        assert!(!reviewer.running && reviewer.note.is_empty());
        // A subagent known only by its tool call (no agentId on its steps).
        let old = copilot(&root, ID, Some("t2")).unwrap();
        assert!(old.entries.iter().all(|e| e.kind == EntryKind::Note));
        assert!(copilot(&root, "not-a-session", None).is_err());
        assert!(copilot(&root, "87654321-4321-4321-4321-cba987654321", None).unwrap().entries.is_empty(), "no history yet");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_histories_are_read_from_their_end() {
        let root = temp("tail");
        let dir = root.join(ID);
        std::fs::create_dir_all(&dir).unwrap();
        let noise = format!("{{\"type\":\"assistant.message\",\"data\":{{\"content\":\"{}\"}}}}\n", "x".repeat(4000));
        let mut text = noise.repeat(700);
        text.push_str(r#"{"type":"user.message","data":{"content":"the newest prompt"}}"#);
        text.push('\n');
        std::fs::write(dir.join("events.jsonl"), text).unwrap();
        let mut transcript = read(&root, AgentKind::Copilot, ID, None, None, None, 5).unwrap();
        assert!(transcript.truncated && transcript.entries.len() == 5);
        assert_eq!(transcript.entries.pop().unwrap().text, "the newest prompt");
        std::fs::remove_dir_all(root).unwrap();
    }

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

    #[test]
    fn opencode_sessions_and_child_sessions() {
        let dir = temp("opencode");
        let path = dir.join("opencode.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        let session = |id: &str, parent: Option<&str>, title: &str| {
            db.execute("INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) VALUES (?1, 'p', ?2, 's', 'C:/r', ?3, '1', 1, 1)", rusqlite::params![id, parent, title]).unwrap();
        };
        let message = |id: &str, session: &str, at: i64, data: Value| {
            db.execute("INSERT OR REPLACE INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?3, ?4)", rusqlite::params![id, session, at, data.to_string()]).unwrap();
        };
        let part = |id: &str, message: &str, session: &str, data: Value| {
            db.execute("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 0, 0, ?4)", rusqlite::params![id, message, session, data.to_string()]).unwrap();
        };
        session("ses_parent", None, "Ship it");
        session("ses_child", Some("ses_parent"), "Explore code (@explore subagent)");
        message("msg_1", "ses_parent", 10, serde_json::json!({ "role": "user", "time": { "created": 10 } }));
        part("prt_1", "msg_1", "ses_parent", serde_json::json!({ "type": "text", "text": "Push the branch" }));
        part("prt_0", "msg_1", "ses_parent", serde_json::json!({ "type": "text", "text": "(system reminder)", "synthetic": true }));
        message("msg_2", "ses_parent", 20, serde_json::json!({ "role": "assistant", "time": { "created": 20, "completed": 30 }, "finish": "stop" }));
        part("prt_2", "msg_2", "ses_parent", serde_json::json!({ "type": "tool", "tool": "bash", "state": { "status": "completed", "input": { "command": "git push", "description": "push" }, "output": "To origin\n a..b main -> main", "metadata": { "exit": 0 }, "time": { "start": 21, "end": 22 } } }));
        part("prt_3", "msg_2", "ses_parent", serde_json::json!({ "type": "text", "text": "Pushed." }));
        // The child is still working: a running tool call.
        message("msg_c1", "ses_child", 40, serde_json::json!({ "role": "user", "time": { "created": 40 } }));
        part("prt_c1", "msg_c1", "ses_child", serde_json::json!({ "type": "text", "text": "Find the callers of foo" }));
        message("msg_c2", "ses_child", 41, serde_json::json!({ "role": "assistant", "time": { "created": 41 } }));
        part("prt_c2", "msg_c2", "ses_child", serde_json::json!({ "type": "tool", "tool": "grep", "state": { "status": "running", "input": { "pattern": "foo\\(" }, "time": { "start": 42 } } }));
        let parent = opencode(&path, "ses_parent", None).unwrap();
        let texts: Vec<String> = parent.entries.iter().map(|e| format!("{:?} {}{}", e.kind, e.text, e.args)).collect();
        assert_eq!(texts, ["User Push the branch", "Tool git push", "Assistant Pushed."], "synthetic parts are not yours");
        assert_eq!((parent.entries[1].result.as_str(), parent.running), ("To origin ↵ a..b main -> main", false));
        let child = opencode(&path, "ses_child", Some("ses_parent")).unwrap();
        assert!(child.running && child.entries[1].running && child.entries[1].args == "foo\\(", "{child:?}");
        assert!(child.note.contains("Explore code"));
        // It finishes: the next read sees it.
        db.execute("UPDATE part SET data = ?1 WHERE id = 'prt_c2'", [serde_json::json!({ "type": "tool", "tool": "grep", "state": { "status": "completed", "input": { "pattern": "foo" }, "output": "src/a.rs:3", "time": { "start": 42, "end": 43 } } }).to_string()]).unwrap();
        message("msg_c2", "ses_child", 41, serde_json::json!({ "role": "assistant", "time": { "created": 41, "completed": 44 }, "finish": "stop" }));
        let child = opencode(&path, "ses_child", Some("ses_parent")).unwrap();
        assert!(!child.running && child.entries[1].result == "src/a.rs:3");
        assert!(opencode(&path, "ses_nope", None).is_err());
        assert!(opencode(&path, "uuid-not-ses", None).unwrap().note.contains("first prompt"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opencode_without_indexes_is_not_scanned() {
        let dir = temp("opencode-noidx");
        let path = dir.join("opencode.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(&SCHEMA.replace("CREATE INDEX message_session_time_created_id_idx ON message (session_id, time_created, id);", "")).unwrap();
        drop(db);
        assert!(opencode(&path, "ses_x", None).unwrap_err().to_string().contains("indexes"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_transcripts_and_subagent_files() {
        let dir = temp("claude");
        let project = dir.join("projects").join("C--r");
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-27T10:00:00Z","message":{"role":"user","content":"Find the bug"}}"#,
            r#"{"type":"user","isMeta":true,"timestamp":"2026-10-27T10:00:00Z","message":{"role":"user","content":"<meta>"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-27T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-27T10:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b1","is_error":false,"content":"ok"}]},"toolUseResult":{"stdout":"test result: ok","stderr":""}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-27T10:00:06Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_a","name":"Agent","input":{"description":"Search","prompt":"Search for callers","subagent_type":"Explore"}}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-27T10:00:07Z","message":{"role":"assistant","content":[{"type":"text","text":"sidechain step"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-27T10:00:08Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"launched"}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1"}}"#,
        ];
        let transcript = project.join(format!("{ID}.jsonl"));
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
        let main = claude(&dir, ID, None, Some(r"C:\r")).unwrap();
        let texts: Vec<String> = main.entries.iter().map(|e| format!("{:?} {}{}", e.kind, e.text, e.args)).collect();
        assert_eq!(texts, ["User Find the bug", "Assistant Looking.", "Tool cargo test", "Tool Search"], "meta and sidechain records stay out");
        assert_eq!(main.entries[2].result, "test result: ok");
        // The background subagent's own file, while it runs and once it finished.
        let subagents = project.join(ID).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-a1.jsonl"),
            [
                r#"{"type":"user","isSidechain":true,"timestamp":"2026-10-27T10:00:07Z","message":{"role":"user","content":"Search for callers"}}"#,
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-27T10:00:09Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"g1","name":"Grep","input":{"pattern":"foo("}}]}}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let agent = claude(&dir, ID, Some("toolu_a"), None).unwrap();
        assert_eq!(agent.entries.iter().map(|e| format!("{:?} {}{}", e.kind, e.text, e.args)).collect::<Vec<_>>(), ["User Search for callers", "Tool foo("]);
        assert!(agent.running && agent.entries[1].running);
        std::fs::write(subagents.join("agent-a1.meta.json"), r#"{"finished":true}"#).unwrap();
        assert!(!claude(&dir, ID, Some("toolu_a"), None).unwrap().running);
        // Without its own file: the task and its answer.
        std::fs::remove_file(subagents.join("agent-a1.jsonl")).unwrap();
        let only = claude(&dir, ID, Some("toolu_a"), None).unwrap();
        assert!(only.note.contains("Only the task") && only.entries[0].text == "Search for callers");
        assert!(claude(&dir, "87654321-4321-4321-4321-cba987654321", None, None).unwrap().note.contains("No transcript"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
