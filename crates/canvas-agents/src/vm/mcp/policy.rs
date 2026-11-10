//! The pack's `mcp-policy.yaml` applied to one box's session with one of
//! the hub's servers. Default deny everywhere: a server is reachable only
//! when the box's preset lists it and the pack has rules for it; a tool is
//! listed and callable only when a rule allows it; every argument of a call
//! must be one the tool declares; a server-initiated request (`roots/list`,
//! `sampling/createMessage`, `elicitation/create`) passes only when a rule
//! names that method (`allow: roots/list`: tool names never contain `/`).
//!
//! Rules are tried in order; the first whose tool matches (exact, or a `*`
//! glob) and whose `when` holds decides. `when` maps an argument to the
//! value it must have, with `{{box.branch}}`, `{{box.repo}}` and
//! `{{box.id}}` filled from the box; a scalar argument matches its text
//! (`true`, `42`), anything else never matches.
//!
//! Refusals are recorded in `<home>/vm/mcp/refusals.jsonl` (time, sandbox,
//! box, server, tool, rule, why); argument values are never recorded.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::vm::pack::{McpPolicy, McpRule};

/// What a box's placeholders are filled with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vars {
    pub branch: String,
    pub repo: String,
    pub r#box: String,
}

/// Why something was refused: the rule that decided (or `default deny`)
/// and a sentence for the agent; never an argument's value.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub rule: String,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Allow,
    Refuse(Refusal),
}

/// One box's policy for one server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerPolicy {
    pub server: String,
    /// `mode: shared`: one process serves every session of this server.
    pub shared: bool,
    /// The protocol version the policy pins.
    pub protocol: Option<String>,
    rules: Vec<McpRule>,
    vars: Vars,
}

pub const DEFAULT_DENY: &str = "default deny";

/// The policy of `server` for a box whose preset is `preset`, or why the box
/// may not use that server at all.
pub fn for_box(policy: &McpPolicy, preset: Option<&str>, server: &str, vars: Vars) -> Result<ServerPolicy, Refusal> {
    let refuse = |rule: &str, why: String| Err(Refusal { rule: rule.into(), why });
    let Some(preset) = preset.map(str::trim).filter(|p| !p.is_empty()) else {
        return refuse("no preset", "this session was started without an MCP preset: it uses none of the hub's MCP servers".into());
    };
    let Some(servers) = policy.presets.get(preset) else {
        return refuse("no preset", format!("the VM pack has no MCP preset {preset:?}"));
    };
    if !servers.iter().any(|s| s == server) {
        return refuse(&format!("preset {preset}"), format!("MCP preset {preset:?} does not include the server {server:?}"));
    }
    let Some(rules) = policy.servers.get(server) else {
        return refuse(DEFAULT_DENY, format!("mcp-policy.yaml has no rules for the server {server:?}"));
    };
    let shared = match rules.mode.as_str() {
        "" | "per_session" => false,
        "shared" => true,
        other => return refuse("mode", format!("mcp-policy.yaml: {server}: mode {other:?} is per_session or shared")),
    };
    let protocol = Some(rules.protocol.trim().to_owned()).filter(|p| !p.is_empty());
    if let Some(version) = &protocol
        && !super::jsonrpc::known(version)
    {
        return refuse("protocol", format!("mcp-policy.yaml pins protocol {version} for {server}; the gateway speaks {}", super::jsonrpc::VERSIONS.join(", ")));
    }
    Ok(ServerPolicy { server: server.to_owned(), shared, protocol, rules: rules.rules.clone(), vars })
}

/// Whether `pattern` (a name, or a glob with `*`) matches `name`.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

fn method_rule(rule: &McpRule) -> bool {
    rule.tool.contains('/')
}

/// A rule as the refusals show it: `allow create_pull_request`.
fn describe(rule: &McpRule) -> String {
    format!("{} {}", rule.action, rule.tool)
}

