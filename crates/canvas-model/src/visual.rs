//! Canvas visuals that are part of the document: dust on unused sessions
//! (`Node::last_used_ms`, `Doc::dust_after_days`) and subsession stacks
//! (`Node::stacked`). Pure functions shared by the clients and the service.

use std::collections::{HashMap, HashSet};

use crate::doc::{Doc, Node, NodeKind};

/// Days without use before a session gathers dust (the canvas default).
pub const DEFAULT_DUST_DAYS: u32 = 3;
/// The choices of Settings › Dust unused sessions after (0: off).
pub const DUST_CHOICES: [u32; 5] = [0, 1, 3, 7, 14];
/// `last_used_ms` moves forward only by at least this much (fewer edits).
pub const USED_STEP_MS: u64 = 10 * 60_000;
const DAY_MS: u64 = 86_400_000;

/// How dusty a session looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dust {
    #[default]
    None,
    Light,
    Medium,
    Heavy,
}

impl Dust {
    /// 0 (none) … 3 (heavy).
    pub fn level(self) -> u8 {
        self as u8
    }
}

/// The dust of a session unused for `age_ms`, gathering from `after_days`
/// (light), then medium from max(14, 2 × after) days and heavy from
/// max(60, 4 × after) days. `after_days == 0` turns dust off.
pub fn dust_for_age(age_ms: u64, after_days: u32) -> Dust {
    if after_days == 0 {
        return Dust::None;
    }
    let after = after_days as u64;
    let days = age_ms / DAY_MS;
    if days >= (4 * after).max(60) {
        Dust::Heavy
    } else if days >= (2 * after).max(14) {
        Dust::Medium
    } else if days >= after {
        Dust::Light
    } else {
        Dust::None
    }
}

/// "unused 12d" (days), "unused 5h" below a day.
pub fn unused_text(last_used_ms: u64, now_ms: u64) -> String {
    format!("unused {}", crate::paths::format_age(last_used_ms, now_ms))
}

/// The newest use a session shows (started, output, typing, history), when it
/// is worth recording: the first value, or one at least [`USED_STEP_MS`]
/// newer than the recorded one.
pub fn next_last_used(recorded: Option<u64>, seen: u64) -> Option<u64> {
    match recorded {
        _ if seen == 0 => None,
        None => Some(seen),
        Some(at) if seen >= at + USED_STEP_MS => Some(seen),
        Some(_) => None,
    }
}

/// The first `last_used_ms` of a session on a canvas from before it was
/// recorded: its history's newest activity, else when you last looked at it.
pub fn migrated_last_used(node: &Node, history_ms: u64) -> Option<u64> {
    if history_ms > 0 { Some(history_ms) } else { node.reviewed_ms }
}

/// A subsession: a session box linked to a parent box on the canvas.
pub fn is_subsession(doc_ids: &HashSet<&str>, node: &Node) -> bool {
    node.kind == NodeKind::Session && node.passive.is_none() && node.parent.as_deref().is_some_and(|p| p != node.id && doc_ids.contains(p))
}

/// Session boxes by parent (subsessions only), in document order.
pub fn children_of(doc: &Doc) -> HashMap<&str, Vec<&Node>> {
    let ids: HashSet<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut children: HashMap<&str, Vec<&Node>> = HashMap::new();
    for node in doc.nodes.iter().filter(|n| is_subsession(&ids, n)) {
        children.entry(node.parent.as_deref().unwrap_or_default()).or_default().push(node);
    }
    children
}

/// The subsessions under `root`, recursively, in document order (oldest first).
pub fn descendants(doc: &Doc, root: &str) -> Vec<String> {
    let children = children_of(doc);
    let mut out = Vec::new();
    let mut seen: HashSet<&str> = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        for child in children.get(id).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(child.id.as_str()) {
                out.push(child.id.clone());
                queue.push(child.id.as_str());
            }
        }
    }
    let order: HashMap<&str, usize> = doc.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    out.sort_by_key(|id| order.get(id.as_str()).copied().unwrap_or(usize::MAX));
    out
}

