//! A stand-in MCP server (stdio) for the gateway's tests and the VM probe
//! (`atelier mcp-standin`): a pretend forge with the tools
//! `list_pull_requests`, `create_pull_request {draft, head, base, title}`,
//! `update_pull_request`, `vote` and `add_comment`. It never calls anything.
//!
//! - Every message it receives is logged, one JSON line each, to `log`, so
//!   tests can tell what reached it (a refused call never does).
//! - It looks for a credential in its own environment
//!   ([`TOKEN_ENV`]) and says whether it has one in its answers
//!   (`credential: present`), never the value, unless [`LEAK_ENV`] is set:
//!   then it puts the value in its answers and a log notification, for the
//!   tests that check the gateway keeps it from the guest.
//! - On demand (arguments of `list_pull_requests`): `progress: true` sends
//!   two progress notifications first (when the request has a
//!   `progressToken`); `ask: "roots/list"` (or `sampling/createMessage`,
//!   `elicitation/create`) sends that request to the client and answers the
//!   call once it is answered; `hold: true` never answers: the call waits
//!   for a cancellation (which it logs).
//! - `tools/list` comes in pages of three (`nextCursor`).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::jsonrpc::{self, Kind};

/// The stand-in's credential (as a real server's token would be given in its `env`).
pub const TOKEN_ENV: &str = "ATELIER_MCP_STANDIN_TOKEN";
/// Set: the stand-in puts its credential in its answers (tests of the gateway's redaction).
pub const LEAK_ENV: &str = "ATELIER_MCP_STANDIN_LEAK";

const PAGE: usize = 3;

fn tools() -> Vec<Value> {
    let number = json!({"type": "integer", "description": "The pull request's number"});
    vec![
        json!({"name": "list_pull_requests", "description": "Lists pull requests", "inputSchema": {"type": "object", "properties": {
            "state": {"type": "string", "enum": ["open", "closed", "all"]},
            "progress": {"type": "boolean", "description": "Send progress notifications first"},
            "ask": {"type": "string", "enum": ["roots/list", "sampling/createMessage", "elicitation/create"], "description": "Ask the client this first"},
            "hold": {"type": "boolean", "description": "Answer only after a cancellation (never)"}
        }}}),
        json!({"name": "create_pull_request", "description": "Opens a pull request", "inputSchema": {"type": "object", "properties": {
            "draft": {"type": "boolean"}, "head": {"type": "string"}, "base": {"type": "string"}, "title": {"type": "string"}
        }, "required": ["head", "base", "title"]}}),
        json!({"name": "update_pull_request", "description": "Changes a pull request", "inputSchema": {"type": "object", "properties": {
            "number": number, "title": {"type": "string"}, "state": {"type": "string"}
        }, "required": ["number"]}}),
        json!({"name": "vote", "description": "Votes on a pull request", "inputSchema": {"type": "object", "properties": {
            "number": number, "vote": {"type": "integer"}
        }, "required": ["number", "vote"]}}),
        json!({"name": "add_comment", "description": "Comments on a pull request", "inputSchema": {"type": "object", "properties": {
            "number": number, "body": {"type": "string"}
        }, "required": ["number", "body"]}}),
    ]
}

struct State {
    credential: Option<String>,
    leak: bool,
    /// Calls held until cancelled: their ids.
    held: Vec<String>,
    /// Requests it sent the client: their id → the call waiting for the answer and what was asked.
    asked: HashMap<String, (Value, String)>,
    next: u64,
}

/// Serves MCP on `input`/`output` (one JSON message per line) until `input` ends.
pub fn run(input: impl BufRead, mut output: impl Write, log: &Path) -> Result<()> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut logged = std::fs::OpenOptions::new().create(true).append(true).open(log).with_context(|| format!("cannot open {}", log.display()))?;
    let mut state = State {
        credential: canvas_common::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty()),
        leak: canvas_common::env::var(LEAK_ENV).is_ok_and(|v| !v.is_empty() && v != "0"),
        held: Vec::new(),
        asked: HashMap::new(),
        next: 0,
    };
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answers = match serde_json::from_str::<Value>(&line) {
            Ok(message) => {
                writeln!(logged, "{message}")?;
                logged.flush()?;
                state.take(message)
            }
            Err(_) => {
                writeln!(logged, "{}", json!({ "unparsed": line.len() }))?;
                vec![jsonrpc::error(Value::Null, jsonrpc::PARSE_ERROR, "not JSON", None)]
            }
        };
        for answer in answers {
            writeln!(output, "{answer}")?;
        }
        output.flush()?;
    }
    Ok(())
}