impl ServerPolicy {
    fn tool_rules<'a>(&'a self, tool: &'a str) -> impl Iterator<Item = &'a McpRule> + 'a {
        self.rules.iter().filter(move |r| !method_rule(r) && glob(&r.tool, tool))
    }

    /// Whether `tools/list` shows `tool`: a rule allows it (perhaps with
    /// conditions) before any rule denies it outright.
    pub fn lists(&self, tool: &str) -> bool {
        for rule in self.tool_rules(tool) {
            match rule.action.as_str() {
                "allow" => return true,
                "deny" if rule.when.is_empty() => return false,
                _ => {}
            }
        }
        false
    }

    /// Whether a call of `tool` with `arguments` is allowed.
    pub fn call(&self, tool: &str, arguments: &Map<String, Value>) -> Verdict {
        let mut mismatch: Option<Refusal> = None;
        for rule in self.tool_rules(tool) {
            match self.holds(rule, arguments) {
                Ok(()) if rule.action == "allow" => return Verdict::Allow,
                // A rule that would have allowed it with other arguments explains the refusal best.
                Ok(()) => return Verdict::Refuse(mismatch.unwrap_or_else(|| Refusal { rule: describe(rule), why: format!("{tool} is not allowed in VM sessions (mcp-policy.yaml: {})", describe(rule)) })),
                Err(why) if rule.action == "allow" && mismatch.is_none() => mismatch = Some(Refusal { rule: describe(rule), why: format!("{tool} is allowed only when {why}") }),
                Err(_) => {}
            }
        }
        Verdict::Refuse(mismatch.unwrap_or_else(|| Refusal { rule: DEFAULT_DENY.into(), why: format!("{tool} is not allowed in VM sessions (mcp-policy.yaml allows it nowhere: default deny)") }))
    }

    /// Whether a server-initiated request `method` may reach the guest: a
    /// rule naming the method allows it (without conditions).
    pub fn allows_method(&self, method: &str) -> bool {
        self.rules.iter().filter(|r| method_rule(r) && glob(&r.tool, method)).map(|r| r.action == "allow" && r.when.is_empty()).next().unwrap_or(false)
    }

    /// `Err`: the first condition of `rule` that `arguments` do not meet, in words.
    fn holds(&self, rule: &McpRule, arguments: &Map<String, Value>) -> Result<(), String> {
        for (name, template) in &rule.when {
            let expected = self.fill(template).map_err(|placeholder| format!("{name} matches {{{{{placeholder}}}}}, which this gateway does not know"))?;
            let actual = match arguments.get(name) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Bool(b)) => Some(b.to_string()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            if actual.as_deref() != Some(expected.as_str()) {
                let what = if template.replace(' ', "") == "{{box.branch}}" { format!("this session's branch, {expected:?}") } else { format!("{expected:?}") };
                return Err(format!("{name} is {what}"));
            }
        }
        Ok(())
    }

    /// `template` with the box's placeholders filled (`Err`: an unknown one).
    fn fill(&self, template: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                out.push_str(&rest[start..]);
                return Ok(out);
            };
            let name = after[..end].trim();
            out.push_str(match name {
                "box.branch" => &self.vars.branch,
                "box.repo" => &self.vars.repo,
                "box.id" => &self.vars.r#box,
                other => return Err(other.to_owned()),
            });
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// The first argument the tool's `inputSchema` does not declare (`Err`), or
/// arguments that are not an object.
pub fn check_arguments(schema: &Value, arguments: &Value) -> Result<(), String> {
    let arguments = match arguments {
        Value::Null => return Ok(()),
        Value::Object(map) => map,
        _ => return Err("arguments are not an object".into()),
    };
    let declared = schema.get("properties").and_then(Value::as_object);
    for name in arguments.keys() {
        if !declared.is_some_and(|d| d.contains_key(name)) {
            return Err(format!("unknown argument {name:?}"));
        }
    }
    Ok(())
}

/// One recorded refusal (a line of `refusals.jsonl`).
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub ms: u64,
    pub sandbox: String,
    pub r#box: String,
    /// Always `mcp` (egress refusals share the VM details).
    pub kind: String,
    pub server: String,
    /// The tool or the method.
    pub tool: String,
    pub rule: String,
    pub why: String,
}

/// `refusals.jsonl` grows to this size, then becomes `refusals.jsonl.old`.
const REFUSALS_MAX: u64 = 5 << 20;

pub fn refusals_path(home: &Path) -> PathBuf {
    home.join("vm").join("mcp").join("refusals.jsonl")
}

