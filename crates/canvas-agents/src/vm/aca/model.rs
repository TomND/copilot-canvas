//! The data plane's JSON for `vm::aca` (docs/VM-SESSIONS-PLAN.md Appendix A,
//! from the Python SDK 0.1.0b4): request bodies built from the provider's
//! types, answers read leniently into them, the shell command `exec` sends,
//! and the checks on every name that becomes part of a URL.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};

use crate::vm::provider::{EgressDecision, EgressPolicy, ExecResult, ImageRef, Labels, Sandbox, SandboxSpec, Secret};

/// The stock image a sandbox starts from when the spec names none.
pub const STOCK_IMAGE: &str = "ubuntu";
/// The platform's idle interval, sent with its idle suspend turned off.
pub const IDLE_INTERVAL_S: u64 = 300;

/// `text` as one path segment of a URL: not empty, no `/`, `\`, `..`, `?`,
/// `#`, `%`, NUL or other control characters, no surrounding spaces.
pub fn segment<'a>(what: &str, text: &'a str) -> Result<&'a str> {
    let ok = !text.is_empty() && text.len() <= 256 && text.trim() == text && !text.contains(['/', '\\', '?', '#', '%']) && !text.contains("..") && !text.chars().any(char::is_control);
    ensure!(ok, "{what} is not a valid name (no / \\ ? # % .. or control characters)");
    Ok(text)
}

/// An absolute path in the guest, without `..` components or control characters.
pub fn guest_path(path: &str) -> Result<&str> {
    let ok = path.starts_with('/') && path.len() <= 4096 && !path.split('/').any(|c| c == "..") && !path.chars().any(char::is_control);
    ensure!(ok, "{path:?} is not an absolute guest path (/…, no .. and no control characters)");
    Ok(path)
}

/// A guest path relative to the guest user's home (the provider contract):
/// not empty, no `..` components, no control characters, not `/…`.
pub fn relative_path(path: &str) -> Result<&str> {
    let ok = !path.is_empty() && !path.starts_with('/') && path.len() <= 4096 && !path.split('/').any(|c| c == "..") && path.split('/').any(|c| !c.is_empty() && c != ".") && !path.chars().any(char::is_control);
    ensure!(ok, "{path:?} is not a guest path (relative to the home or /…, no .. and no control characters)");
    Ok(path)
}

/// `path` (a guest path) as shell text: absolute as it is, relative from `$HOME`.
pub fn shell_path(path: &str) -> Result<String> {
    Ok(if path.starts_with('/') { quote(guest_path(path)?) } else { format!("\"$HOME\"/{}", quote(relative_path(path)?)) })
}

/// The shell text that puts the uploaded file `staged` at `dest` with
/// `mode` (the folders on the way made) and removes the upload either way.
pub fn install_command(staged: &str, dest: &str, mode: u32) -> Result<String> {
    ensure!(mode <= 0o7777, "{mode:o} is not a file mode");
    let staged = quote(guest_path(staged)?);
    Ok(format!("install -D -m {mode:04o} -- {staged} {}; code=$?; rm -f -- {staged}; exit $code", shell_path(dest)?))
}

/// The shell text printing the guest user's home.
pub const HOME_COMMAND: &str = "printf %s \"$HOME\"";

/// Labels on a sandbox or in a filter: keys and values without control
/// characters, and without `,` and `=` (the filter's separators).
pub fn check_labels(labels: &Labels) -> Result<()> {
    for (key, value) in labels {
        let ok = |t: &str| !t.contains([',', '=']) && !t.chars().any(char::is_control) && t.len() <= 256;
        ensure!(!key.is_empty() && ok(key) && ok(value), "label {key:?} is not valid (no , = or control characters)");
    }
    Ok(())
}

/// The list filter: `k=v,k2=v2`.
pub fn label_filter(labels: &Labels) -> Result<String> {
    check_labels(labels)?;
    Ok(labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","))
}

