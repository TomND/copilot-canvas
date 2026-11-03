//! Canvas visuals that are part of the document: dust on unused sessions
//! (`Node::last_used_ms`, `Doc::dust_after_days`) and decks beneath sessions:
//! stacks of subsessions (`Node::stacked`) and folded subagent boxes
//! (`Node::subagents_shown`, `Doc::subagents_shown`; folded by default); and
//! which boxes a window's hide switches hide (`hidden_boxes`).
//! Pure functions shared by the clients and the service.

use std::collections::{HashMap, HashSet};

use crate::doc::{AGENT_H, AGENT_W, Doc, Node, NodeKind, Rect, SESSION_H, SESSION_W};

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

/// Which deck hides each box: node id → the session the deck lies beneath.
/// Stacks hold a stacked session's subsessions with their subagent boxes
/// (the stacked ancestor highest up the parent chain wins); a session's
/// folded subagent boxes go into its own deck, or into the stack that holds
/// the session. Boxes in the hidden area ([`hidden_area`]) are in no deck.
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
    let sessions: HashMap<&str, &Node> = doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).map(|n| (n.id.as_str(), n)).collect();
    for agent in doc.nodes.iter().filter(|n| n.kind == NodeKind::Agent) {
        if out.contains_key(&agent.id) {
            continue;
        }
        let Some(parent) = agent.parent.as_deref().and_then(|p| sessions.get(p)) else { continue };
        if subagents_folded(doc, parent) {
            let root = out.get(&parent.id).cloned().unwrap_or_else(|| parent.id.clone());
            out.insert(agent.id.clone(), root);
        }
    }
    if doc.nodes.iter().any(|n| n.hidden) {
        let hidden = hidden_area(doc);
        out.retain(|id, _| !hidden.contains_key(id));
    }
    out
}

/// A session's subagent boxes are folded into its deck: its own choice
/// (`Node::subagents_shown`), else the canvas's (`Doc::subagents_shown`,
/// folded by default).
pub fn subagents_folded(doc: &Doc, session: &Node) -> bool {
    session.kind == NodeKind::Session && !session.subagents_shown.unwrap_or(doc.subagents_shown)
}

/// A session's subagent boxes, in document order.
pub fn subagents_of(doc: &Doc, session: &str) -> Vec<String> {
    doc.nodes.iter().filter(|n| n.kind == NodeKind::Agent && n.parent.as_deref() == Some(session)).map(|n| n.id.clone()).collect()
}

/// What a session's deck holds (the document's state, no transitions).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeckContents {
    /// The session it lies beneath.
    pub root: String,
    /// Its stacked subsessions (recursively), in document order.
    pub sessions: Vec<String>,
    /// Its own folded subagent boxes, in document order.
    pub agents: Vec<String>,
    /// Every box it holds (the subsessions' subagent boxes too).
    pub members: Vec<String>,
}

impl DeckContents {
    /// Boxes it shows as cards: stacked sessions and folded subagent boxes.
    pub fn cards(&self) -> usize {
        self.sessions.len() + self.agents.len()
    }
}

/// The decks `under` (from [`stacked_under`]) describes, in the document
/// order of their sessions.
pub fn decks_of(doc: &Doc, under: &HashMap<String, String>) -> Vec<DeckContents> {
    let mut by_root: HashMap<&str, DeckContents> = HashMap::new();
    for node in &doc.nodes {
        let Some(root) = under.get(&node.id) else { continue };
        let deck = by_root.entry(root.as_str()).or_insert_with(|| DeckContents { root: root.clone(), ..Default::default() });
        deck.members.push(node.id.clone());
        match node.kind {
            NodeKind::Session => deck.sessions.push(node.id.clone()),
            NodeKind::Agent if node.parent.as_deref() == Some(root.as_str()) => deck.agents.push(node.id.clone()),
            _ => {}
        }
    }
    doc.nodes.iter().filter_map(|n| by_root.remove(n.id.as_str())).collect()
}

/// Every deck of the canvas.
pub fn decks(doc: &Doc) -> Vec<DeckContents> {
    decks_of(doc, &stacked_under(doc))
}