/// Appends `record` to the refusals (best effort: a refusal is never held up by its record).
pub fn record(home: &Path, record: &Record) {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let path = refusals_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > REFUSALS_MAX) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
    }
    let Ok(mut line) = serde_json::to_string(record) else { return };
    line.push('\n');
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example() -> McpPolicy {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
        crate::vm::pack::load(&folder).0.mcp
    }

    fn vars() -> Vars {
        Vars { branch: "atelier/fix-login".into(), repo: "contoso-app".into(), r#box: "b1".into() }
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn the_example_policy_lists_and_allows_only_what_it_names() {
        let policy = for_box(&example(), Some("forge-readonly"), "forge", vars()).unwrap();
        assert!(!policy.shared && policy.protocol.is_none());
        assert!(policy.lists("list_pull_requests") && policy.lists("create_pull_request"));
        assert!(!policy.lists("update_pull_request") && !policy.lists("vote") && !policy.lists("add_comment"), "denied and unnamed tools are not listed");
        assert_eq!(policy.call("list_pull_requests", &Map::new()), Verdict::Allow);
        let ok = args(json!({"draft": true, "head": "atelier/fix-login", "base": "main", "title": "Fix"}));
        assert_eq!(policy.call("create_pull_request", &ok), Verdict::Allow);
        assert_eq!(policy.call("create_pull_request", &args(json!({"draft": "true", "head": "atelier/fix-login"}))), Verdict::Allow, "the text of a scalar");
        let Verdict::Refuse(other_head) = policy.call("create_pull_request", &args(json!({"draft": true, "head": "main"}))) else { panic!("another head") };
        assert_eq!(other_head.rule, "allow create_pull_request");
        assert!(other_head.why.contains("head is this session's branch, \"atelier/fix-login\"") && !other_head.why.contains("\"main\""), "{}", other_head.why);
        let Verdict::Refuse(not_draft) = policy.call("create_pull_request", &args(json!({"draft": false, "head": "atelier/fix-login"}))) else { panic!("not a draft") };
        assert!(not_draft.why.contains("draft is \"true\""), "{}", not_draft.why);
        assert!(matches!(policy.call("create_pull_request", &args(json!({"draft": [true], "head": "atelier/fix-login"}))), Verdict::Refuse(_)), "a list never matches");
        assert!(matches!(policy.call("create_pull_request", &args(json!({"head": "atelier/fix-login"}))), Verdict::Refuse(_)), "a missing argument never matches");
        let Verdict::Refuse(vote) = policy.call("vote", &Map::new()) else { panic!("vote") };
        assert_eq!(vote.rule, "deny vote");
        let Verdict::Refuse(update) = policy.call("update_pull_request", &Map::new()) else { panic!("update") };
        assert_eq!(update.rule, "deny update_pull_request");
        let Verdict::Refuse(comment) = policy.call("add_comment", &Map::new()) else { panic!("comment") };
        assert_eq!(comment.rule, DEFAULT_DENY);
        for method in ["roots/list", "sampling/createMessage", "elicitation/create", "ping"] {
            assert!(!policy.allows_method(method), "{method} is refused unless a rule allows it");
        }
    }

    #[test]
    fn a_server_is_reachable_only_through_the_boxs_preset_and_a_policy() {
        let mut pack = example();
        assert_eq!(for_box(&pack, None, "forge", vars()).unwrap_err().rule, "no preset");
        assert_eq!(for_box(&pack, Some(""), "forge", vars()).unwrap_err().rule, "no preset");
        assert!(for_box(&pack, Some("other"), "forge", vars()).unwrap_err().why.contains("no MCP preset"));
        assert!(for_box(&pack, Some("forge-readonly"), "github", vars()).unwrap_err().why.contains("does not include"));
        pack.presets.insert("wide".into(), vec!["forge".into(), "nopolicy".into()]);
        assert_eq!(for_box(&pack, Some("wide"), "nopolicy", vars()).unwrap_err().rule, DEFAULT_DENY);
        pack.servers.get_mut("forge").unwrap().protocol = "2099-01-01".into();
        assert!(for_box(&pack, Some("wide"), "forge", vars()).unwrap_err().why.contains("2099-01-01"));
        pack.servers.get_mut("forge").unwrap().protocol = "2025-06-18".into();
        pack.servers.get_mut("forge").unwrap().mode = "shared".into();
        let policy = for_box(&pack, Some("wide"), "forge", vars()).unwrap();
        assert!(policy.shared && policy.protocol.as_deref() == Some("2025-06-18"));
        pack.servers.get_mut("forge").unwrap().mode = "pooled".into();
        assert_eq!(for_box(&pack, Some("wide"), "forge", vars()).unwrap_err().rule, "mode");
    }

    #[test]
    fn rules_go_in_order_with_globs_placeholders_and_methods() {
        let rule = |action: &str, tool: &str, when: &[(&str, &str)]| McpRule { action: action.into(), tool: tool.into(), when: when.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let mut pack = McpPolicy::default();
        pack.presets.insert("p".into(), vec!["s".into()]);
        pack.servers.insert(
            "s".into(),
            crate::vm::pack::McpServerPolicy {
                mode: String::new(),
                protocol: String::new(),
                rules: vec![
                    rule("deny", "list_*", &[("state", "closed")]),
                    rule("allow", "list_*", &[]),
                    rule("allow", "push", &[("ref", "refs/heads/{{ box.branch }}"), ("repo", "{{box.repo}}")]),
                    rule("allow", "tag", &[("name", "{{box.secret}}")]),
                    rule("deny", "*", &[]),
                    rule("allow", "never_reached", &[]),
                    rule("allow", "roots/list", &[]),
                    rule("deny", "sampling/*", &[]),
                    rule("allow", "elicitation/create", &[("x", "y")]),
                ],
            },
        );
        let policy = for_box(&pack, Some("p"), "s", vars()).unwrap();
        assert!(policy.lists("list_issues") && policy.lists("list_pull_requests"), "a conditional deny does not hide a tool");
        assert_eq!(policy.call("list_issues", &args(json!({"state": "open"}))), Verdict::Allow);
        assert!(matches!(policy.call("list_issues", &args(json!({"state": "closed"}))), Verdict::Refuse(r) if r.rule == "deny list_*"));
        assert_eq!(policy.call("push", &args(json!({"ref": "refs/heads/atelier/fix-login", "repo": "contoso-app"}))), Verdict::Allow);
        assert!(matches!(policy.call("push", &args(json!({"ref": "refs/heads/main", "repo": "contoso-app"}))), Verdict::Refuse(r) if r.why.contains("refs/heads/atelier/fix-login")));
        assert!(matches!(policy.call("tag", &args(json!({"name": "x"}))), Verdict::Refuse(r) if r.why.contains("does not know") && r.rule == "allow tag"), "an unknown placeholder never matches");
        assert_eq!(policy.fill("{{box.secret}}"), Err("box.secret".into()));
        assert_eq!(policy.fill("{{box.id}}:{{box.repo}} {{"), Ok("b1:contoso-app {{".into()));
        assert!(!policy.lists("never_reached") && matches!(policy.call("never_reached", &Map::new()), Verdict::Refuse(r) if r.rule == "deny *"));
        assert!(policy.allows_method("roots/list") && !policy.allows_method("sampling/createMessage") && !policy.allows_method("elicitation/create"));
        assert!(!policy.lists("roots/list"), "a method is never a tool");
        assert!(glob("a*b*c", "aXbYc") && glob("*", "") && !glob("a*b", "ab_") && glob("ab*", "ab") && !glob("a*a", "a"));
    }

    #[test]
    fn arguments_must_be_declared_by_the_tool() {
        let schema = json!({"type": "object", "properties": {"head": {"type": "string"}, "draft": {"type": "boolean"}}});
        assert_eq!(check_arguments(&schema, &json!({"head": "x"})), Ok(()));
        assert_eq!(check_arguments(&schema, &Value::Null), Ok(()));
        assert_eq!(check_arguments(&schema, &json!({"head": "x", "force": true})), Err("unknown argument \"force\"".into()));
        assert!(check_arguments(&schema, &json!(["head"])).is_err());
        assert!(check_arguments(&json!({"type": "object"}), &json!({"a": 1})).is_err(), "no properties: no argument");
        assert_eq!(check_arguments(&json!({"type": "object"}), &json!({})), Ok(()));
    }

    #[test]
    fn refusals_are_appended_and_rotated() {
        let home = std::env::temp_dir().join(format!("atelier-mcp-refusals-{}", uuid::Uuid::new_v4()));
        let one = Record { ms: 1, sandbox: "sbx-1".into(), r#box: "b".into(), kind: "mcp".into(), server: "forge".into(), tool: "vote".into(), rule: "deny vote".into(), why: "no".into() };
        record(&home, &one);
        record(&home, &one);
        let text = std::fs::read_to_string(refusals_path(&home)).unwrap();
        assert_eq!(text.lines().count(), 2);
        let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!((line["box"].as_str(), line["rule"].as_str(), line["kind"].as_str()), (Some("b"), Some("deny vote"), Some("mcp")));
        std::fs::write(refusals_path(&home), vec![b'x'; REFUSALS_MAX as usize + 1]).unwrap();
        record(&home, &one);
        assert_eq!(std::fs::read_to_string(refusals_path(&home)).unwrap().lines().count(), 1);
        assert!(refusals_path(&home).with_extension("jsonl.old").is_file());
        let _ = std::fs::remove_dir_all(home);
    }
}