/// `text` as one POSIX shell word: `'…'`, each `'` written `'\''`.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What a command reads.
#[derive(Clone, Copy, Debug)]
pub enum Stdin<'a> {
    /// Nothing (`/dev/null`).
    None,
    /// These bytes, carried in the command as base64.
    Inline(&'a [u8]),
    /// A file already in the guest, removed after the command.
    File(&'a str),
}

/// The shell text `executeShellCommand` runs for `argv`: every word quoted,
/// so nothing in it is ever read by the shell.
pub fn shell_command(argv: &[String], stdin: Stdin) -> Result<String> {
    ensure!(!argv.is_empty(), "no command to run");
    ensure!(argv.iter().all(|a| !a.contains('\0')), "a command word contains NUL");
    let words = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    // `atelier` is the satellite: the one an image build installed first.
    let words = if argv[0] == "atelier" {
        let dir = crate::vm::provider::SATELLITE_BIN.rsplit_once('/').map_or("", |(dir, _)| dir);
        format!("PATH={}:\"$PATH\"; export PATH; {words}", quote(dir))
    } else {
        words
    };
    Ok(match stdin {
        Stdin::None => format!("{words} < /dev/null"),
        Stdin::Inline(bytes) => {
            use base64::Engine as _;
            format!("printf %s {} | base64 -d | {words}", quote(&base64::engine::general_purpose::STANDARD.encode(bytes)))
        }
        Stdin::File(path) => {
            let path = quote(guest_path(path)?);
            format!("{words} < {path}; code=$?; rm -f {path}; exit $code")
        }
    })
}

/// The `executeShellCommand` body.
pub fn exec_body(command: &str) -> Value {
    json!({ "command": command })
}

/// An `Allow`/`Deny`/`Transform` word in the API's case.
fn action_word(text: &str, allowed: &[&'static str], default: &'static str) -> Result<&'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(default);
    }
    allowed.iter().copied().find(|a| a.eq_ignore_ascii_case(text)).ok_or_else(|| anyhow!("egress action {text:?} is not one of {}", allowed.join(", ")))
}

/// The `egresspolicy` body (also the `egressPolicy` of a create): deny by
/// default; header values only by reference to a group secret.
pub fn egress_body(policy: &EgressPolicy) -> Result<Value> {
    let default = action_word(&policy.default_action, &["Allow", "Deny"], "Deny")?;
    let mut rules = Vec::new();
    for (i, rule) in policy.rules.iter().enumerate() {
        let name = if rule.name.trim().is_empty() { format!("rule-{}", i + 1) } else { rule.name.trim().to_owned() };
        ensure!(!rule.host.trim().is_empty(), "egress rule {name}: no host");
        let action = action_word(&rule.action, &["Allow", "Deny", "Transform"], "Allow")?;
        let mut matcher = json!({ "host": rule.host.trim() });
        if let Some(path) = rule.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            matcher["path"] = json!(path);
        }
        if !rule.methods.is_empty() {
            matcher["methods"] = json!(rule.methods.iter().map(|m| m.trim().to_ascii_uppercase()).collect::<Vec<_>>());
        }
        let mut act = json!({ "type": action });
        if !rule.headers.is_empty() {
            ensure!(action == "Transform", "egress rule {name}: only a Transform sets headers");
            let mut headers = Vec::new();
            for h in &rule.headers {
                ensure!(!h.name.trim().is_empty() && !h.secret.trim().is_empty() && !h.key.trim().is_empty(), "egress rule {name}: every header names itself, a secret and its key");
                let mut secret_ref = json!({ "secretId": h.secret.trim(), "secretKey": h.key.trim() });
                if !h.format.is_empty() {
                    secret_ref["format"] = json!(h.format);
                }
                headers.push(json!({ "operation": "Set", "name": h.name.trim(), "valueRef": { "secretRef": secret_ref } }));
            }
            act["headers"] = Value::Array(headers);
        }
        rules.push(json!({ "name": name, "match": matcher, "action": act }));
    }
    Ok(json!({ "defaultAction": default, "rules": rules }))
}

