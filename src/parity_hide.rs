//! The parity suite's checks of hiding subsessions and subagent boxes (row
//! 53: `H` and `J` each on its own; row 54: one box at a time from its menu
//! into the hidden area and back from its parent's marker and the toolbar)
//! and of group drags (row 55: a group dragged across a box outside it
//! leaves the box where it is), on a canvas of their own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check};
use crate::client::doc::{Doc, Group, Node, NodeKind};
use crate::selftest::{Ui, load, wait_doc};

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
/// child (with Child helper shown); "Drag group" 0,16 24×8 holds Mover, and
/// Bystander (30,18) is outside any group.
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
    doc.nodes.push(session("m-mover", "Mover", 2, 18, work));
    doc.nodes.push(session("m-by", "Bystander", 30, 18, work));
    doc.groups.push(Group { id: "g-drag".into(), name: "Drag group".into(), x: 0, y: 16, w: 24, h: 8, color: 2, ..Default::default() });
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

fn rect(v: &Value) -> Option<[f64; 4]> {
    let r: Vec<f64> = v.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
}

/// The text of a screen line's cells `x .. x + w`.
fn cells(line: &str, x: usize, w: usize) -> String {
    let mut col = 0;
    let mut out = String::new();
    for ch in line.chars() {
        let width = crate::client::paint::text_width(&ch.to_string());
        if col >= x && col + width <= x + w {
            out.push(ch);
        }
        col += width;
        if col >= x + w {
            break;
        }
    }
    out
}

/// Where the open menu's item `label` is (a point on its row): the row of
/// the menu (the tree's `modal`) whose text is the label, alone or before
/// its hint.
fn menu_item(gui: &Gui, label: &str) -> Result<(f64, f64)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tree = gui.tree()?;
        if let Some(m) = rect(&tree["modal"]) {
            let screen = gui.screen();
            let (x, y, w, h) = (m[0] as usize, m[1] as usize, m[2] as usize, m[3] as usize);
            for (row, line) in screen.lines().enumerate().skip(y + 1).take(h.saturating_sub(2)) {
                let inner = cells(line, x + 1, w.saturating_sub(2));
                let text = inner.trim();
                if text == label || text.strip_prefix(label).is_some_and(|rest| rest.starts_with("  ")) {
                    return Ok((x as f64 + 3.5, row as f64 + 0.5));
                }
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "no menu item {label:?}\n--- screen ---\n{}", gui.screen());
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Right-clicks a box and runs its menu's item `label`.
fn box_menu(gui: &mut Gui, name: &str, label: &str) -> Result<()> {
    gui.canvas_focus()?;
    let at = gui.box_center(name)?;
    gui.mouse("down", "right", at, &[])?;
    gui.mouse("up", "right", at, &[])?;
    let item = menu_item(gui, label).with_context(|| format!("{name}'s menu"))?;
    gui.click(item, &[])
}

/// Clicks text on the screen near its end (a marker's or a counter's
/// click target holds its last cells).
fn click_text(gui: &Gui, text: &str) -> Result<()> {
    let (x, y) = gui.find(text).with_context(|| format!("{text:?} on the screen"))?;
    let width = crate::client::paint::text_width(text) as f64;
    gui.click((x + width - 3.0, y), &[])
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

    check(rows, "54", "New (WI-025): right-click Hide puts a subsession or a subagent box into the hidden area (the canvas file, every window); its parent's marker and the toolbar's counters list it; Show puts it back in place; nothing starts", "hide_one_box_and_show_it", || {
        let before = launches(sb);
        gui.canvas_focus()?;
        gui.key("Esc", &[])?;
        let start = places(&load(&path)?);
        let hidden = |d: &Doc, id: &str| d.node(id).is_some_and(|n| n.hidden);
        // The subsession: Hide from its menu; its subagent box goes with it.
        box_menu(&mut gui, "Hide child", "Hide")?;
        wait_doc(&path, "the subsession hidden in the canvas file", |d| hidden(d, "h-child") && !hidden(d, "h-child:x"))?;
        gui.wait_tree("hidden with its subagent box, counted on the parent", 5, |t| !drawn(t, "Hide child") && !drawn(t, "Child helper") && marker(t, "Hide parent") == (2, 1))?;
        // The parent's subagent box: Hide from its menu.
        box_menu(&mut gui, "Hide helper", "Hide")?;
        wait_doc(&path, "the subagent box hidden in the canvas file", |d| hidden(d, "h-parent:helper"))?;
        gui.wait_tree("both counted on the parent", 5, |t| !drawn(t, "Hide helper") && marker(t, "Hide parent") == (3, 2))?;
        gui.wait("the counters, the marker and the sidebar's marks", 5, |t| {
            toolbar(&gui).contains(" ↳ 1 hidden ") && toolbar(&gui).contains(" ◇ 2 hidden ") && t.contains("⋯ 1 · ◇ 2 hidden") && t.lines().any(|l| l.contains("Hide child") && l.contains("hidden"))
        })?;
        let _ = gui.png(0, "hide-area");
        // The parent's marker lists them: Show the subsession, back where it was.
        click_text(&gui, "⋯ 1 · ◇ 2 hidden")?;
        let item = menu_item(&gui, "Show ↳ Hide child")?;
        menu_item(&gui, "Show all (2)")?;
        menu_item(&gui, "Show ◇ Hide helper")?;
        let _ = gui.png(0, "hide-area-list");
        gui.click(item, &[])?;
        wait_doc(&path, "shown in the canvas file at its place", |d| !hidden(d, "h-child") && d.node("h-child").is_some_and(|n| (n.x, n.y) == (36, 2)))?;
        let tree = gui.wait_tree("the subsession drawn again with its subagent box", 5, |t| drawn(t, "Hide child") && drawn(t, "Child helper") && marker(t, "Hide parent") == (1, 1))?;
        let at = |name: &str| named(&tree, name).and_then(|b| rect(&b["rect"])).with_context(|| name.to_owned());
        let (p, c) = (at("Hide parent")?, at("Hide child")?);
        let scale = tree["canvas"]["scale"].as_f64().unwrap_or(1.0);
        anyhow::ensure!(((c[0] - p[0]) - 34.0 * scale).abs() < 0.01 && (c[1] - p[1]).abs() < 0.01, "drawn where it was: parent {p:?}, child {c:?}");
        // The toolbar's subagent counter lists the rest for the whole canvas.
        gui.canvas_focus()?;
        click_text(&gui, " ◇ 1 hidden ")?;
        let item = menu_item(&gui, "Show ◇ Hide helper")?;
        menu_item(&gui, "Hide subagents")?;
        gui.click(item, &[])?;
        wait_doc(&path, "the subagent box shown in the canvas file", |d| !hidden(d, "h-parent:helper"))?;
        gui.wait_tree("everything drawn", 5, |t| ["Hide parent", "Hide helper", "Hide child", "Child helper"].iter().all(|n| drawn(t, n)) && marker(t, "Hide parent") == (0, 0))?;
        gui.wait("no counters", 5, |_| !toolbar(&gui).contains(" hidden "))?;
        anyhow::ensure!(places(&load(&path)?) == start, "hiding or showing moved a box");
        std::thread::sleep(Duration::from_millis(300));
        anyhow::ensure!(launches(sb) == before, "hiding or showing started a session");
        Ok("the subsession (with its subagent box) and the parent's subagent box hidden from their menus (the canvas file); ⋯ 1 · ◇ 2 hidden on the parent, ↳ 1 and ◇ 2 in the toolbar, the sidebar marks them; the marker's list showed the subsession at its place, the toolbar's ◇ list the subagent box; nothing started".into())
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