/// Where a session's deck is drawn (its top card, world units). Holding
/// subsessions: a session-sized card beneath the session and its shown
/// subagent boxes. Holding subagent boxes alone: a slim subagent-sized card
/// where the first subagent box would go (`Doc::agent_slot_in`).
pub fn deck_rect(doc: &Doc, root: &Node, sessions: bool) -> Rect {
    if !sessions {
        return Rect { x: root.x + 4, y: root.y + SESSION_H + 1, w: AGENT_W, h: AGENT_H };
    }
    let mut bottom = root.y + SESSION_H;
    if !subagents_folded(doc, root) {
        for agent in doc.nodes.iter().filter(|n| n.kind == NodeKind::Agent && !n.unplaced && !n.hidden && n.parent.as_deref() == Some(root.id.as_str())) {
            let below = agent.y >= root.y && agent.x < root.x + SESSION_W && agent.x + AGENT_W > root.x;
            if below {
                bottom = bottom.max(agent.y + AGENT_H);
            }
        }
    }
    Rect { x: root.x + 2, y: bottom + 1, w: SESSION_W, h: SESSION_H }
}

/// Cards drawn behind a deck's top card (0 to 2) for `cards` boxes held.
pub fn deck_back(cards: usize) -> i32 {
    cards.saturating_sub(1).min(2) as i32
}

/// A deck's top card with the cards behind it (they peek out right and below).
pub fn deck_bounds(doc: &Doc, deck: &DeckContents) -> Option<Rect> {
    let root = doc.node(&deck.root)?;
    let r = deck_rect(doc, root, !deck.sessions.is_empty());
    let back = deck_back(deck.cards());
    Some(Rect { w: r.w + back, h: r.h + back, ..r })
}

/// The rectangles the canvas's layout takes, each with the box (or the
/// session of the deck) it belongs to: boxes drawn on their own and decks
/// with their back cards. Folded, stacked, unplaced and hidden-area boxes
/// take none.
pub fn layout(doc: &Doc) -> Vec<(&str, Rect)> {
    let under = stacked_under(doc);
    let hidden = hidden_area(doc);
    let by_id: HashMap<&str, &Node> = doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out: Vec<(&str, Rect)> = doc.nodes.iter().filter(|n| !under.contains_key(&n.id) && !n.unplaced && !hidden.contains_key(&n.id)).map(|n| (n.id.as_str(), n.rect())).collect();
    for deck in decks_of(doc, &under) {
        let Some(root) = by_id.get(deck.root.as_str()) else { continue };
        let r = deck_rect(doc, root, !deck.sessions.is_empty());
        let back = deck_back(deck.cards());
        out.push((root.id.as_str(), Rect { w: r.w + back, h: r.h + back, ..r }));
    }
    out
}

/// [`layout`]'s rectangles alone.
pub fn layout_rects(doc: &Doc) -> Vec<Rect> {
    layout(doc).into_iter().map(|(_, r)| r).collect()
}

/// The box a hidden box shows on (its nearest ancestor that is not hidden),
/// or `None` when `node` is not hidden. A subsession or subagent box hides
/// on its own (`Node::hidden`, the hidden area), `subsessions` hides every
/// subsession (`H`) and `subagents` every subagent box (`J`); everything
/// under a hidden subsession hides with it. `find` looks boxes up by id.
pub fn hidden_into<'a>(node: &Node, find: impl Fn(&str) -> Option<&'a Node>, subsessions: bool, subagents: bool) -> Option<&'a Node> {
    let hides = |n: &Node| match n.kind {
        NodeKind::Agent => Some(n.hidden || subagents),
        NodeKind::Session if n.passive.is_none() => Some(n.hidden || subsessions),
        _ => None,
    };
    let parent_of = |n: &Node| n.parent.as_deref().filter(|p| *p != n.id).and_then(&find);
    let mut visible = None;
    let (mut hidden, mut up) = (hides(node), parent_of(node));
    // The highest hidden box of the parent chain decides (a top-level session never hides).
    for _ in 0..64 {
        let (Some(hidden_here), Some(parent)) = (hidden, up) else { break };
        if hidden_here {
            visible = Some(parent);
        }
        hidden = hides(parent);
        up = parent_of(parent);
    }
    visible
}

/// Every hidden box (see [`hidden_into`]), mapped to the box whose marker
/// counts it.
pub fn hidden_boxes(doc: &Doc, subsessions: bool, subagents: bool) -> HashMap<String, String> {
    if !subsessions && !subagents && !doc.nodes.iter().any(|n| n.hidden) {
        return HashMap::new();
    }
    let by_id: HashMap<&str, &Node> = doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    doc.nodes.iter().filter_map(|n| hidden_into(n, |id| by_id.get(id).copied(), subsessions, subagents).map(|top| (n.id.clone(), top.id.clone()))).collect()
}