/// The create body: the image (or the stock one), the size, the platform's
/// idle suspend **off** (the canvas suspends on its own; never an auto-delete
/// policy), labels, plain environment and the egress policy.
pub fn create_body(spec: &SandboxSpec) -> Result<Value> {
    ensure!(spec.cpu_milli > 0 && spec.memory_mi > 0, "a sandbox needs cpu and memory");
    check_labels(&spec.labels)?;
    let source = match spec.image.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => json!({ "diskImage": { "id": id } }),
        None => json!({ "diskImage": { "name": STOCK_IMAGE, "isPublic": true } }),
    };
    let mut resources = json!({ "cpu": format!("{}m", spec.cpu_milli), "memory": format!("{}Mi", spec.memory_mi) });
    if spec.disk_gi > 0 {
        resources["disk"] = json!(format!("{}Gi", spec.disk_gi));
    }
    let mut body = json!({
        "sourcesRef": source,
        "resources": resources,
        "lifecycle": { "autoSuspendPolicy": { "enabled": false, "interval": IDLE_INTERVAL_S, "mode": "Memory" } },
        "labels": spec.labels,
    });
    if !spec.env.is_empty() {
        let mut env = Map::new();
        for (key, value) in &spec.env {
            let ok = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            ensure!(ok, "environment name {key:?} is not valid");
            env.insert(key.clone(), json!(value));
        }
        body["environment"] = Value::Object(env);
    }
    if let Some(policy) = &spec.egress {
        body["egressPolicy"] = egress_body(policy)?;
    }
    Ok(body)
}

/// The `secrets/{id}` body `{"values": {key: value}}`, in a buffer zeroized
/// when dropped (sized once, so no copy is left behind by growing).
pub fn secret_body(key: &str, value: &Secret) -> Result<Secret> {
    let text = std::str::from_utf8(value.expose()).map_err(|_| anyhow!("the secret's value is not text"))?;
    ensure!(!key.trim().is_empty(), "a secret needs a key");
    let mut out = Vec::with_capacity(6 * (key.len() + text.len()) + 32);
    out.extend_from_slice(b"{\"values\":{");
    serde_json::to_writer(&mut out, key)?;
    out.push(b':');
    serde_json::to_writer(&mut out, text)?;
    out.extend_from_slice(b"}}");
    Ok(Secret::new(out))
}

fn text_at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| value[*k].as_str().or_else(|| value["properties"][*k].as_str())).filter(|s| !s.is_empty())
}

/// The last segment of an id that may be a full resource path.
fn short_id(id: &str) -> &str {
    id.trim_end_matches('/').rsplit('/').next().unwrap_or(id)
}

fn labels_of(value: &Value) -> Labels {
    let map = value["labels"].as_object().or_else(|| value["properties"]["labels"].as_object());
    map.into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()
}

/// A time as milliseconds: a number (seconds or milliseconds) or RFC 3339.
fn time_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().map(|n| if n < 100_000_000_000 { n * 1000 } else { n }),
        Value::String(s) => canvas_common::paths::parse_rfc3339_ms(s.trim()),
        _ => None,
    }
}

/// A sandbox from an answer (`id`, `state`, `labels`, also under `properties`).
pub fn sandbox(value: &Value) -> Result<Sandbox> {
    let id = text_at(value, &["id", "name"]).map(short_id).context("the sandbox in the answer has no id")?;
    let state = text_at(value, &["state", "status", "provisioningState"]).unwrap_or_default();
    Ok(Sandbox { id: id.to_owned(), state: state.to_owned(), labels: labels_of(value) })
}

/// A page of a list: `{ "value": [...], "nextLink": … }` or a bare list.
pub fn page(value: &Value) -> Result<(Vec<Value>, Option<String>)> {
    match value {
        Value::Array(items) => Ok((items.clone(), None)),
        Value::Object(map) => {
            let items = match map.get("value") {
                Some(Value::Array(items)) => items.clone(),
                None | Some(Value::Null) => Vec::new(),
                Some(_) => bail!("the list's value is not a list"),
            };
            Ok((items, map.get("nextLink").and_then(Value::as_str).filter(|l| !l.is_empty()).map(str::to_owned)))
        }
        _ => bail!("the answer is not a list"),
    }
}