/// Everything a stack holds: its subsessions and their subagent boxes.
pub fn stack_members(doc: &Doc, root: &str) -> Vec<String> {
    let sessions = descendants(doc, root);
    let set: HashSet<&str> = sessions.iter().map(String::as_str).collect();
    let agents: Vec<String> = doc
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Agent && n.parent.as_deref().is_some_and(|p| set.contains(p)))
        .map(|n| n.id.clone())
        .collect();
    sessions.into_iter().chain(agents).collect()
}

/// Which stack hides each box: node id → the stacked ancestor highest up the
/// parent chain (subagents follow their session).
pub fn stacked_under(doc: &Doc) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut roots: Vec<&Node> = doc.nodes.iter().filter(|n| n.stacked && n.kind == NodeKind::Session).collect();
    // Outer stacks first, so they win over stacks nested in them.
    let depth = |n: &Node| {
        let mut d = 0;
        let mut at = n.parent.as_deref();
        while let Some(p) = at.and_then(|p| doc.node(p)) {
            d += 1;
            if d > 64 {
                break;
            }
            at = p.parent.as_deref();
        }
        d
    };
    roots.sort_by_key(|n| depth(n));
    for root in roots {
        if out.contains_key(&root.id) {
            continue;
        }
        for id in stack_members(doc, &root.id) {
            out.entry(id).or_insert_with(|| root.id.clone());
        }
    }
    out
}

