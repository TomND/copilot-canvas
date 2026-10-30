//! Removing boxes from the canvas: one box (as before), or the eligible part
//! of a selection after a confirmation (right-click on a selected box, Del).
//! A box only leaves the canvas: history and conversations are never
//! deleted. Sessions running in a canvas terminal stay (stop them first);
//! subagent boxes go with their session, subsessions stay unlinked. Ctrl+Z
//! restores the last removal.

use std::collections::HashSet;

use crate::app::App;
use crate::dialogs::{Action, Confirm, Modal};
use crate::doc::{Node, NodeKind};
use crate::protocol::ClientMsg;
use crate::undo::Undo;

/// What removing a set of boxes does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemovePlan {
    /// Boxes that go.
    pub remove: Vec<String>,
    /// Sessions that stay because they run in a canvas terminal.
    pub running: Vec<String>,
}

impl App {
    /// Which of `ids` may go: everything but sessions running in a canvas
    /// terminal (the rule of a single removal).
    pub fn remove_plan(&self, ids: &[String]) -> RemovePlan {
        let mut plan = RemovePlan::default();
        for id in ids {
            let Some(node) = self.doc.node(id) else { continue };
            if plan.remove.contains(id) || plan.running.contains(id) {
                continue;
            }
            if node.kind == NodeKind::Session && self.owned_alive(id) {
                plan.running.push(id.clone());
            } else {
                plan.remove.push(id.clone());
            }
        }
        plan
    }

    /// Asks before removing the eligible boxes of `ids` (a multi-selection).
    pub fn confirm_remove(&mut self, ids: Vec<String>) {
        let plan = self.remove_plan(&ids);
        let running = plan.running.len();
        if plan.remove.is_empty() {
            return self.notify(format!("{running} running session(s) kept: stop them first (X). Nothing was removed."), true);
        }
        let count = plan.remove.len();
        let mut body = vec![format!("{count} box(es) leave the canvas; their history and conversations are kept (nothing is deleted).")];
        if running > 0 {
            body.push(format!("{running} running session{} kept: stop {} first.", if running == 1 { "" } else { "s" }, if running == 1 { "it" } else { "them" }));
        }
        body.push("Subagent boxes go with their session; subsessions stay, unlinked.".into());
        body.push("Ctrl+Z restores them.".into());
        self.modals.push(Modal::Confirm(Confirm {
            title: format!("Remove {count} from the canvas?"),
            body,
            // The whole selection: what runs by then is kept again.
            buttons: vec![(format!("Remove {count}"), Action::RemoveNodes(ids)), ("Cancel".into(), Action::Nothing)],
            sel: 0,
        }));
    }

    /// Removes the eligible boxes of `ids` (one undo restores them all).
    pub fn remove_nodes(&mut self, ids: &[String]) {
        let plan = self.remove_plan(ids);
        if plan.remove.is_empty() {
            if !plan.running.is_empty() {
                self.notify("Stop this session (X) before removing its box. History is never deleted.", true);
            }
            return;
        }
        let before = self.doc.nodes.clone();
        for id in &plan.remove {
            if self.doc.node(id).is_some_and(|n| n.kind == NodeKind::Session) {
                self.send(ClientMsg::Forget { id: id.clone() });
                self.tile_slots.retain(|t| t != id);
            }
            self.doc.remove_node(id);
        }
        let left: HashSet<&str> = self.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        let nodes: Vec<Node> = before.iter().filter(|n| !left.contains(n.id.as_str())).cloned().collect();
        let links: Vec<(String, String)> =
            before.iter().filter(|n| left.contains(n.id.as_str())).filter_map(|n| n.parent.clone().filter(|p| !left.contains(p.as_str())).map(|p| (n.id.clone(), p))).collect();
        self.selection.retain(|s| left.contains(s.as_str()));
        if let Some(crate::side::RightDetail::Session(id)) = &self.right_detail
            && !left.contains(id.as_str())
        {
            self.right_detail = None;
        }
        let removed = nodes.len();
        self.record_undo(Undo::Removed { nodes, links });
        self.mark_dirty();
        if ids.len() > 1 || !plan.running.is_empty() {
            let kept = if plan.running.is_empty() { String::new() } else { format!("; {} running session(s) kept", plan.running.len()) };
            self.notify(format!("Removed {removed} box(es) from the canvas (history kept){kept}. Ctrl+Z restores them."), false);
        }
    }