/// The canvas's hidden area: boxes hidden on their own and everything under
/// them (the same for every window), mapped as [`hidden_boxes`] does.
pub fn hidden_area(doc: &Doc) -> HashMap<String, String> {
    hidden_boxes(doc, false, false)
}

/// A box that can be hidden on its own: a subsession or a subagent box
/// (with its parent on the canvas).
pub fn can_hide(doc: &Doc, node: &Node) -> bool {
    let parent = node.parent.as_deref().filter(|p| *p != node.id).and_then(|p| doc.node(p));
    match node.kind {
        NodeKind::Agent => parent.is_some(),
        NodeKind::Session => node.passive.is_none() && parent.is_some(),
        NodeKind::Automation => false,
    }
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
        // Subagent boxes are shown here (the stacks are what these tests are about).
        let mut doc = Doc { subagents_shown: true, ..Doc::default() };
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
    fn hiding_maps_each_box_to_its_nearest_visible_ancestor_by_switch() {
        let doc = family();
        let get = |hidden: &HashMap<String, String>, id: &str| hidden.get(id).cloned();
        let some = |id: &str| Some(id.to_owned());
        // Both switches: every subsession and subagent box.
        let hidden = hidden_boxes(&doc, true, true);
        assert_eq!((get(&hidden, "a"), get(&hidden, "a1"), get(&hidden, "b")), (some("root"), some("root"), some("root")));
        assert_eq!(get(&hidden, "a1:x"), some("root"), "subagents follow their session");
        assert_eq!(get(&hidden, "root:y"), some("root"), "a top-level session's subagent boxes hide into it");
        assert!(!hidden.contains_key("root") && !hidden.contains_key("c"));
        assert!(!hidden.contains_key("orphan"), "a parent that is not on the canvas: a plain session");
        // H: subsessions and what is under them; a top-level session's subagent boxes stay.
        let h = hidden_boxes(&doc, true, false);
        assert_eq!((get(&h, "a1"), get(&h, "a1:x"), get(&h, "root:y")), (some("root"), some("root"), None));
        // J: every subagent box, into its own session; subsessions stay.
        let j = hidden_boxes(&doc, false, true);
        assert_eq!((get(&j, "a1:x"), get(&j, "root:y"), get(&j, "a"), get(&j, "a1")), (some("a1"), some("root"), None, None));
        assert!(hidden_boxes(&doc, false, false).is_empty());
        // One box at a time, with any lookup.
        let a1x = doc.node("a1:x").unwrap();
        assert_eq!(hidden_into(a1x, |id| doc.node(id), false, true).map(|n| n.id.as_str()), Some("a1"));
        assert_eq!(hidden_into(doc.node("root").unwrap(), |id| doc.node(id), true, true), None);
    }

    #[test]
    fn boxes_hidden_on_their_own_take_their_subtree_and_move_with_their_parent() {
        let mut doc = family();
        let at = |doc: &Doc, id: &str| (doc.node(id).unwrap().x, doc.node(id).unwrap().y);
        // Only subsessions and subagent boxes can hide on their own.
        assert!(can_hide(&doc, doc.node("a").unwrap()) && can_hide(&doc, doc.node("root:y").unwrap()));
        assert!(!can_hide(&doc, doc.node("root").unwrap()) && !can_hide(&doc, doc.node("orphan").unwrap()));
        doc.node_mut("root").unwrap().hidden = true;
        assert!(hidden_area(&doc).is_empty(), "a top-level session never hides");
        doc.node_mut("root").unwrap().hidden = false;
        // A hidden subsession takes what is under it; each counts on the nearest visible box.
        doc.node_mut("a").unwrap().hidden = true;
        doc.node_mut("root:y").unwrap().hidden = true;
        let area = hidden_area(&doc);
        let top = |id: &str| area.get(id).map(String::as_str);
        assert_eq!((top("a"), top("a1"), top("a1:x"), top("root:y")), (Some("root"), Some("root"), Some("root"), Some("root")));
        assert_eq!((top("b"), top("c"), top("root")), (None, None, None));
        assert_eq!(hidden_boxes(&doc, false, false), area, "the same for every window");
        // They take no room, and they move with the box they count on.
        assert!(!layout(&doc).iter().any(|(id, _)| ["a", "a1", "a1:x", "root:y"].contains(id)));
        doc.move_nodes(&["root".into()], 2, 1);
        assert_eq!((at(&doc, "a"), at(&doc, "a1"), at(&doc, "b"), at(&doc, "c")), ((42, 1), (82, 1), (40, 10), (0, 30)), "the hidden ones follow, a shown subsession stays");
        // They belong to the parent's group wherever their own place is.
        doc.node_mut("a1").unwrap().x = 500;
        let group = doc.group_around(&["root".into()], "G").unwrap();
        assert_eq!(doc.group_of(doc.node("a1").unwrap()).map(|g| g.id.clone()), Some(group.clone()));
        let g = doc.group(&group).unwrap().clone();
        assert!(g.w < 60, "a group around the parent leaves hidden boxes out: {g:?}");
        let before = at(&doc, "a1");
        doc.move_group(&group, 1, 1);
        assert_eq!(at(&doc, "a1"), (before.0 + 1, before.1 + 1), "a group drag carries them too");
        // A folded subagent box hidden on its own leaves its deck; the last one takes the deck along.
        let mut doc = family();
        doc.subagents_shown = false;
        doc.nodes.push(agent("root:z", "root"));
        assert_eq!(decks(&doc).iter().find(|d| d.root == "root").map(|d| d.agents.len()), Some(2));
        doc.node_mut("root:y").unwrap().hidden = true;
        assert_eq!(decks(&doc).iter().find(|d| d.root == "root").map(|d| d.agents.clone()), Some(vec!["root:z".to_owned()]));
        doc.node_mut("root:z").unwrap().hidden = true;
        assert!(!decks(&doc).iter().any(|d| d.root == "root") && !stacked_under(&doc).contains_key("root:y"));
        // The document keeps the choice; canvases from before have none, and an unset one is not written.
        let back: Doc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert!(back.node("root:y").unwrap().hidden && !back.node("root").unwrap().hidden);
        let old: Node = serde_json::from_value(serde_json::json!({ "id": "x", "kind": "agent", "name": "X", "x": 0, "y": 0, "parent": "root" })).unwrap();
        assert!(!old.hidden && !serde_json::to_string(&old).unwrap().contains("hidden"));
        assert!(serde_json::to_string(back.node("root:y").unwrap()).unwrap().contains("\"hidden\":true"));
    }

    #[test]
    fn old_canvases_fold_subagents_and_the_choices_round_trip() {
        let text = r#"{"version":2,"nodes":[{"id":"s","kind":"session","name":"S","x":0,"y":0},{"id":"s:a","kind":"agent","name":"A","x":4,"y":5,"parent":"s","agent_key":"a"}],"groups":[],"view":{"x":0,"y":0}}"#;
        let mut doc: Doc = serde_json::from_str(text).unwrap();
        assert!(subagents_folded(&doc, &doc.nodes[0]), "a canvas from before: folded");
        assert_eq!(stacked_under(&doc).get("s:a").map(String::as_str), Some("s"));
        assert_eq!((doc.nodes[1].x, doc.nodes[1].y, doc.nodes[1].unplaced), (4, 5, false), "its saved place stays");
        let saved = serde_json::to_string(&doc).unwrap();
        assert!(!saved.contains("subagents_shown") && !saved.contains("unplaced"), "unset fields are not written: {saved}");
        // The canvas default, a session's own choice and an unplaced box are saved and read back.
        doc.subagents_shown = true;
        assert!(!subagents_folded(&doc, &doc.nodes[0]), "Settings › Subagents: shown");
        doc.nodes[0].subagents_shown = Some(false);
        assert!(subagents_folded(&doc, &doc.nodes[0]), "the session's own choice wins");
        doc.nodes[1].unplaced = true;
        let back: Doc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back, doc);
        // A shared field: other windows and clients get it as a patch.
        let mut other = Doc::default();
        crate::sync::set_field(&mut other, "subagents_shown", crate::sync::field(&doc, "subagents_shown").unwrap()).unwrap();
        assert!(other.subagents_shown);
        // Not a session: never folded; an orphaned subagent box shows itself.
        assert!(!subagents_folded(&doc, &doc.nodes[1]));
        doc.nodes.remove(0);
        assert!(stacked_under(&doc).is_empty());
    }

    fn folded() -> Doc {
        // s (2,2) with subagent boxes in a tall column far below, in group g
        // that grew for them; t is another session; u a subsession of t.
        let mut doc = Doc::default();
        doc.nodes.push(session("s", None, 2, 2));
        for i in 0..5 {
            let mut a = agent(&format!("s:{i}"), "s");
            (a.x, a.y) = (6, 7 + i * 4);
            doc.nodes.push(a);
        }
        doc.nodes.push(session("t", None, 40, 2));
        doc.nodes.push(session("u", Some("t"), 40, 9));
        doc.groups.push(crate::doc::Group { id: "g".into(), name: "G".into(), x: 0, y: 0, w: 76, h: 30, ..Default::default() });
        doc
    }

    #[test]
    fn decks_hold_folded_subagents_and_take_the_room_of_their_card() {
        let mut doc = folded();
        let held = decks(&doc);
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].root.as_str(), held[0].agents.len(), held[0].sessions.len(), held[0].cards()), ("s", 5, 0, 5));
        let card = deck_rect(&doc, doc.node("s").unwrap(), false);
        assert_eq!(card, Rect { x: 6, y: 7, w: AGENT_W, h: AGENT_H }, "a slim card where the first subagent box goes");
        assert_eq!(deck_bounds(&doc, &held[0]), Some(Rect { x: 6, y: 7, w: AGENT_W + 2, h: AGENT_H + 2 }), "two cards peek out behind it");
        // Folded boxes take no room: only the deck does.
        let rects = layout_rects(&doc);
        assert_eq!(rects.len(), 4, "s, t, u and the deck: {rects:?}");
        assert!(doc.is_free(&Rect { x: 6, y: 20, w: 4, h: 2 }, ""), "a folded box's saved place is free");
        assert!(!doc.is_free(&Rect { x: 8, y: 8, w: 2, h: 1 }, ""), "its deck is not");
        assert_eq!(doc.bounds(), Some(Rect { x: 0, y: 0, w: 76, h: 30 }), "the group's frame bounds it");
        // Fit to members: around what shows (the group grew for the column).
        doc.fit_group("g");
        let g = doc.group("g").unwrap().clone();
        assert_eq!((g.x, g.y, g.w, g.h), (0, 0, 72, 14), "shrunk to s, its deck, t and u");
        // Membership: folded boxes belong with their session, even outside the group now.
        let below = doc.node("s:4").unwrap();
        assert!(below.y + 1 > g.y + g.h, "the last box's saved place is outside the fitted group");
        assert_eq!(doc.group_of(below).map(|g| g.id.as_str()), Some("g"));
        let all = doc.groups_by_node();
        assert!(doc.nodes.iter().all(|n| all.get(n.id.as_str()).map(|g| &g.id) == doc.group_of(n).map(|g| &g.id)), "one pass agrees with group_of");
        assert_eq!(doc.members("g").len(), 8);
        // A group drag carries them with their session.
        doc.move_group("g", 1, 1);
        assert_eq!((doc.node("s").unwrap().x, doc.node("s:4").unwrap().y), (3, 24));
        // Shown: they are on their own again, at their saved places.
        doc.node_mut("s").unwrap().subagents_shown = Some(true);
        assert!(decks(&doc).is_empty() && stacked_under(&doc).is_empty());
        assert!(!doc.is_free(&Rect { x: 7, y: 24, w: 2, h: 1 }, ""));
        assert_eq!(doc.group_of(doc.node("s:4").unwrap()), None, "shown outside the group: geometric again");
    }

    #[test]
    fn one_deck_holds_a_stack_and_the_folded_subagents_of_its_session() {
        let mut doc = family();
        doc.subagents_shown = false;
        doc.node_mut("root").unwrap().stacked = true;
        let under = stacked_under(&doc);
        assert_eq!(under.get("root:y").map(String::as_str), Some("root"), "the root's own subagent box");
        assert_eq!(under.get("a1:x").map(String::as_str), Some("root"), "a stacked subsession's subagent box");
        let held = decks(&doc);
        assert_eq!(held.len(), 1, "one deck per session");
        assert_eq!((held[0].sessions.clone(), held[0].agents.clone()), (vec!["a".to_owned(), "a1".to_owned(), "b".to_owned()], vec!["root:y".to_owned()]));
        assert_eq!(held[0].members.len(), 5);
        let root = doc.node("root").unwrap().clone();
        assert_eq!(deck_rect(&doc, &root, true), Rect { x: 2, y: 5, w: SESSION_W, h: SESSION_H }, "a stack's card, right beneath its session");
        // Unstacked, the subsessions fold their own subagent boxes.
        doc.node_mut("root").unwrap().stacked = false;
        let under = stacked_under(&doc);
        assert_eq!((under.get("a1:x").map(String::as_str), under.get("root:y").map(String::as_str)), (Some("a1"), Some("root")));
        assert_eq!(decks(&doc).len(), 2);
    }
}