impl State {
    fn credential_text(&self) -> String {
        match (&self.credential, self.leak) {
            (Some(token), true) => format!("credential: {token}"),
            (Some(_), false) => "credential: present".into(),
            (None, _) => "credential: missing".into(),
        }
    }

    fn text(&self, id: Value, text: String) -> Value {
        jsonrpc::result(id, json!({ "content": [{ "type": "text", "text": format!("{text} ({})", self.credential_text()) }], "isError": false }))
    }

    fn take(&mut self, message: Value) -> Vec<Value> {
        match jsonrpc::classify(&message) {
            Ok(Kind::Request { id, method }) => self.request(id, &method, message.get("params").cloned().unwrap_or(Value::Null)),
            Ok(Kind::Notification { method }) => {
                if method == "notifications/cancelled" {
                    let id = jsonrpc::key(&message["params"]["requestId"]);
                    self.held.retain(|h| *h != id);
                }
                Vec::new()
            }
            Ok(Kind::Response { id }) => match self.asked.remove(&jsonrpc::key(&id)) {
                Some((call, what)) => {
                    let text = match message.get("error") {
                        Some(error) => format!("{what} refused: {}", error["message"].as_str().unwrap_or("?")),
                        None => format!("{what} answered: {}", message["result"]),
                    };
                    vec![self.text(call, text)]
                }
                None => Vec::new(),
            },
            Err(why) => vec![jsonrpc::error(message.get("id").cloned().unwrap_or(Value::Null), jsonrpc::INVALID_REQUEST, why, None)],
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> Vec<Value> {
        match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                let version = if jsonrpc::known(asked) { asked } else { jsonrpc::LATEST };
                let mut out = vec![jsonrpc::result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": true }, "resources": { "subscribe": true }, "prompts": {}, "logging": {}, "completions": {} },
                        "serverInfo": { "name": "atelier-mcp-standin", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": "A stand-in forge for tests."
                    }),
                )];
                if self.leak
                    && let Some(token) = &self.credential
                {
                    out.push(jsonrpc::notification("notifications/message", json!({ "level": "info", "data": format!("using token {token}") })));
                }
                out
            }
            "ping" => vec![jsonrpc::result(id, json!({}))],
            "tools/list" => {
                let all = tools();
                let start = params["cursor"].as_str().and_then(|c| c.strip_prefix("page-")).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(all.len());
                let end = (start + PAGE).min(all.len());
                let mut result = json!({ "tools": all[start..end] });
                if end < all.len() {
                    result["nextCursor"] = json!(format!("page-{end}"));
                }
                vec![jsonrpc::result(id, result)]
            }
            "tools/call" => self.call(id, &params),
            other => vec![jsonrpc::error(id, jsonrpc::METHOD_NOT_FOUND, &format!("the stand-in has no method {other}"), None)],
        }
    }

    fn call(&mut self, id: Value, params: &Value) -> Vec<Value> {
        let arguments = &params["arguments"];
        let text = |key: &str| arguments[key].as_str().unwrap_or_default().to_owned();
        match params["name"].as_str().unwrap_or_default() {
            "list_pull_requests" => {
                let mut out = Vec::new();
                if arguments["progress"] == true
                    && let Some(token) = params["_meta"].get("progressToken")
                {
                    for step in 1..=2 {
                        out.push(jsonrpc::notification("notifications/progress", json!({ "progressToken": token, "progress": step, "total": 2, "message": format!("page {step}") })));
                    }
                }
                if arguments["hold"] == true {
                    self.held.push(jsonrpc::key(&id));
                    return out;
                }
                if let Some(what) = arguments["ask"].as_str() {
                    self.next += 1;
                    let asked = json!(format!("standin-{}", self.next));
                    let params = match what {
                        "sampling/createMessage" => json!({ "messages": [{ "role": "user", "content": { "type": "text", "text": "Summarize the pull requests" } }], "maxTokens": 100 }),
                        "elicitation/create" => json!({ "message": "Which pull request?", "requestedSchema": { "type": "object", "properties": { "number": { "type": "integer" } } } }),
                        _ => json!({}),
                    };
                    self.asked.insert(jsonrpc::key(&asked), (id, what.to_owned()));
                    out.push(jsonrpc::request(asked, what, params));
                    return out;
                }
                let state = Some(text("state")).filter(|s| !s.is_empty()).unwrap_or_else(|| "open".into());
                out.push(self.text(id, format!("{state} pull requests: #1 Fix the login page, #2 Update the docs")));
                out
            }
            "create_pull_request" => {
                let kind = if arguments["draft"] == true { "draft pull request" } else { "pull request" };
                vec![self.text(id, format!("created {kind} #3 {:?} from {} into {}", text("title"), text("head"), text("base")))]
            }
            "update_pull_request" => vec![self.text(id, format!("updated pull request #{}", arguments["number"]))],
            "vote" => vec![self.text(id, format!("voted {} on pull request #{}", arguments["vote"], arguments["number"]))],
            "add_comment" => vec![self.text(id, format!("commented on pull request #{}", arguments["number"]))],
            other => vec![jsonrpc::error(id, jsonrpc::INVALID_PARAMS, &format!("unknown tool {other}"), None)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(lines: &[Value]) -> (Vec<Value>, Vec<Value>) {
        let dir = std::env::temp_dir().join(format!("atelier-mcp-standin-{}", uuid::Uuid::new_v4()));
        let log = dir.join("log.jsonl");
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect::<String>() + "not json\n\n";
        let mut output = Vec::new();
        run(std::io::Cursor::new(input), &mut output, &log).unwrap();
        let answers = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let logged = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let _ = std::fs::remove_dir_all(dir);
        (answers, logged)
    }

    #[test]
    fn it_answers_logs_everything_and_pages_its_tools() {
        let (answers, logged) = exchange(&[
            jsonrpc::request(json!(1), "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } })),
            jsonrpc::notification("notifications/initialized", json!({})),
            jsonrpc::request(json!("two"), "tools/list", json!({})),
            jsonrpc::request(json!(3), "tools/list", json!({ "cursor": "page-3" })),
            jsonrpc::request(json!(4), "tools/call", json!({ "name": "create_pull_request", "arguments": { "draft": true, "head": "h", "base": "main", "title": "T" } })),
            jsonrpc::request(json!(5), "resources/list", json!({})),
        ]);
        assert_eq!(answers[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(answers[1]["id"], "two");
        assert_eq!(answers[1]["result"]["tools"].as_array().unwrap().len(), 3);
        assert_eq!(answers[1]["result"]["nextCursor"], "page-3");
        let rest: Vec<&str> = answers[2]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(rest, ["vote", "add_comment"]);
        assert!(answers[2]["result"].get("nextCursor").is_none());
        let created = answers[3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(created.contains("draft pull request") && created.contains("from h into main"), "{created}");
        assert_eq!(answers[4]["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        assert_eq!(answers[5]["error"]["code"], jsonrpc::PARSE_ERROR, "a line that is not JSON");
        assert_eq!(logged.len(), 7, "every message it got, the notification and the bad line too");
        assert_eq!(logged[1]["method"], "notifications/initialized");
    }

    #[test]
    fn progress_requests_to_the_client_and_held_calls_come_on_demand() {
        let (answers, _) = exchange(&[
            jsonrpc::request(json!(1), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "progress": true }, "_meta": { "progressToken": "p1" } })),
            jsonrpc::request(json!(2), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "ask": "roots/list" } })),
            jsonrpc::result(json!("standin-1"), json!({ "roots": [] })),
            jsonrpc::request(json!(3), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "ask": "sampling/createMessage" } })),
            jsonrpc::error(json!("standin-2"), jsonrpc::REFUSED, "refused by the policy", None),
            jsonrpc::request(json!(4), "tools/call", json!({ "name": "list_pull_requests", "arguments": { "hold": true } })),
            jsonrpc::notification("notifications/cancelled", json!({ "requestId": 4 })),
            jsonrpc::request(json!(5), "ping", json!({})),
        ]);
        assert_eq!(answers[0]["method"], "notifications/progress");
        assert_eq!(answers[0]["params"]["progressToken"], "p1");
        assert_eq!(answers[1]["params"]["progress"], 2);
        assert_eq!(answers[2]["id"], 1);
        assert!(answers[2]["result"]["content"][0]["text"].as_str().unwrap().contains("credential: "));
        assert_eq!((answers[3]["method"].as_str(), answers[3]["id"].as_str()), (Some("roots/list"), Some("standin-1")));
        assert!(answers[4]["result"]["content"][0]["text"].as_str().unwrap().starts_with("roots/list answered"));
        assert_eq!(answers[4]["id"], 2);
        assert_eq!(answers[5]["method"], "sampling/createMessage");
        assert!(answers[6]["result"]["content"][0]["text"].as_str().unwrap().contains("refused: refused by the policy"));
        assert_eq!(answers[7]["id"], 5, "the held call was never answered");
        assert_eq!(answers.len(), 9, "and the bad line's error");
    }
}