/// Nodes hidden by "hide all subsessions": subsessions and their subagents,
/// each mapped to its visible ancestor at the top of the parent chain.
pub fn hidden_subsessions(doc: &Doc) -> HashMap<String, String> {
    let ids: HashSet<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut out = HashMap::new();
    for node in doc.nodes.iter().filter(|n| is_subsession(&ids, n)) {
        let mut top = node;
        for _ in 0..64 {
            match top.parent.as_deref().and_then(|p| doc.node(p)).filter(|p| is_subsession(&ids, top) && p.id != node.id) {
                Some(parent) => top = parent,
                None => break,
            }
        }
        if top.id != node.id {
            out.insert(node.id.clone(), top.id.clone());
        }
    }
    let agents: Vec<(String, String)> = doc
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Agent)
        .filter_map(|n| n.parent.as_deref().and_then(|p| out.get(p)).map(|top| (n.id.clone(), top.clone())))
        .collect();
    out.extend(agents);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, parent: Option<&str>, x: i32, y: i32) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": y })).unwrap();
        node.parent = parent.map(str::to_owned);
        node
    }

    fn agent(id: &str, parent: &str) -> Node {
        Node { kind: NodeKind::Agent, ..session(id, Some(parent), 0, 0) }
    }

    #[test]
    fn dust_levels_follow_the_age_and_the_setting() {
        let day = DAY_MS;
        assert_eq!(dust_for_age(2 * day, 3), Dust::None);
        assert_eq!(dust_for_age(3 * day, 3), Dust::Light);
        assert_eq!(dust_for_age(13 * day, 3), Dust::Light);
        assert_eq!(dust_for_age(14 * day, 3), Dust::Medium);
        assert_eq!(dust_for_age(59 * day, 3), Dust::Medium);
        assert_eq!(dust_for_age(60 * day, 3), Dust::Heavy);
        assert_eq!(dust_for_age(400 * day, 0), Dust::None, "off");
        assert_eq!(dust_for_age(day, 1), Dust::Light);
        assert_eq!(dust_for_age(20 * day, 14), Dust::Light, "medium from 28 days when the threshold is 14");
        assert_eq!(dust_for_age(28 * day, 14), Dust::Medium);
        assert_eq!(unused_text(0, 12 * day + 5), "unused 12d");
        assert_eq!(unused_text(0, 5 * 3_600_000), "unused 5h");
        assert!(Dust::Heavy > Dust::Light && Dust::Medium.level() == 2);
    }

    #[test]
    fn last_used_moves_forward_in_steps_and_migrates_from_history_or_reviews() {
        assert_eq!(next_last_used(None, 0), None, "nothing seen");
        assert_eq!(next_last_used(None, 5), Some(5));
        assert_eq!(next_last_used(Some(1_000), 1_000 + USED_STEP_MS - 1), None, "small steps are not recorded");
        assert_eq!(next_last_used(Some(1_000), 1_000 + USED_STEP_MS), Some(1_000 + USED_STEP_MS));
        assert_eq!(next_last_used(Some(9_000_000), 1_000), None, "never backwards");
        let mut node = session("a", None, 0, 0);
        assert_eq!(migrated_last_used(&node, 0), None);
        node.reviewed_ms = Some(77);
        assert_eq!(migrated_last_used(&node, 0), Some(77), "when you last looked, without history");
        assert_eq!(migrated_last_used(&node, 500), Some(500), "the history's newest activity first");
        // Old canvases load without the fields; unset fields are not written.
        let old: Doc = serde_json::from_str(r#"{"version":2,"nodes":[{"id":"a","kind":"session","name":"A","x":0,"y":0}],"groups":[],"view":{"x":0,"y":0}}"#).unwrap();
        assert_eq!((old.nodes[0].last_used_ms, old.nodes[0].stacked, old.dust_after_days), (None, false, DEFAULT_DUST_DAYS));
        let text = serde_json::to_string(&old.nodes[0]).unwrap();
        assert!(!text.contains("last_used_ms") && !text.contains("stacked"), "{text}");
    }

    fn family() -> Doc {
        // root → a → a1 (with a subagent), root → b; c is unrelated; p is passive.
        let mut doc = Doc::default();
        doc.nodes.push(session("root", None, 0, 0));
        doc.nodes.push(session("a", Some("root"), 40, 0));
        doc.nodes.push(session("a1", Some("a"), 80, 0));
        doc.nodes.push(agent("a1:x", "a1"));
        doc.nodes.push(agent("root:y", "root"));
        doc.nodes.push(session("b", Some("root"), 40, 10));
        doc.nodes.push(session("c", None, 0, 30));
        doc.nodes.push(session("orphan", Some("gone"), 0, 40));
        doc
    }

    #[test]
    fn stacks_hold_descendants_with_their_subagents_and_move_with_the_parent() {
        let mut doc = family();
        assert_eq!(descendants(&doc, "root"), ["a", "a1", "b"], "document order, recursive");
        assert_eq!(stack_members(&doc, "root"), ["a", "a1", "b", "a1:x"], "the root's own subagents are not stacked");
        assert!(stacked_under(&doc).is_empty());
        doc.node_mut("root").unwrap().stacked = true;
        doc.node_mut("a").unwrap().stacked = true;
        let under = stacked_under(&doc);
        assert_eq!(under.get("a1").map(String::as_str), Some("root"), "the outer stack wins");
        assert_eq!(under.get("a1:x").map(String::as_str), Some("root"));
        assert!(!under.contains_key("root") && !under.contains_key("c") && !under.contains_key("root:y"));
        // Moving the stacked parent moves its stack (their saved positions).
        doc.move_nodes(&["root".into()], 3, 2);
        let at = |doc: &Doc, id: &str| (doc.node(id).unwrap().x, doc.node(id).unwrap().y);
        assert_eq!((at(&doc, "a"), at(&doc, "a1"), at(&doc, "b"), at(&doc, "c")), ((43, 2), (83, 2), (43, 12), (0, 30)));
        // Unstacking keeps those positions: the round trip ends where it started.
        doc.node_mut("root").unwrap().stacked = false;
        doc.move_nodes(&["root".into()], -3, -2);
        assert_eq!((at(&doc, "a"), at(&doc, "root")), ((43, 2), (0, 0)), "unstacked children stay");
        let group = doc.group_around(&["root".into()], "G").unwrap();
        doc.node_mut("root").unwrap().stacked = true;
        doc.move_group(&group, 1, 1);
        assert_eq!((at(&doc, "root"), at(&doc, "a1")), ((1, 1), (84, 3)), "a group drag carries the stack too");
    }

    #[test]
    fn hiding_subsessions_maps_each_to_its_top_ancestor() {
        let doc = family();
        let hidden = hidden_subsessions(&doc);
        assert_eq!(hidden.get("a").map(String::as_str), Some("root"));
        assert_eq!(hidden.get("a1").map(String::as_str), Some("root"));
        assert_eq!(hidden.get("a1:x").map(String::as_str), Some("root"), "subagents follow their session");
        assert_eq!(hidden.get("b").map(String::as_str), Some("root"));
        assert!(!hidden.contains_key("root") && !hidden.contains_key("root:y") && !hidden.contains_key("c"));
        assert!(!hidden.contains_key("orphan"), "a parent that is not on the canvas: a plain session");
    }
}