/// `{ "exitCode", "stdout", "stderr" }`.
pub fn exec_result(value: &Value) -> Result<ExecResult> {
    let code = match &value["exitCode"] {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .context("the answer has no exitCode")?;
    Ok(ExecResult { code, stdout: value["stdout"].as_str().unwrap_or_default().as_bytes().to_vec(), stderr: value["stderr"].as_str().unwrap_or_default().to_owned() })
}

/// A disk image (also inside a commit's `{ "diskImage": … }`).
pub fn image(value: &Value) -> Result<ImageRef> {
    let value = if value["diskImage"].is_object() { &value["diskImage"] } else { value };
    let id = text_at(value, &["id", "name"]).map(short_id).context("the image in the answer has no id")?;
    let labels = labels_of(value);
    let name = labels.get("name").cloned().or_else(|| text_at(value, &["name"]).filter(|n| short_id(n) != id).map(str::to_owned)).unwrap_or_default();
    let created_ms = ["createdAt", "created", "createdTime", "creationTime"]
        .iter()
        .find_map(|k| time_ms(&value[*k]).or_else(|| time_ms(&value["properties"][*k])))
        .or_else(|| time_ms(&value["systemData"]["createdAt"]))
        .unwrap_or(0);
    Ok(ImageRef { id: id.to_owned(), name, created_ms })
}

/// The body of an image build.
pub fn image_build_body(base: &str, name: &str) -> Result<Value> {
    ensure!(!base.trim().is_empty(), "an image needs a base");
    let labels = BTreeMap::from([("name".to_owned(), name.to_owned())]);
    check_labels(&labels)?;
    Ok(json!({ "image": { "base": base.trim() }, "labels": labels }))
}

/// The body of a commit (an image of a running sandbox's disk).
pub fn commit_body(name: &str) -> Result<Value> {
    let labels = BTreeMap::from([("name".to_owned(), name.to_owned())]);
    check_labels(&labels)?;
    Ok(json!({ "labels": labels }))
}

/// An egress decision, read leniently (`allowed`, or an `action`/`decision` word).
pub fn decision(value: &Value) -> EgressDecision {
    let word = text_at(value, &["action", "decision", "result"]).unwrap_or_default().to_ascii_lowercase();
    EgressDecision {
        ms: ["timestamp", "time", "ms"].iter().find_map(|k| time_ms(&value[*k])).unwrap_or(0),
        host: text_at(value, &["host"]).unwrap_or_default().to_owned(),
        method: text_at(value, &["method"]).unwrap_or_default().to_owned(),
        path: text_at(value, &["path"]).unwrap_or_default().to_owned(),
        allowed: value["allowed"].as_bool().unwrap_or(word.starts_with("allow") || word == "transform"),
    }
}

/// The service's own reason for a refusal: `error.code` and `error.message`
/// (or `code`/`message`), else the text's first line.
pub fn error_parts(body: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(body);
    match serde_json::from_str::<Value>(&text) {
        Ok(json) => {
            let error = if json["error"].is_object() { &json["error"] } else { &json };
            let code = error["code"].as_str().unwrap_or_default().to_owned();
            let message = error["message"].as_str().map(str::to_owned).unwrap_or_else(|| json["error"].as_str().unwrap_or_default().to_owned());
            (code, message)
        }
        Err(_) if text.trim_start().starts_with('<') => (String::new(), String::new()),
        Err(_) => (String::new(), text.into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::provider::{EgressRule, HeaderFromSecret};

    #[test]
    fn names_that_become_url_segments_are_checked() {
        for good in ["sbx-1", "contoso-rg", "00000000-0000-0000-0000-000000000000", "img.v1"] {
            assert!(segment("x", good).is_ok(), "{good}");
        }
        for bad in ["", "a/b", "a\\b", "..", "a..b", "a?b", "a#b", "a%2fb", "a\0b", " a", "a\nb"] {
            assert!(segment("the sandbox id", bad).unwrap_err().to_string().starts_with("the sandbox id is not"), "{bad:?}");
        }
        assert!(guest_path("/home/agent/x.json").is_ok());
        for bad in ["relative", "/a/../b", "/a/\0", ""] {
            assert!(guest_path(bad).is_err(), "{bad:?}");
        }
        let labels: Labels = [("atelier.hub".into(), "h1".into()), ("atelier.role".into(), "pool".into())].into();
        assert_eq!(label_filter(&labels).unwrap(), "atelier.hub=h1,atelier.role=pool");
        assert!(label_filter(&[("a".into(), "x,y".into())].into()).is_err());
        assert!(label_filter(&[("a=b".into(), "x".into())].into()).is_err());
        assert!(label_filter(&[(String::new(), "x".into())].into()).is_err());
    }

    #[test]
    fn commands_quote_every_word() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
        let argv: Vec<String> = ["git", "status", "a b"].map(String::from).to_vec();
        assert_eq!(shell_command(&argv, Stdin::None).unwrap(), "'git' 'status' 'a b' < /dev/null");
        assert_eq!(shell_command(&argv[..1], Stdin::Inline(b"hi")).unwrap(), "printf %s 'aGk=' | base64 -d | 'git'");
        assert_eq!(shell_command(&argv[..1], Stdin::File("/tmp/in")).unwrap(), "'git' < '/tmp/in'; code=$?; rm -f '/tmp/in'; exit $code");
        assert!(shell_command(&[], Stdin::None).is_err());
        assert!(shell_command(&["a\0b".into()], Stdin::None).is_err());
        assert!(shell_command(&argv, Stdin::File("relative")).is_err());
        let satellite: Vec<String> = ["atelier", "satellite", "--prepare"].map(String::from).to_vec();
        assert_eq!(shell_command(&satellite, Stdin::None).unwrap(), "PATH='/opt/atelier/bin':\"$PATH\"; export PATH; 'atelier' 'satellite' '--prepare' < /dev/null");
    }

    #[test]
    fn guest_paths_are_absolute_or_from_the_home() {
        assert_eq!(shell_path("/opt/atelier/bin/atelier").unwrap(), "'/opt/atelier/bin/atelier'");
        assert_eq!(shell_path(".atelier/satellite.json").unwrap(), "\"$HOME\"/'.atelier/satellite.json'");
        assert_eq!(shell_path("it's/$(x)").unwrap(), r#""$HOME"/'it'\''s/$(x)'"#);
        for bad in ["", ".", "./", "../x", "a/../../b", "a\nb", "/a/../b"] {
            assert!(shell_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(install_command("/tmp/up", ".atelier/satellite.json", 0o600).unwrap(), "install -D -m 0600 -- '/tmp/up' \"$HOME\"/'.atelier/satellite.json'; code=$?; rm -f -- '/tmp/up'; exit $code");
        assert!(install_command("/tmp/up", "x", 0o17777).is_err() && install_command("up", "x", 0o600).is_err());
        #[cfg(unix)]
        {
            // The text works in a real shell: folders made, the mode set, the upload gone.
            use std::os::unix::fs::PermissionsExt;
            let home = std::env::temp_dir().join(format!("atelier-aca-install-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&home).unwrap();
            let staged = home.join("staged");
            std::fs::write(&staged, b"payload").unwrap();
            let command = install_command(&staged.display().to_string(), "a b/c.json", 0o640).unwrap();
            let out = std::process::Command::new("/bin/sh").args(["-c", &command]).env("HOME", &home).output().unwrap();
            if out.status.success() {
                assert_eq!(std::fs::read(home.join("a b/c.json")).unwrap(), b"payload");
                assert_eq!(std::fs::metadata(home.join("a b/c.json")).unwrap().permissions().mode() & 0o777, 0o640);
                assert!(!staged.exists());
            }
            let _ = std::fs::remove_dir_all(home);
        }
    }

    #[test]
    fn egress_policies_render_in_the_api_shape() {
        let policy = EgressPolicy {
            default_action: String::new(),
            rules: vec![
                EgressRule { name: "git".into(), host: "example.com".into(), path: Some("/contoso/app.git/*".into()), methods: vec!["get".into(), "POST".into()], action: "allow".into(), headers: vec![] },
                EgressRule { name: String::new(), host: "api.example.com".into(), path: None, methods: vec![], action: "transform".into(), headers: vec![HeaderFromSecret { name: "Authorization".into(), secret: "forge-token".into(), key: "value".into(), format: "Bearer {}".into() }] },
            ],
        };
        assert_eq!(
            egress_body(&policy).unwrap(),
            json!({ "defaultAction": "Deny", "rules": [
                { "name": "git", "match": { "host": "example.com", "path": "/contoso/app.git/*", "methods": ["GET", "POST"] }, "action": { "type": "Allow" } },
                { "name": "rule-2", "match": { "host": "api.example.com" }, "action": { "type": "Transform", "headers": [
                    { "operation": "Set", "name": "Authorization", "valueRef": { "secretRef": { "secretId": "forge-token", "secretKey": "value", "format": "Bearer {}" } } }
                ] } }
            ] })
        );
        let mut bad = policy.clone();
        bad.rules[0].action = "rewrite".into();
        assert!(egress_body(&bad).unwrap_err().to_string().contains("rewrite"));
        let mut bad = policy.clone();
        bad.rules[0].headers = policy.rules[1].headers.clone();
        assert!(egress_body(&bad).unwrap_err().to_string().contains("only a Transform"));
        let mut bad = policy.clone();
        bad.default_action = "Transform".into();
        assert!(egress_body(&bad).is_err());
        let mut bad = policy;
        bad.rules[1].host = " ".into();
        assert!(egress_body(&bad).unwrap_err().to_string().contains("no host"));
    }

    #[test]
    fn secret_bodies_escape_and_stay_exact() {
        let body = secret_body("value", &Secret::new(b"s3\"cr\\et\n".to_vec())).unwrap();
        assert_eq!(std::str::from_utf8(body.expose()).unwrap(), r#"{"values":{"value":"s3\"cr\\et\n"}}"#);
        assert!(secret_body("value", &Secret::new(vec![0xff])).is_err());
        assert!(secret_body(" ", &Secret::new(b"x".to_vec())).is_err());
    }

    #[test]
    fn answers_are_read_leniently() {
        let s = sandbox(&json!({ "id": "sbx-1", "state": "Running", "labels": { "atelier.box": "b1" } })).unwrap();
        assert_eq!((s.id.as_str(), s.running(), s.labels["atelier.box"].as_str()), ("sbx-1", true, "b1"));
        let s = sandbox(&json!({ "id": "/subscriptions/x/sandboxes/sbx-2", "properties": { "state": "Suspended", "labels": { "a": "b" } } })).unwrap();
        assert_eq!((s.id.as_str(), s.suspended(), s.labels.len()), ("sbx-2", true, 1));
        assert!(sandbox(&json!({ "state": "Running" })).is_err());
        let (items, next) = page(&json!({ "value": [1, 2], "nextLink": "https://x/next" })).unwrap();
        assert_eq!((items.len(), next.as_deref()), (2, Some("https://x/next")));
        assert_eq!(page(&json!([1])).unwrap().0.len(), 1);
        assert!(page(&json!({ "value": 3 })).is_err() && page(&json!("x")).is_err());
        let r = exec_result(&json!({ "exitCode": 2, "stdout": "out", "stderr": "err" })).unwrap();
        assert_eq!((r.code, r.stdout.as_slice(), r.stderr.as_str()), (2, b"out".as_slice(), "err"));
        assert!(exec_result(&json!({ "stdout": "" })).is_err());
        let i = image(&json!({ "diskImage": { "id": "img-1", "labels": { "name": "app-2026" }, "createdAt": "2026-11-09T02:00:00Z" } })).unwrap();
        assert_eq!((i.id.as_str(), i.name.as_str(), i.created_ms), ("img-1", "app-2026", 1_794_189_600_000));
        let d = decision(&json!({ "timestamp": 1_794_189_600, "host": "evil.example.com", "method": "GET", "path": "/", "action": "Deny" }));
        assert_eq!((d.ms, d.allowed, d.host.as_str()), (1_794_189_600_000, false, "evil.example.com"));
        assert!(decision(&json!({ "allowed": true })).allowed);
        assert_eq!(error_parts(br#"{"error": {"code": "Conflict", "message": "is running"}}"#), ("Conflict".into(), "is running".into()));
        assert_eq!(error_parts(b"<html>bad gateway</html>"), (String::new(), String::new()));
        assert_eq!(error_parts(b"plain words"), (String::new(), "plain words".into()));
    }

    #[test]
    fn create_bodies_turn_idle_suspend_off_and_omit_what_is_empty() {
        let spec = SandboxSpec { image: Some(" ".into()), cpu_milli: 500, memory_mi: 1024, disk_gi: 0, ..Default::default() };
        assert_eq!(
            create_body(&spec).unwrap(),
            json!({
                "sourcesRef": { "diskImage": { "name": "ubuntu", "isPublic": true } },
                "resources": { "cpu": "500m", "memory": "1024Mi" },
                "lifecycle": { "autoSuspendPolicy": { "enabled": false, "interval": 300, "mode": "Memory" } },
                "labels": {},
            })
        );
        assert!(create_body(&SandboxSpec { cpu_milli: 0, ..spec.clone() }).is_err());
        assert!(create_body(&SandboxSpec { env: vec![("1X".into(), "v".into())], ..spec.clone() }).is_err());
        assert!(create_body(&SandboxSpec { labels: [("a".into(), "b,c".into())].into(), ..spec }).is_err());
    }
}
