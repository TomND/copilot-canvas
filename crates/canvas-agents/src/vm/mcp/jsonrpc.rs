//! JSON-RPC 2.0 as MCP uses it: what a message is, the protocol versions the
//! gateway speaks (pinned: a version it does not know is refused, never
//! guessed), batches (MCP 2025-03-26 allows them, later versions removed
//! them; they never cross the link: the guest's proxy splits or refuses
//! them), errors, and ids (kept exactly as the sender wrote them).

use serde_json::{Map, Value, json};

/// The MCP protocol versions the gateway and the guest's proxy speak, oldest first.
pub const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The newest of [`VERSIONS`].
pub const LATEST: &str = "2025-11-25";
/// What a Streamable HTTP request without `MCP-Protocol-Version` is taken to speak.
pub const HTTP_DEFAULT: &str = "2025-03-26";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// The session ended (the server, the link or the gateway closed it).
pub const SESSION_ENDED: i64 = -32000;
/// Refused by the VM's MCP policy (`mcp-policy.yaml`).
pub const REFUSED: i64 = -32003;

/// Whether the gateway speaks `version`.
pub fn known(version: &str) -> bool {
    VERSIONS.contains(&version)
}

/// Whether `version` lets a client send a JSON-RPC batch (only 2025-03-26 did).
pub fn batches(version: &str) -> bool {
    version == "2025-03-26"
}

/// What one JSON-RPC message is.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Request { id: Value, method: String },
    Notification { method: String },
    Response { id: Value },
}

/// Classifies one message (not a batch); `Err` says what is wrong with it.
pub fn classify(message: &Value) -> Result<Kind, &'static str> {
    let Some(object) = message.as_object() else { return Err("a JSON-RPC message is an object") };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("jsonrpc must be \"2.0\"");
    }
    let id = object.get("id");
    if let Some(id) = id
        && !id_ok(id)
    {
        return Err("an id is a string or an integer");
    }
    match object.get("method") {
        Some(Value::String(method)) if !method.is_empty() => {
            if object.contains_key("result") || object.contains_key("error") {
                return Err("a request has no result or error");
            }
            if object.get("params").is_some_and(|p| !p.is_object() && !p.is_array()) {
                return Err("params is an object");
            }
            Ok(match id {
                Some(id) => Kind::Request { id: id.clone(), method: method.clone() },
                None => Kind::Notification { method: method.clone() },
            })
        }
        Some(_) => Err("method is a string"),
        None => match (id, object.contains_key("result"), object.get("error")) {
            (Some(id), true, None) => Ok(Kind::Response { id: id.clone() }),
            (Some(id), false, Some(error)) if error.is_object() => Ok(Kind::Response { id: id.clone() }),
            (None, _, _) => Err("a response has an id"),
            _ => Err("neither a request, a notification nor a response"),
        },
    }
}

