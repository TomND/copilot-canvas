//! Recently used sessions: the session boxes you viewed, started or typed
//! into, newest first. Each box records when (`Node::accessed_ms`, a quiet
//! field of the shared document), so every window and client of a canvas
//! sees the same list, two windows recording at once both keep their
//! stamps, and a removed box leaves the list with it. A [`Walk`] steps
//! through the list without reordering it, as Alt+Tab does.

use crate::doc::{Doc, Node, NodeKind};

/// Most rows the Sessions panel's RECENT section shows (and a walk visits).
pub const SHOWN: usize = 8;

/// The session boxes by most recent use (newest first; ties by id).
pub fn order(doc: &Doc) -> Vec<&Node> {
    let mut nodes: Vec<&Node> = doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && n.accessed_ms.is_some()).collect();
    nodes.sort_by(|a, b| b.accessed_ms.cmp(&a.accessed_ms).then_with(|| a.id.cmp(&b.id)));
    nodes
}

/// The newest stamp of any other box.
fn newest_other(doc: &Doc, id: &str) -> u64 {
    doc.nodes.iter().filter(|n| n.id != id).filter_map(|n| n.accessed_ms).max().unwrap_or(0)
}

/// Records a use of session box `id` at `now_ms`, ahead of every other box
/// (also when another machine's clock ran ahead). False when nothing
/// changed: not a session box, or already the newest and recorded less than
/// `again_ms` ago (typing does not rewrite the document on every key).
pub fn touch(doc: &mut Doc, id: &str, now_ms: u64, again_ms: u64) -> bool {
    let newest = newest_other(doc, id);
    let Some(node) = doc.node_mut(id).filter(|n| n.kind == NodeKind::Session) else { return false };
    if node.accessed_ms.is_some_and(|at| at > newest && now_ms.saturating_sub(at) < again_ms) {
        return false;
    }
    node.accessed_ms = Some(now_ms.max(newest + 1));
    true
}

/// A walk through the recent sessions: each step views the next older (or
/// newer) one, while the order stays as it was when the walk began; the
/// session it ends on becomes the most recent afterwards.
#[derive(Clone, Debug, PartialEq)]
pub struct Walk {
    /// Session ids, newest first, as when the walk began.
    pub order: Vec<String>,
    /// The step reached (`None`: where the walk began, when that is not in the list).
    pub at: Option<usize>,
    /// Some step moved it.
    pub moved: bool,
}

impl Walk {
    /// A walk over `order` from the session shown now.
    pub fn new(order: Vec<String>, current: Option<&str>) -> Self {
        let at = current.and_then(|c| order.iter().position(|o| o == c));
        Self { order, at, moved: false }
    }

    /// One step (`older`: the session used before, else the one after) and
    /// the session to view; `None` when the list is empty.
    pub fn step(&mut self, older: bool) -> Option<&str> {
        let n = self.order.len();
        if n == 0 {
            return None;
        }
        let next = match (self.at, older) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        self.at = Some(next);
        self.moved = true;
        Some(&self.order[next])
    }

    /// The session the walk is on.
    pub fn current(&self) -> Option<&str> {
        self.at.and_then(|i| self.order.get(i)).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{Op, apply_all, diff};

    fn session(id: &str) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0 })).unwrap()
    }

    fn doc(ids: &[&str]) -> Doc {
        Doc { nodes: ids.iter().map(|id| session(id)).collect(), ..Doc::default() }
    }

    fn ids(doc: &Doc) -> Vec<&str> {
        order(doc).into_iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn the_newest_use_comes_first() {
        let mut d = doc(&["a", "b", "c"]);
        assert!(ids(&d).is_empty(), "nothing used yet");
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            assert!(touch(&mut d, id, 1_000 + i as u64, 60_000));
        }
        assert_eq!(ids(&d), ["c", "b", "a"]);
        assert!(touch(&mut d, "a", 2_000, 60_000));
        assert_eq!(ids(&d), ["a", "c", "b"]);
        // Typing into the newest one does not rewrite it for a minute.
        assert!(!touch(&mut d, "a", 2_500, 60_000));
        assert!(touch(&mut d, "a", 70_000, 60_000));
        // Another machine's clock ran ahead: a use here still goes first.
        d.node_mut("b").unwrap().accessed_ms = Some(9_000_000);
        assert!(touch(&mut d, "c", 80_000, 60_000));
        assert_eq!(ids(&d), ["c", "b", "a"]);
        assert_eq!(d.node("c").unwrap().accessed_ms, Some(9_000_001));
        // Subagents and automations are not sessions of the list.
        let mut agent = session("c:x");
        agent.kind = NodeKind::Agent;
        d.nodes.push(agent);
        assert!(!touch(&mut d, "c:x", 90_000, 60_000) && !touch(&mut d, "missing", 90_000, 60_000));
        // A removed box leaves the list.
        d.remove_node("b");
        assert_eq!(ids(&d), ["c", "a"]);
    }

    #[test]
    fn a_walk_steps_through_without_reordering() {
        let order: Vec<String> = ["c", "b", "a"].iter().map(|s| s.to_string()).collect();
        let mut walk = Walk::new(order.clone(), Some("c"));
        assert_eq!(walk.step(true), Some("b"));
        assert_eq!(walk.step(true), Some("a"));
        assert_eq!(walk.step(true), Some("c"), "round to the newest");
        assert_eq!(walk.step(false), Some("a"), "back again");
        assert_eq!((walk.order.clone(), walk.current(), walk.moved), (order.clone(), Some("a"), true));
        // From a session that is not in the list: the newest first (or, going back, the oldest).
        assert_eq!(Walk::new(order.clone(), Some("x")).step(true), Some("c"));
        assert_eq!(Walk::new(order.clone(), None).step(false), Some("a"));
        assert_eq!(Walk::new(Vec::new(), None).step(true), None);
        assert_eq!(Walk::new(order, Some("c")).current(), Some("c"));
    }

    #[test]
    fn uses_travel_with_the_document_and_merge_between_windows() {
        let base = doc(&["a", "b"]);
        // Saved and loaded.
        let mut d = base.clone();
        touch(&mut d, "a", 5_000, 60_000);
        let saved: Doc = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(saved.node("a").unwrap().accessed_ms, Some(5_000));
        assert!(!serde_json::to_string(&base).unwrap().contains("accessed_ms"), "nothing written for unused boxes");
        // Two windows record at once: each edit carries its own box's field only …
        let (mut one, mut two) = (base.clone(), base.clone());
        touch(&mut one, "a", 6_000, 60_000);
        touch(&mut two, "b", 6_001, 60_000);
        let (edit_one, edit_two) = (diff(&base, &one), diff(&base, &two));
        assert!(matches!(&edit_one[..], [Op::NodeFields(n, f)] if n.id == "a" && *f == ["accessed_ms"]), "{edit_one:?}");
        // … so the service and both windows end with both stamps.
        let mut service = base.clone();
        apply_all(&mut service, &edit_one).unwrap();
        apply_all(&mut service, &edit_two).unwrap();
        apply_all(&mut one, &edit_two).unwrap();
        apply_all(&mut two, &edit_one).unwrap();
        for d in [&service, &one, &two] {
            assert_eq!(ids(d), ["b", "a"]);
        }
    }
}
