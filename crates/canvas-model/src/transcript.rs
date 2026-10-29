//! A read-only transcript of an agent conversation (`Request::Transcript`):
//! your prompts, the agent's answers and its tool calls in order, as the
//! inspector shows them. The backend reads it from the agent's own history
//! (bounded: the newest part of large histories); nothing is started or
//! written.

use serde::{Deserialize, Serialize};

/// Entries one answer holds at most.
pub const MAX_ENTRIES: usize = 400;
/// Characters of one prompt or answer kept.
pub const MAX_TEXT: usize = 4_000;
/// Characters of a tool call's arguments kept.
pub const MAX_ARGS: usize = 160;
/// Characters of a tool call's result kept.
pub const MAX_RESULT: usize = 240;

/// What an entry is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EntryKind {
    /// A prompt (yours, or a subagent's task).
    User,
    /// The agent's text.
    Assistant,
    /// A tool call: name, short arguments, short result.
    Tool,
    /// Something that happened (a subagent started, an error, an abort).
    #[default]
    Note,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Entry {
    pub kind: EntryKind,
    /// When it happened (0: unknown).
    #[serde(default)]
    pub ms: u64,
    /// The prompt, answer or note.
    #[serde(default)]
    pub text: String,
    /// Tool name.
    #[serde(default)]
    pub tool: String,
    /// The tool call's arguments in a few words.
    #[serde(default)]
    pub args: String,
    /// The start of its result (or its exit code).
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub failed: bool,
    /// A tool call without its result yet.
    #[serde(default)]
    pub running: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Transcript {
    /// Oldest first.
    pub entries: Vec<Entry>,
    /// Older history exists that was not read.
    #[serde(default)]
    pub truncated: bool,
    /// The last turn has not finished (it looks like it runs).
    #[serde(default)]
    pub running: bool,
    /// Where it was read from, or what to know about it.
    #[serde(default)]
    pub note: String,
}

impl Transcript {
    /// Keeps the newest `limit` entries (at most [`MAX_ENTRIES`]).
    pub fn keep_newest(&mut self, limit: usize) {
        let limit = limit.clamp(1, MAX_ENTRIES);
        if self.entries.len() > limit {
            self.entries.drain(..self.entries.len() - limit);
            self.truncated = true;
        }
    }

    /// Completes the tool call entry at `index` with its result.
    pub fn finish_tool(&mut self, index: usize, result: &str, failed: bool) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.result = clip(result, MAX_RESULT, true);
            entry.failed = failed;
            entry.running = false;
        }
    }

    /// The transcript as plain text (Copy).
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            let line = match entry.kind {
                EntryKind::User => format!("> {}", entry.text),
                EntryKind::Assistant => entry.text.clone(),
                EntryKind::Tool => {
                    let result = if entry.running { " (running)".to_owned() } else if entry.result.is_empty() { String::new() } else { format!(" -> {}", entry.result) };
                    format!("[{}] {}{result}", entry.tool, entry.args)
                }
                EntryKind::Note => format!("- {}", entry.text),
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

/// `text` cut to `max` characters (an ellipsis marks the cut); `flat` puts
/// it on one line (whitespace runs become one space).
pub fn clip(text: &str, max: usize, flat: bool) -> String {
    let text = if flat { text.split_whitespace().collect::<Vec<_>>().join(" ") } else { text.trim().replace("\r\n", "\n") };
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A tool call's arguments in a few words: its command, path, pattern, URL,
/// description or prompt, else its arguments as compact JSON.
pub fn short_args(args: &serde_json::Value) -> String {
    const KEYS: [&str; 12] = ["command", "cmd", "filePath", "file_path", "path", "pattern", "url", "query", "description", "prompt", "subagent_type", "intent"];
    if let Some(text) = args.as_str() {
        return clip(text, MAX_ARGS, true);
    }
    for key in KEYS {
        if let Some(value) = args.get(key).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty()) {
            return clip(value, MAX_ARGS, true);
        }
    }
    match args {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(map) if map.is_empty() => String::new(),
        other => clip(&other.to_string(), MAX_ARGS, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_short_arguments_and_the_newest_entries() {
        assert_eq!(clip("a  b\n c", 10, true), "a b c");
        assert_eq!(clip("abcdef", 4, false), "abc…");
        assert_eq!(short_args(&serde_json::json!({ "command": "git push", "timeout": 5 })), "git push");
        assert_eq!(short_args(&serde_json::json!({ "filePath": "C:\\x\\a.rs" })), "C:\\x\\a.rs");
        assert_eq!(short_args(&serde_json::json!({ "n": 1 })), "{\"n\":1}");
        assert_eq!(short_args(&serde_json::json!({})), "");
        let mut t = Transcript { entries: (0..10).map(|i| Entry { text: i.to_string(), ..Default::default() }).collect(), ..Default::default() };
        t.keep_newest(3);
        assert!(t.truncated);
        assert_eq!(t.entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["7", "8", "9"]);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Transcript>(&json).unwrap(), t);
        assert!(t.plain_text().starts_with("- 7\n"));
    }
}