/// MCP ids: strings or integers (never null, never fractions).
fn id_ok(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

/// An id as a map key (`1` and `"1"` stay apart).
pub fn key(id: &Value) -> String {
    id.to_string()
}

/// A batch, or one message, as the messages to relay one by one, under
/// `version` (`None`: the session is not initialized, where a batch is never
/// allowed: `initialize` may not be part of one).
pub fn split(body: Value, version: Option<&str>) -> Result<Vec<Value>, Value> {
    match body {
        Value::Array(items) => {
            match version {
                Some(version) if batches(version) => {}
                Some(version) => return Err(error(Value::Null, INVALID_REQUEST, &format!("JSON-RPC batches are not part of MCP {version}: send one message per request"), None)),
                None => return Err(error(Value::Null, INVALID_REQUEST, "a JSON-RPC batch cannot open a session: initialize alone first", None)),
            }
            if items.is_empty() {
                return Err(error(Value::Null, INVALID_REQUEST, "an empty batch", None));
            }
            if items.len() > 100 {
                return Err(error(Value::Null, INVALID_REQUEST, "a batch of more than 100 messages", None));
            }
            for item in &items {
                if let Err(why) = classify(item) {
                    return Err(error(Value::Null, INVALID_REQUEST, why, None));
                }
                if matches!(classify(item), Ok(Kind::Request { ref method, .. }) if method == "initialize") {
                    return Err(error(Value::Null, INVALID_REQUEST, "initialize may not be part of a batch", None));
                }
            }
            Ok(items)
        }
        single => match classify(&single) {
            Ok(_) => Ok(vec![single]),
            Err(why) => {
                let id = single.get("id").filter(|id| id_ok(id)).cloned().unwrap_or(Value::Null);
                Err(error(id, INVALID_REQUEST, why, None))
            }
        },
    }
}

/// An error response.
pub fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = Map::new();
    error.insert("code".into(), code.into());
    error.insert("message".into(), message.into());
    if let Some(data) = data {
        error.insert("data".into(), data);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// A result response.
pub fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn request(id: Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// The protocol version to ask a server for: `pinned` when the policy pins
/// one, else what the client asked if the gateway speaks it, else the
/// newest the gateway speaks.
pub fn ask_version(client: &str, pinned: Option<&str>) -> String {
    match pinned {
        Some(pinned) => pinned.to_owned(),
        None if known(client) => client.to_owned(),
        None => LATEST.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_classified_and_ids_kept_as_written() {
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"})), Ok(Kind::Request { id: json!(7), method: "tools/list".into() }));
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "id": "a-1", "method": "ping"})), Ok(Kind::Request { id: json!("a-1"), method: "ping".into() }));
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})), Ok(Kind::Notification { method: "notifications/initialized".into() }));
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "id": 3, "result": {}})), Ok(Kind::Response { id: json!(3) }));
        assert_eq!(classify(&json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -1, "message": "m"}})), Ok(Kind::Response { id: json!("x") }));
        for bad in [
            json!([]),
            json!("text"),
            json!({"id": 1, "method": "ping"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": 3}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "result": {}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": 3}),
            json!({"jsonrpc": "2.0", "result": {}}),
            json!({"jsonrpc": "2.0", "id": 1}),
        ] {
            assert!(classify(&bad).is_err(), "{bad}");
        }
        assert_ne!(key(&json!(1)), key(&json!("1")), "an integer id and a string id stay apart");
        let big = json!({"jsonrpc": "2.0", "id": u64::MAX, "method": "ping"});
        assert_eq!(classify(&big), Ok(Kind::Request { id: json!(u64::MAX), method: "ping".into() }));
        assert_eq!(serde_json::to_string(&error(json!(u64::MAX), REFUSED, "no", None)).unwrap(), format!(r#"{{"error":{{"code":-32003,"message":"no"}},"id":{},"jsonrpc":"2.0"}}"#, u64::MAX));
    }

    #[test]
    fn batches_are_split_only_where_the_protocol_version_has_them() {
        let batch = json!([{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}, {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 0}}]);
        let split_up = split(batch.clone(), Some("2025-03-26")).unwrap();
        assert_eq!(split_up.len(), 2);
        assert_eq!(split_up[0]["id"], 1);
        for version in ["2024-11-05", "2025-06-18", "2025-11-25"] {
            let refused = split(batch.clone(), Some(version)).unwrap_err();
            assert_eq!(refused["error"]["code"], INVALID_REQUEST);
            assert!(refused["error"]["message"].as_str().unwrap().contains(version));
        }
        assert!(split(batch, None).is_err(), "no batch before initialize");
        assert!(split(json!([]), Some("2025-03-26")).is_err());
        assert!(split(json!([{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}]), Some("2025-03-26")).is_err());
        assert!(split(json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}, 5]), Some("2025-03-26")).is_err());
        assert!(split(json!(vec![json!({"jsonrpc": "2.0", "method": "notifications/x"}); 101]), Some("2025-03-26")).is_err());
        let single = split(json!({"jsonrpc": "2.0", "id": "s", "method": "ping"}), None).unwrap();
        assert_eq!(single, [json!({"jsonrpc": "2.0", "id": "s", "method": "ping"})]);
        let bad = split(json!({"jsonrpc": "2.0", "id": "s", "method": 1}), None).unwrap_err();
        assert_eq!((bad["id"].clone(), bad["error"]["code"].clone()), (json!("s"), json!(INVALID_REQUEST)), "the id of a bad message is kept when it can be");
    }

    #[test]
    fn versions_are_pinned_never_guessed() {
        assert!(VERSIONS.iter().all(|v| known(v)) && !known("2099-01-01") && !known(""));
        assert_eq!(VERSIONS.last(), Some(&LATEST));
        assert_eq!(ask_version("2025-06-18", None), "2025-06-18");
        assert_eq!(ask_version("2099-01-01", None), LATEST, "a version the gateway does not speak is not passed on");
        assert_eq!(ask_version("2025-11-25", Some("2025-06-18")), "2025-06-18", "the policy's pin wins");
        assert!(batches("2025-03-26") && !batches("2025-06-18") && !batches("2024-11-05"));
    }

    #[test]
    fn a_large_message_keeps_every_byte() {
        let text = "x".repeat(900 << 10);
        let message = request(json!("big"), "tools/call", json!({"name": "add_comment", "arguments": {"body": text}}));
        let bytes = serde_json::to_vec(&message).unwrap();
        let back: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, message);
        assert_eq!(split(back, Some("2025-06-18")).unwrap()[0]["params"]["arguments"]["body"].as_str().map(str::len), Some(900 << 10));
    }
}
