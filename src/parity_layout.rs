//! The parity suite's checks of canvas layout undo (row 44) and area
//! selection of subsessions (row 45), on a canvas of their own: a group with
//! a parent session, its subagent box, a subsession with its subagent box in
//! a nested group, a shell and another session.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check};
use crate::client::doc::{Doc, Group, Node, NodeKind};
use crate::selftest::{Ui, wait_doc};

fn session(id: &str, name: &str, x: i32, y: i32, cwd: &Path) -> Node {
    let mut node: Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y })).expect("a node");
    node.cwd = Some(cwd.display().to_string());
    node.reviewed_ms = Some(crate::paths::now_ms());
    node
}

/// The layout canvas (world units): "Layout group" 0,0 70×30 holds Parent
/// (with its subagent box Helper), the nested group Inner with Child (a
/// subsession of Parent) and its subagent box, Layout shell and Other.
fn canvas(home: &Path, work: &Path) -> Result<PathBuf> {
    let mut doc = Doc::default();
    doc.nodes.push(session("l-parent", "Parent", 2, 2, work));
    let mut helper = session("l-parent:helper", "Helper", 6, 7, work);
    (helper.kind, helper.parent, helper.agent_key) = (NodeKind::Agent, Some("l-parent".into()), Some("helper".into()));
    doc.nodes.push(helper);
    let mut child = session("l-child", "Child", 2, 13, work);
    child.parent = Some("l-parent".into());
    doc.nodes.push(child);
    let mut child_helper = session("l-child:x", "Child helper", 6, 18, work);
    (child_helper.kind, child_helper.parent, child_helper.agent_key) = (NodeKind::Agent, Some("l-child".into()), Some("x".into()));
    doc.nodes.push(child_helper);
    let mut shell = session("l-shell", "Layout shell", 36, 2, work);
    shell.command = Some(vec!["powershell.exe".into(), "-NoLogo".into(), "-NoProfile".into()]);
    shell.fresh = true;
    doc.nodes.push(shell);
    doc.nodes.push(session("l-other", "Other", 36, 9, work));
    doc.groups.push(Group { id: "g-layout".into(), name: "Layout group".into(), x: 0, y: 0, w: 70, h: 30, color: 1, ..Default::default() });
    doc.groups.push(Group { id: "g-inner".into(), name: "Inner".into(), x: 0, y: 12, w: 34, h: 12, color: 3, ..Default::default() });
    doc.view.x = -1;
    doc.view.y = -1;
    let path = home.join("layout.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

type Geometry = (Vec<(String, i32, i32)>, Vec<(String, i32, i32, i32, i32)>);

/// The places of this canvas's own boxes and groups (the sandbox's
/// automations add boxes of their own meanwhile).
fn geometry(doc: &Doc) -> Geometry {
    let mut nodes: Vec<(String, i32, i32)> = doc.nodes.iter().filter(|n| ours(&n.id)).map(|n| (n.id.clone(), n.x, n.y)).collect();
    let mut groups: Vec<(String, i32, i32, i32, i32)> = doc.groups.iter().filter(|g| ours(&g.id)).map(|g| (g.id.clone(), g.x, g.y, g.w, g.h)).collect();
    nodes.sort();
    groups.sort();
    (nodes, groups)
}

fn ours(id: &str) -> bool {
    id.starts_with("l-") || id == "g-layout" || id == "g-inner"
}

fn rect(v: &Value) -> Option<[f64; 4]> {
    let r: Vec<f64> = v.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
}

fn group_rect(gui: &Gui, id: &str) -> Result<[f64; 4]> {
    let tree = gui.tree()?;
    tree["canvas"]["groups"].as_array().and_then(|g| g.iter().find(|g| g["id"] == id)).and_then(|g| rect(&g["rect"])).with_context(|| format!("group {id} in view"))
}

fn selection(gui: &Gui) -> Result<BTreeSet<String>> {
    Ok(gui.tree()?["selection"].as_array().map(|s| s.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default())
}

fn ids(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// `+` / `-` until the canvas is at `scale`.
fn zoom_to(gui: &mut Gui, scale: f64) -> Result<()> {
    for _ in 0..8 {
        let now = gui.tree()?["canvas"]["scale"].as_f64().unwrap_or(0.0);
        if (now - scale).abs() < 0.01 {
            return Ok(());
        }
        gui.send(if now < scale { "+" } else { "-" })?;
        std::thread::sleep(Duration::from_millis(150));
    }
    anyhow::bail!("the canvas never reached scale {scale}")
}

fn launches(sb: &Sandbox) -> usize {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count()
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    let path = match canvas(&sb.instance.home, &sb.work) {
        Ok(path) => path,
        Err(error) => return check(rows, "44", "Layout undo", "layout_canvas", || Err(error)),
    };
    let arg = path.display().to_string();
    let mut gui = match sb.start(&["--canvas", &arg], "gui-layout.log") {
        Ok(gui) => gui,
        Err(error) => return check(rows, "44", "Layout undo", "layout_canvas", || Err(error)),
    };
    if let Err(error) = gui.wait("the layout canvas", 25, |t| t.contains("Layout group")) {
        return check(rows, "44", "Layout undo", "layout_canvas", || Err(error));
    }

    check(rows, "45", "New: Shift-drag selects subagent boxes and subsessions like sessions (100 % and 50 %), never hidden ones; the selection moves each box once", "subsession_area_selection", || {
        let before_launches = launches(sb);
        gui.canvas_focus()?;
        gui.key("Esc", &[])?;
        let mut detail = Vec::new();
        for scale in [1.0, 0.5] {
            zoom_to(&mut gui, scale)?;
            let start = sb_doc(&path)?;
            // From on top of the parent over its subagent box: an area, never a drag.
            let (_, p) = gui.find_box("Parent")?;
            let (_, h) = gui.find_box("Helper")?;
            gui.drag("left", (p[0] + 0.6, p[1] + 0.4), (h[0] + h[2] - 0.6, h[1] + h[3] - 0.3), &["shift"])?;
            gui.wait_tree(&format!("parent and subagent selected at {scale}"), 5, |t| t["selection"].as_array().is_some_and(|s| s.len() == 2))?;
            anyhow::ensure!(selection(&gui)? == ids(&["l-parent", "l-parent:helper"]), "at {scale}: {:?}", selection(&gui)?);
            // Adding the subsession (Shift adds).
            let (_, c) = gui.find_box("Child")?;
            gui.drag("left", (c[0] + 0.6, c[1] + 0.4), (c[0] + c[2] - 0.6, c[1] + c[3] - 0.3), &["shift"])?;
            gui.wait_tree("the subsession added", 5, |t| t["selection"].as_array().is_some_and(|s| s.len() == 3))?;
            anyhow::ensure!(selection(&gui)? == ids(&["l-parent", "l-parent:helper", "l-child"]), "at {scale}: {:?}", selection(&gui)?);
            anyhow::ensure!(geometry(&sb_doc(&path)?) == geometry(&start), "a Shift-drag moved a box at {scale}");
            // Dragging the parent moves every selected box (and the subsession's subagent box) once.
            let at = gui.box_center("Parent")?;
            gui.drag("left", at, (at.0 + 3.0, at.1 + 1.0), &[])?;
            let d = ((3.0 / scale) as i32, (1.0 / scale) as i32);
            let pos = |doc: &Doc, id: &str| doc.node(id).map(|n| (n.x, n.y)).unwrap_or_default();
            let moved = wait_doc(&path, &format!("the parent moved by {d:?}"), |doc| pos(doc, "l-parent") == (pos(&start, "l-parent").0 + d.0, pos(&start, "l-parent").1 + d.1))?;
            let delta = |id: &str| (pos(&moved, id).0 - pos(&start, id).0, pos(&moved, id).1 - pos(&start, id).1);
            for id in ["l-parent:helper", "l-child", "l-child:x"] {
                anyhow::ensure!(delta(id) == d, "{id} moved by {:?}, the parent by {d:?} (at {scale})", delta(id));
            }
            anyhow::ensure!(delta("l-other") == (0, 0) && delta("l-shell") == (0, 0), "boxes outside the selection stay");
            gui.chord('z', &["ctrl"])?;
            wait_doc(&path, "Ctrl+Z put the selection back", |doc| geometry(doc) == geometry(&start))?;
            gui.key("Esc", &[])?;
            detail.push(format!("{:.0} %: parent + subagent, + subsession, each moved once by {d:?}", scale * 100.0));
        }
        // Hidden subsessions are out of reach.
        gui.send("h")?;
        let g = group_rect(&gui, "g-layout")?;
        gui.drag("left", (g[0] + 1.5, g[1] + 1.5), (g[0] + g[2] - 1.5, g[1] + g[3] - 1.5), &["shift"])?;
        let hidden = gui.wait_tree("the visible boxes selected", 5, |t| t["selection"].as_array().is_some_and(|s| s.len() >= 3))?;
        let chosen: BTreeSet<String> = hidden["selection"].as_array().map(|s| s.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        anyhow::ensure!(!chosen.contains("l-child") && !chosen.contains("l-child:x") && chosen.contains("l-parent:helper"), "with subsessions hidden: {chosen:?}");
        gui.send("h")?;
        gui.key("Esc", &[])?;
        anyhow::ensure!(launches(sb) == before_launches, "an area selection started a session");
        Ok(format!("{}; hidden ones left out ({} selected); nothing started", detail.join("; "), chosen.len()))
    });

    check(rows, "44", "New: Ctrl+Z undoes a group drag (boxes, subagents, nested group) and a ◢ resize, one gesture per step, to the exact places; Ctrl+Y redoes; in a terminal Ctrl+Z is the program's", "layout_undo_redo", || {
        gui.canvas_focus()?;
        gui.key("Esc", &[])?;
        zoom_to(&mut gui, 0.5)?;
        // Selected (its panel narrows the canvas) and centred: the whole group in view.
        let g = group_rect(&gui, "g-layout")?;
        gui.click((g[0] + 3.5, g[1] + 0.4), &[])?;
        gui.wait_tree("the group selected", 5, |t| t["canvas"]["groups"].as_array().is_some_and(|g| g.iter().any(|g| g["id"] == "g-layout" && g["selected"] == true)))?;
        gui.send("c")?;
        zoom_to(&mut gui, 0.5)?;
        let before = sb_doc(&path)?;
        let g = group_rect(&gui, "g-layout")?;
        // Four cells right and one up at 50 %: (8, -2) in the document (away
        // from the Automations group the sandbox adds below it).
        gui.drag("left", (g[0] + 3.5, g[1] + 0.4), (g[0] + 7.5, g[1] - 0.6), &[])?;
        let group = |d: &Doc| d.group("g-layout").map(|g| (g.x, g.y, g.w, g.h)).unwrap_or_default();
        let (dx, dy) = (8, -2);
        let moved = wait_doc(&path, "the group moved by (8, -2)", |d| group(d).0 == group(&before).0 + dx && group(d).1 == group(&before).1 + dy)?;
        for node in before.nodes.iter().filter(|n| ours(&n.id)) {
            let now = moved.node(&node.id).context("a box")?;
            anyhow::ensure!((now.x - node.x, now.y - node.y) == (dx, dy), "{} moved by ({}, {}), the group by ({dx}, {dy})", node.id, now.x - node.x, now.y - node.y);
        }
        anyhow::ensure!(moved.group("g-inner").zip(before.group("g-inner")).is_some_and(|(a, b)| (a.x - b.x, a.y - b.y) == (dx, dy)), "the nested group moved with it");
        gui.wait("the undo hint", 5, |t| t.contains("Ctrl+Z undo move"))?;
        // ◢: a second step.
        let g = group_rect(&gui, "g-layout")?;
        let corner = (g[0] + g[2] - 0.4, g[1] + g[3] - 0.3);
        let area = rect(&gui.tree()?["canvas"]["area"]).context("canvas area")?;
        anyhow::ensure!(corner.0 < area[0] + area[2] && corner.1 < area[1] + area[3], "the ◢ corner {corner:?} is outside the canvas {area:?}");
        gui.drag("left", corner, (corner.0 + 4.0, corner.1 + 2.0), &[])?;
        let resized = wait_doc(&path, "the group resized by (8, 4)", |d| (group(d).2, group(d).3) == (group(&moved).2 + 8, group(&moved).3 + 4))?;
        // A terminal with the keyboard: Ctrl+Z reaches the program, the canvas keeps both changes.
        gui.open("Layout shell", "PS ")?;
        let exe = std::env::current_exe()?;
        gui.send(&format!("& '{}' paste-probe\r", exe.display()))?;
        gui.wait("the paste probe", 20, |t| t.contains(crate::paste_probe::READY))?;
        gui.chord('z', &["ctrl"])?;
        gui.wait("Ctrl+Z in the program", 10, |t| t.contains("KEY \\x1a"))?;
        std::thread::sleep(Duration::from_millis(400));
        anyhow::ensure!(geometry(&sb_doc(&path)?) == geometry(&resized), "Ctrl+Z in a terminal changed the canvas");
        gui.chord('d', &["ctrl"])?;
        gui.wait("the probe ended", 10, |t| t.contains("BYE"))?;
        // On the canvas: one gesture per Ctrl+Z, newest first; Ctrl+Y redoes.
        let launched = launches(sb);
        gui.canvas_focus()?;
        gui.chord('z', &["ctrl"])?;
        wait_doc(&path, "Ctrl+Z took back the resize", |d| geometry(d) == geometry(&moved))?;
        gui.chord('z', &["ctrl"])?;
        wait_doc(&path, "Ctrl+Z took back the move exactly", |d| geometry(d) == geometry(&before))?;
        gui.wait("the undo notice", 5, |t| t.contains("Undid: moved group"))?;
        gui.chord('y', &["ctrl"])?;
        wait_doc(&path, "Ctrl+Y redid the move", |d| geometry(d) == geometry(&moved))?;
        gui.chord('z', &["ctrl"])?;
        wait_doc(&path, "and Ctrl+Z undid it again", |d| geometry(d) == geometry(&before))?;
        anyhow::ensure!(launches(sb) == launched, "an undo started a session");
        Ok(format!("group drag ({} boxes, nested group) by ({dx}, {dy}) and ◢ resize undone one at a time to the exact places in the canvas file, redone and undone again; Ctrl+Z in the shell reached the paste probe", geometry(&before).0.len()))
    });

    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
}

fn sb_doc(path: &Path) -> Result<Doc> {
    crate::selftest::load(path)
}
