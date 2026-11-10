//! What the VM boxes' guests were refused, for the VM view (`VmView::refusals`,
//! `VmInfo::refusals`): the MCP gateway's (`vm::mcp::policy`, its
//! `refusals.jsonl`) and the egress points' (each sandbox's decisions the
//! provider recorded, collected here into `<home>/vm/egress/refusals.jsonl`).
//! Never an argument's value or a header.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use canvas_model::vm::Refusal;
use serde::Deserialize;

use super::launch::Intent;
use super::provider::Provider;

/// The newest lines read from each file.
const TAIL: u64 = 256 << 10;
/// Refusals the view lists.
const SHOWN: usize = 50;
const MAX: u64 = 5 << 20;

#[derive(Deserialize, Default)]
#[serde(default)]
struct Line {
    ms: u64,
    r#box: String,
    kind: String,
    server: String,
    tool: String,
    rule: String,
    why: String,
}

pub fn egress_path(home: &Path) -> PathBuf {
    home.join("vm").join("egress").join("refusals.jsonl")
}

fn tail(path: &Path) -> Vec<Line> {
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut text = String::new();
    let _ = file.take(TAIL).read_to_string(&mut text);
    let text = if start > 0 { text.split_once('\n').map(|(_, rest)| rest).unwrap_or_default() } else { &text };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// The newest refusals and how many each box had (in what was read).
pub fn read(home: &Path) -> (Vec<Refusal>, HashMap<String, u32>) {
    let mut lines: Vec<Line> = tail(&super::mcp::policy::refusals_path(home));
    lines.extend(tail(&egress_path(home)));
    let mut counts: HashMap<String, u32> = HashMap::new();
    for line in &lines {
        *counts.entry(line.r#box.clone()).or_default() += 1;
    }
    lines.sort_by_key(|l| std::cmp::Reverse(l.ms));
    let refusals = lines
        .into_iter()
        .take(SHOWN)
        .map(|l| {
            let what = if l.kind == "egress" { format!("{} {}", l.tool, l.server) } else { format!("{} {}", l.server, l.tool) };
            let why = if l.rule.is_empty() { l.why } else { format!("{}: {}", l.rule, l.why) };
            Refusal { ms: l.ms, r#box: l.r#box, kind: if l.kind.is_empty() { "mcp".into() } else { l.kind }, what: what.trim().to_owned(), why }
        })
        .collect();
    (refusals, counts)
}

fn seen_path(home: &Path) -> PathBuf {
    home.join("vm").join("egress").join("seen.json")
}

/// Takes the running boxes' egress refusals the provider recorded since the
/// last call into `<home>/vm/egress/refusals.jsonl`; how many were new.
pub fn collect_egress(home: &Path, provider: &dyn Provider, intents: &[Intent]) -> usize {
    let mut seen: BTreeMap<String, u64> = std::fs::read(seen_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut lines = String::new();
    let mut taken = 0;
    for intent in intents.iter().filter(|i| !i.sandbox.is_empty()) {
        let Ok(decisions) = provider.egress_decisions(&intent.sandbox) else { continue };
        let last = seen.get(&intent.sandbox).copied().unwrap_or(0);
        let mut newest = last;
        for decision in decisions.into_iter().filter(|d| !d.allowed && d.ms > last) {
            newest = newest.max(decision.ms);
            let line = serde_json::json!({ "ms": decision.ms, "sandbox": intent.sandbox, "box": intent.r#box, "kind": "egress", "server": decision.host, "tool": format!("{} {}", decision.method, decision.path).trim(), "rule": "", "why": "the box's egress policy does not allow it" });
            lines.push_str(&line.to_string());
            lines.push('\n');
            taken += 1;
        }
        seen.insert(intent.sandbox.clone(), newest);
    }
    seen.retain(|sandbox, _| intents.iter().any(|i| i.sandbox == *sandbox));
    if taken > 0 {
        let path = egress_path(home);
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(home));
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX) {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(lines.as_bytes());
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&seen) {
        let _ = std::fs::create_dir_all(home.join("vm").join("egress"));
        let _ = crate::paths::write_atomic(&seen_path(home), &bytes);
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::provider::{EgressDecision, SandboxSpec};

    #[test]
    fn mcp_and_egress_refusals_reach_the_view_newest_first_and_once_each() {
        let home = std::env::temp_dir().join(format!("atelier-refusals-{}", uuid::Uuid::new_v4()));
        let root = home.join("local");
        let local = crate::vm::local::Local::new(root.clone());
        let sandbox = local.create(&SandboxSpec::default()).unwrap();
        let intent = Intent { r#box: "b1".into(), sandbox: sandbox.id.clone(), ..Default::default() };
        local.log_egress(&sandbox.id, &EgressDecision { ms: 10, host: "evil.example.net".into(), method: "POST".into(), path: "/x".into(), allowed: false }).unwrap();
        local.log_egress(&sandbox.id, &EgressDecision { ms: 11, host: "example.com".into(), method: "GET".into(), path: "/".into(), allowed: true }).unwrap();
        assert_eq!(collect_egress(&home, &local, std::slice::from_ref(&intent)), 1, "the refusal only");
        assert_eq!(collect_egress(&home, &local, std::slice::from_ref(&intent)), 0, "once");
        super::super::mcp::policy::record(&home, &super::super::mcp::policy::Record { ms: 20, sandbox: sandbox.id.clone(), r#box: "b1".into(), kind: "mcp".into(), server: "forge".into(), tool: "vote".into(), rule: "deny: vote".into(), why: "denied by the pack".into() });
        let (refusals, counts) = read(&home);
        assert_eq!(counts.get("b1"), Some(&2));
        assert_eq!((refusals[0].kind.as_str(), refusals[0].what.as_str()), ("mcp", "forge vote"));
        assert_eq!((refusals[1].kind.as_str(), refusals[1].what.as_str()), ("egress", "POST /x evil.example.net"));
        crate::vm::local::Local::delete(&local, &sandbox.id).unwrap();
        let _ = std::fs::remove_dir_all(home);
    }
}
