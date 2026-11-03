//! The parity suite's checks of hiding subsessions and subagent boxes (row
//! 53: `H` and `J` each on its own), on a canvas of its own: a parent
//! session with a subagent box and a subsession that has one too.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check};
use crate::client::doc::{Doc, Group, Node, NodeKind};
use crate::selftest::{Ui, load};

fn session(id: &str, name: &str, x: i32, y: i32, cwd: &Path) -> Node {
    let mut node: Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y })).expect("a node");
    node.cwd = Some(cwd.display().to_string());
    node.reviewed_ms = Some(crate::paths::now_ms());
    node
}

fn agent(id: &str, name: &str, parent: &str, x: i32, y: i32, cwd: &Path) -> Node {
    let mut node = session(id, name, x, y, cwd);
    (node.kind, node.parent, node.agent_key) = (NodeKind::Agent, Some(parent.into()), id.split(':').nth(1).map(str::to_owned));
    node
}

/// The hiding canvas (world units): "Hide group" 0,0 66×13 holds Hide
/// parent (its subagent box Hide helper shown) and its subsession Hide
/// child (with Child helper shown).
fn canvas(home: &Path, work: &Path) -> Result<PathBuf> {
    let mut doc = Doc::default();
    let mut parent = session("h-parent", "Hide parent", 2, 2, work);
    parent.subagents_shown = Some(true);
    doc.nodes.push(parent);
    doc.nodes.push(agent("h-parent:helper", "Hide helper", "h-parent", 6, 7, work));
    let mut child = session("h-child", "Hide child", 36, 2, work);
    (child.parent, child.subagents_shown) = (Some("h-parent".into()), Some(true));
    doc.nodes.push(child);
    doc.nodes.push(agent("h-child:x", "Child helper", "h-child", 40, 7, work));
    doc.groups.push(Group { id: "g-hide".into(), name: "Hide group".into(), x: 0, y: 0, w: 66, h: 13, color: 5, ..Default::default() });
    doc.view.x = -1;
    doc.view.y = -1;
    // A narrow details panel: everything stays in view.
    doc.ui.terminal_percent = 30;
    let path = home.join("hide.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

fn boxes(tree: &Value) -> Vec<Value> {
    tree["canvas"]["boxes"].as_array().cloned().unwrap_or_default()
}

fn named(tree: &Value, name: &str) -> Option<Value> {
    boxes(tree).into_iter().find(|b| b["name"] == name)
}

/// Drawn on its own, fully (not part way through a transition).
fn drawn(tree: &Value, name: &str) -> bool {
    named(tree, name).is_some_and(|b| b["alpha"] == 1.0)
}

/// A box's hidden marker: (all it counts, subagent boxes among them).
fn marker(tree: &Value, name: &str) -> (u64, u64) {
    named(tree, name).map(|b| (b["hidden"].as_u64().unwrap_or(0), b["hidden_agents"].as_u64().unwrap_or(0))).unwrap_or_default()
}

/// The places of this canvas's boxes (the sandbox's automations add boxes of their own).
fn places(doc: &Doc) -> Vec<(String, i32, i32)> {
    let mut out: Vec<(String, i32, i32)> = doc.nodes.iter().filter(|n| n.id.starts_with("h-")).map(|n| (n.id.clone(), n.x, n.y)).collect();
    out.sort();
    out
}

fn launches(sb: &Sandbox) -> usize {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count()
}

/// The toolbar's row of the screen.
fn toolbar(gui: &Gui) -> String {
    gui.screen().lines().next().unwrap_or_default().to_owned()
}

/// Waits until this window's preferences file holds `needle`.
fn prefs_hold(home: &Path, needle: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = std::fs::read_dir(home.join("prefs")).into_iter().flatten().flatten().any(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains(needle)));
        if found {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no preferences file in {} holds {needle}", home.join("prefs").display());
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    let path = match canvas(&sb.instance.home, &sb.work) {
        Ok(path) => path,
        Err(error) => return check(rows, "53", "Hiding", "hide_canvas", || Err(error)),
    };
    let arg = path.display().to_string();
    let mut gui = match sb.start(&["--canvas", &arg], "gui-hide.log") {
        Ok(gui) => gui,
        Err(error) => return check(rows, "53", "Hiding", "hide_canvas", || Err(error)),
    };
    if let Err(error) = gui.wait("the hiding canvas", 25, |t| t.contains("Hide group")) {
        return check(rows, "53", "Hiding", "hide_canvas", || Err(error));
    }

    check(rows, "53", "New (WI-024): H hides subsessions (with their subagent boxes) and J every subagent box, each on its own, with a toolbar counter each; markers count by kind; places kept; nothing starts", "hide_switches_each_kind", || {
        let before = launches(sb);
        gui.canvas_focus()?;
        gui.key("Esc", &[])?;
        let start = places(&load(&path)?);
        gui.wait_tree("every box drawn", 10, |t| ["Hide parent", "Hide helper", "Hide child", "Child helper"].iter().all(|n| drawn(t, n)))?;
        // J: every subagent box; the subsession stays, each session counts its own.
        gui.send("j")?;
        gui.wait_tree("J hid the subagent boxes", 5, |t| !drawn(t, "Hide helper") && !drawn(t, "Child helper") && drawn(t, "Hide child") && marker(t, "Hide parent") == (1, 1) && marker(t, "Hide child") == (1, 1))?;
        gui.wait("the subagent counter alone", 5, |_| toolbar(&gui).contains(" ◇ 2 hidden ") && !toolbar(&gui).contains('↳'))?;
        prefs_hold(&sb.instance.home, "\"hide_subagents\": true")?;
        // H as well: the subsession with its subagent box, counted on the parent.
        gui.send("h")?;
        gui.wait_tree("H hid the subsession", 5, |t| !drawn(t, "Hide child") && marker(t, "Hide parent") == (3, 2))?;
        gui.wait("both counters and the parent's marker", 5, |t| toolbar(&gui).contains(" ↳ 1 hidden ") && toolbar(&gui).contains(" ◇ 2 hidden ") && t.contains("⋯ 1 · ◇ 2 hidden"))?;
        let _ = gui.png(0, "hide-both");
        // J off: the parent's subagent box is back; the subsession's stays hidden with it.
        gui.send("j")?;
        gui.wait_tree("J showed the parent's subagent box", 5, |t| drawn(t, "Hide helper") && !drawn(t, "Child helper") && !drawn(t, "Hide child") && marker(t, "Hide parent") == (2, 1))?;
        gui.wait("the subsession counter alone", 5, |_| toolbar(&gui).contains(" ↳ 1 hidden ") && !toolbar(&gui).contains('◇'))?;
        // H off: everything back at its place.
        gui.send("h")?;
        gui.wait_tree("everything shown again", 5, |t| ["Hide parent", "Hide helper", "Hide child", "Child helper"].iter().all(|n| drawn(t, n)) && marker(t, "Hide parent") == (0, 0))?;
        gui.wait("no counters", 5, |_| !toolbar(&gui).contains(" hidden "))?;
        anyhow::ensure!(places(&load(&path)?) == start, "hiding moved a box");
        std::thread::sleep(Duration::from_millis(300));
        anyhow::ensure!(launches(sb) == before, "hiding started a session");
        Ok("J: 2 subagent boxes (◇ 2 hidden), the subsession drawn; H too: ⋯ 1 · ◇ 2 hidden on the parent, ↳ 1 and ◇ 2 in the toolbar; J off: the parent's subagent box back, the subsession's still with it; H off: all in place; the switch in the window's preferences; nothing started".into())
    });

    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hiding_canvas_has_a_subsession_and_subagent_boxes_shown() {
        let dir = std::env::temp_dir().join(format!("cc-parity-hide-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = load(&canvas(&dir, &dir).unwrap()).unwrap();
        let ids: std::collections::HashSet<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(canvas_model::visual::is_subsession(&ids, doc.node("h-child").unwrap()));
        assert!(canvas_model::visual::stacked_under(&doc).is_empty(), "every subagent box is shown on its own");
        assert_eq!(doc.group_of(doc.node("h-child:x").unwrap()).map(|g| g.id.as_str()), Some("g-hide"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