    /// Ctrl+Z after a removal: the boxes come back where they were (those
    /// another window brought back meanwhile are left alone), and so do the
    /// subsession links the removal cut.
    pub fn restore_removed(&mut self, nodes: Vec<Node>, links: Vec<(String, String)>) {
        let mut restored = 0;
        for node in nodes {
            if self.doc.node(&node.id).is_none() {
                self.doc.nodes.push(node);
                restored += 1;
            }
        }
        for (child, parent) in links {
            if self.doc.node(&parent).is_some()
                && let Some(node) = self.doc.node_mut(&child).filter(|n| n.parent.is_none())
            {
                node.parent = Some(parent);
            }
        }
        self.sync_agents();
        self.mark_dirty();
        self.notify(format!("Restored {restored} removed box(es)."), false);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::protocol::TermInfo;
    use crate::sync::Opened;

    fn node(id: &str, kind: &str, parent: Option<&str>) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": kind, "name": id, "x": 0, "y": 0, "parent": parent, "agent_key": "k" })).unwrap()
    }

    fn fixture() -> (App, Receiver<ClientMsg>) {
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let nodes = vec![node("a", "session", None), node("a:x", "agent", Some("a")), node("b", "session", None), node("c", "session", None), node("sub", "session", Some("a"))];
        let doc = Doc { nodes, ..Doc::default() };
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        app.terms.insert("b".into(), TermInfo { id: "b".into(), alive: true, ..Default::default() });
        (app, sent)
    }

    #[test]
    fn a_selection_loses_its_eligible_boxes_and_keeps_running_sessions() {
        let (mut app, sent) = fixture();
        let all: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(app.remove_plan(&all), RemovePlan { remove: vec!["a".into(), "c".into()], running: vec!["b".into()] });
        app.selection = all.clone();
        app.confirm_remove(all.clone());
        let Some(Modal::Confirm(confirm)) = app.modals.last() else { panic!("a confirmation") };
        assert_eq!(confirm.title, "Remove 2 from the canvas?");
        assert!(confirm.body.iter().any(|l| l == "1 running session kept: stop it first."), "{:?}", confirm.body);
        let Action::RemoveNodes(ids) = confirm.buttons[0].1.clone() else { panic!("remove action") };
        app.modals.clear();
        app.remove_nodes(&ids);
        let left: Vec<&str> = app.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(left, ["b", "sub"], "the subagent went with its session; the running one stays");
        assert_eq!(app.doc.node("sub").unwrap().parent, None, "the subsession stays, unlinked");
        assert_eq!(app.selection, ["b"]);
        let forgotten: Vec<String> = sent.try_iter().filter_map(|m| if let ClientMsg::Forget { id } = m { Some(id) } else { None }).collect();
        assert_eq!(forgotten, ["a", "c"], "only boxes are forgotten (history stays)");
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("1 running session(s) kept")));
        assert_eq!(app.undo_label(), "Undo remove");
        // One undo brings them all back, with the link the removal cut.
        app.undo_last();
        let mut back: Vec<&str> = app.doc.nodes.iter().map(|n| n.id.as_str()).collect();
        back.sort();
        assert_eq!(back, ["a", "a:x", "b", "c", "sub"]);
        assert_eq!(app.doc.node("sub").unwrap().parent.as_deref(), Some("a"));
        assert!(app.undo.is_empty());
    }

    #[test]
    fn only_running_sessions_selected_removes_nothing() {
        let (mut app, _sent) = fixture();
        app.confirm_remove(vec!["b".into()]);
        assert!(app.modals.is_empty() && app.notice.as_ref().is_some_and(|n| n.text.contains("stop them first")));
        app.remove_nodes(&["b".into()]);
        assert!(app.doc.node("b").is_some());
    }
}
