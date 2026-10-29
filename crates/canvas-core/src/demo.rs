//! The `--demo` canvas: native PowerShell sessions, no agent calls.

use crate::doc::{Doc, Group, Node, NodeKind};

/// A group with two sessions, a subagent and a subsession; every session runs
/// PowerShell in `cwd`.
pub fn demo_doc(cwd: &str) -> Doc {
    let shell = Some(vec!["powershell.exe".to_owned(), "-NoLogo".to_owned(), "-NoProfile".to_owned()]);
    let session = |id: &str, name: &str, x: i32, y: i32| Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x,
        y,
        cwd: Some(cwd.to_owned()),
        parent: None,
        agent_key: None,
        source: Some("demo".into()),
        command: shell.clone(),
        fresh: true,
        automation: None,
        synced_name: None,
        reviewed_ms: None,
        mcp_off: None,
        passive: None,
        no_messages: false,
        agent: None,
        agent_session: None, last_used_ms: None, stacked: false, model: None,
    };
    let mut doc = Doc::default();
    doc.nodes.push(session("demo-main", "Build session canvas", 2, 2));
    doc.nodes.push(Node {
        kind: NodeKind::Agent,
        parent: Some("demo-main".into()),
        agent_key: Some("explore".into()),
        command: None,
        ..session("demo-main:explore", "Explore terminal APIs", 6, 7)
    });
    doc.nodes.push(session("demo-review", "Review implementation", 36, 2));
    let mut sub = session("demo-sub", "Write tests", 36, 9);
    sub.parent = Some("demo-review".into());
    doc.nodes.push(sub);
    doc.groups.push(Group { id: "g-demo".into(), name: "Canvas project".into(), x: 0, y: 0, w: 70, h: 15, color: 0, folder: None, source: None, defaults: None });
    doc.view.x = -2;
    doc.view.y = -1;
    doc.selected = Some("demo-main".into());
    doc
}
