//! The parity suite's checks of the canvas visuals (GUI only): dust on unused
//! sessions, stacks and hidden subsessions with their animations' end
//! states, effects from `copilot-canvas effect` (acknowledged by a click that
//! starts nothing; animated only while visible), the group chips at low zoom
//! and the sidebar's folder and passive rows, and group files opened in the
//! canvas in remote mode (`--remote loopback`: never in Explorer).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Gui, Kind, Row, Sandbox, check_kind};
use crate::client::doc::{Doc, Group, Node, Passive};
use crate::selftest::{Ui, wait_doc};

const DAY: u64 = 86_400_000;

fn session(id: &str, name: &str, x: i32, y: i32, cwd: &Path, used_days_ago: Option<u64>) -> Node {
    let now = crate::paths::now_ms();
    let mut node: Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y })).expect("a node");
    node.cwd = Some(cwd.display().to_string());
    node.reviewed_ms = Some(now);
    node.last_used_ms = used_days_ago.map(|d| now - d * DAY);
    node
}

/// The visuals canvas: four sessions used 0, 5, 20 and 90 days ago, a parent
/// with two subsessions (stacked when `stacked`), a passive session (with
/// `passive`: its loop arc animates), and a
/// group with a folder (`ai-docs/notes.md`, `GOALS.md`, the passive files).
pub(crate) fn canvas(home: &Path, work: &Path, folder: &Path, stacked: bool, passive: bool) -> Result<PathBuf> {
    let mut doc = Doc::default();
    for (i, (id, name, age)) in [("v-fresh", "Fresh one", 0), ("v-light", "Unused 5 days", 5), ("v-medium", "Unused 20 days", 20), ("v-heavy", "Unused 90 days", 90)].iter().enumerate() {
        doc.nodes.push(session(id, name, 2 + i as i32 * 36, 2, work, Some(*age)));
    }
    let mut parent = session("v-parent", "Parent", 2, 10, work, Some(0));
    parent.stacked = stacked;
    doc.nodes.push(parent);
    for (i, (id, name)) in [("v-child-a", "Child A"), ("v-child-b", "Child B")].iter().enumerate() {
        let mut child = session(id, name, 38, 10 + i as i32 * 7, work, Some(0));
        child.parent = Some("v-parent".into());
        doc.nodes.push(child);
    }
    let mut watcher = session("v-watcher", "Watcher", 110, 17, folder, None);
    watcher.passive = Some(Passive { key: "v-watcher-key".into(), group: "g-visual".into(), slug: "watcher".into(), template: "Custom".into(), rollover_runs: 20, max_runs_per_day: 24, ..Default::default() });
    if passive {
        doc.nodes.push(watcher);
    }
    doc.groups.push(Group { id: "g-visual".into(), name: "Visual group".into(), x: 0, y: 0, w: 146, h: 25, color: 2, folder: Some(folder.display().to_string()), source: None, defaults: None });
    doc.view.x = -1;
    doc.view.y = -1;
    let docs = folder.join("ai-docs");
    let passive = folder.join(".canvas").join("passive").join("watcher");
    for dir in [&docs, &folder.join("human-outputs"), &passive] {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(docs.join("notes.md"), "# Notes\nVISUAL NOTES on the service's machine\n")?;
    std::fs::write(docs.join("GOALS.md"), "# Goals\nKeep the canvas tidy.\n")?;
    std::fs::write(passive.join("rules.md"), "# Watcher rules\nWATCHER RULES TEXT\n")?;
    std::fs::write(passive.join("state.md"), "# Memory\n")?;
    std::fs::write(passive.join("log.md"), "# Run log\n")?;
    let path = home.join("visuals.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

fn rect(v: &Value) -> Option<[f64; 4]> {
    let r: Vec<f64> = v.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
}

fn boxes(tree: &Value) -> Vec<Value> {
    tree["canvas"]["boxes"].as_array().cloned().unwrap_or_default()
}

fn named<'a>(boxes: &'a [Value], name: &str) -> Option<&'a Value> {
    boxes.iter().find(|b| b["name"] == name)
}

fn center(r: [f64; 4]) -> (f64, f64) {
    (r[0] + (r[2] / 2.0).min(6.0), r[1] + r[3] / 2.0)
}

/// Frames the GUI presented in `secs` seconds.
fn frames_in(gui: &Gui, secs: f64) -> Result<u64> {
    let before = gui.req(json!({ "cmd": "perf" }))?["perf"]["presented"].as_u64().unwrap_or(0);
    std::thread::sleep(Duration::from_secs_f64(secs));
    Ok(gui.req(json!({ "cmd": "perf" }))?["perf"]["presented"].as_u64().unwrap_or(0).saturating_sub(before))
}

/// Runs `copilot-canvas effect …` as a session of the visual group.
pub(crate) fn effect(exe: &Path, home: &Path, instance: &str, args: &[&str]) -> Result<()> {
    // The service resolves names with the routing the canvas sent it (once a second).
    let routing = home.join("bus").join("routing.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&routing).is_ok_and(|r| r.contains("v-fresh")) {
        anyhow::ensure!(Instant::now() < deadline, "the canvas never sent its routing to the service");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = std::process::Command::new(exe)
        .arg("effect")
        .args(args)
        .args(["--instance", instance, "--home", &home.display().to_string()])
        .env("COPILOT_CANVAS_SESSION", "v-fresh")
        .output()?;
    anyhow::ensure!(out.status.success(), "copilot-canvas effect: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// The visual rows (each on the visuals canvas).
pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    // Animated effects whatever this machine's animation setting (the GUI inherits it).
    // SAFETY: set before the GUI starts; nothing else reads it concurrently.
    unsafe { std::env::set_var("COPILOT_CANVAS_EFFECTS", "on") };
    let gui_only = |why| Kind::Only(why);
    let canvas = match canvas(&sb.instance.home, &sb.work, &sb.group_folder, false, true) {
        Ok(path) => path,
        Err(error) => return check_kind(rows, "V", "Canvas visuals", "visuals_canvas", Kind::E2e, || Err(error)),
    };
    let canvas_arg = canvas.display().to_string();
    let mut gui = match sb.start(&["--canvas", &canvas_arg], "gui-visuals.log") {
        Ok(gui) => gui,
        Err(error) => return check_kind(rows, "V", "Canvas visuals", "visuals_canvas", Kind::E2e, || Err(error)),
    };
    let ready = gui.wait("the visuals canvas", 25, |t| t.contains("Visual group")).and_then(|_| gui.send("f"));
    if let Err(error) = ready {
        return check_kind(rows, "V", "Canvas visuals", "visuals_canvas", Kind::E2e, || Err(error));
    }

    check_kind(rows, "V1", "Dust: unused sessions dusty by age (static), age in the box and sidebar, Select unused", "dust_levels_and_select_unused", gui_only("the TUI dims and shows the age (snapshot tests)"), || {
        let tree = gui.wait_tree("dust levels from last use", 15, |t| {
            let b = boxes(t);
            named(&b, "Unused 90 days").is_some_and(|b| b["dust"] == 3) && named(&b, "Unused 20 days").is_some_and(|b| b["dust"] == 2)
        })?;
        let b = boxes(&tree);
        let level = |name: &str| named(&b, name).map(|b| b["dust"].as_u64().unwrap_or(9)).unwrap_or(9);
        anyhow::ensure!(level("Fresh one") == 0 && level("Unused 5 days") == 1, "levels {} {}", level("Fresh one"), level("Unused 5 days"));
        anyhow::ensure!(named(&b, "Unused 20 days").is_some_and(|b| b["line"].as_str().is_some_and(|l| l.contains("unused 20d"))), "age in the status line");
        gui.wait("the age in the sidebar", 5, |t| t.lines().any(|l| l.contains("Unused 90 days") && l.contains("90d")))?;
        let empty = {
            let g = tree["canvas"]["groups"].as_array().and_then(|g| g.first().cloned()).context("group")?;
            let r = rect(&g["rect"]).context("group rect")?;
            (r[0] + r[2] - 3.5, r[1] + r[3] - 2.5)
        };
        gui.mouse("down", "right", empty, &[])?;
        gui.mouse("up", "right", empty, &[])?;
        gui.wait("the canvas menu", 5, |t| t.contains("Select unused sessions"))?;
        let (x, y) = gui.find("Select unused sessions").context("menu item")?;
        gui.click((x + 2.0, y), &[])?;
        let tree = gui.wait_tree("the unused sessions selected", 5, |t| t["selection"].as_array().is_some_and(|s| s.len() == 3))?;
        let selection: Vec<String> = tree["selection"].as_array().map(|s| s.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        anyhow::ensure!(selection.iter().all(|s| ["v-light", "v-medium", "v-heavy"].contains(&s.as_str())), "{selection:?}");
        gui.key("Esc", &[])?;
        Ok(format!("levels 0/1/2/3 at 0/5/20/90 days, ages shown; selected {selection:?}"))
    });

    check_kind(rows, "V2", "Stacks: K or the menu stacks subsessions (animated), double-click unstacks to the same places; H hides all", "stack_unstack_and_hide", gui_only("the TUI switches without animation (unit tests)"), || {
        gui.canvas_focus()?;
        let tree = gui.tree()?;
        let b = boxes(&tree);
        let homes: Vec<[f64; 4]> = ["Child A", "Child B"].iter().map(|n| named(&b, n).and_then(|b| rect(&b["rect"])).context("a child box")).collect::<Result<_>>()?;
        let parent = named(&b, "Parent").and_then(|b| rect(&b["rect"])).context("parent")?;
        gui.mouse("down", "right", center(parent), &[])?;
        gui.mouse("up", "right", center(parent), &[])?;
        gui.wait("the session menu", 5, |t| t.contains("Stack subsessions"))?;
        let (x, y) = gui.find("Stack subsessions").context("menu item")?;
        gui.click((x + 2.0, y), &[])?;
        // The cards fly into the deck (some frame shows them part way) …
        let started = Instant::now();
        let mut midway = false;
        while started.elapsed() < Duration::from_millis(600) && !midway {
            midway = boxes(&gui.tree()?).iter().any(|b| b["alpha"].as_f64().is_some_and(|a| a < 0.99));
            std::thread::sleep(Duration::from_millis(15));
        }
        // … and end in the deck: no child boxes, one top card with +N.
        let tree = gui.wait_tree("the stack's end state", 5, |t| {
            let b = boxes(t);
            named(&b, "Child A").is_none() && b.iter().any(|b| b["deck"]["count"] == 2 && b["alpha"] == 1.0)
        })?;
        wait_doc(Path::new(&canvas_arg), "stacked in the document", |d| d.node("v-parent").is_some_and(|n| n.stacked))?;
        let _ = gui.png(0, "visual-stack");
        let deck = boxes(&tree).into_iter().find(|b| !b["deck"].is_null()).context("deck")?;
        anyhow::ensure!(deck["name"] == "Child B", "the newest on top: {deck}");
        let deck_rect = rect(&deck["rect"]).context("deck rect")?;
        anyhow::ensure!(deck_rect[1] > parent[1], "beneath the parent");
        // Double-click: back to exactly where they were.
        let at = center(deck_rect);
        gui.click(at, &[])?;
        gui.click(at, &[])?;
        let tree = gui.wait_tree("the unstacked end state", 5, |t| {
            let b = boxes(t);
            named(&b, "Child A").is_some() && b.iter().all(|b| b["alpha"] == 1.0) && b.iter().all(|b| b["deck"].is_null())
        })?;
        let b = boxes(&tree);
        let back: Vec<[f64; 4]> = ["Child A", "Child B"].iter().filter_map(|n| named(&b, n).and_then(|b| rect(&b["rect"]))).collect();
        anyhow::ensure!(back == homes, "restored places {back:?} vs {homes:?}");
        // H hides every subsession (⋯ 2 hidden on the parent), H shows them again.
        gui.canvas_focus()?;
        gui.send("h")?;
        let hidden = gui.wait_tree("subsessions hidden", 5, |t| {
            let b = boxes(t);
            named(&b, "Child A").is_none() && named(&b, "Parent").is_some_and(|p| p["hidden"] == 2)
        })?;
        anyhow::ensure!(hidden["canvas"]["boxes"].as_array().is_some_and(|b| !b.is_empty()));
        let _ = gui.png(0, "visual-hidden");
        gui.send("h")?;
        let tree = gui.wait_tree("subsessions shown again", 5, |t| {
            let b = boxes(t);
            named(&b, "Child A").is_some() && b.iter().all(|b| b["alpha"] == 1.0)
        })?;
        let b = boxes(&tree);
        let shown: Vec<[f64; 4]> = ["Child A", "Child B"].iter().filter_map(|n| named(&b, n).and_then(|b| rect(&b["rect"]))).collect();
        anyhow::ensure!(shown == homes, "shown in place");
        Ok(format!("stacked (a transition frame seen: {midway}), deck beneath with +1, unstacked to the same places, hidden and shown"))
    });

    check_kind(rows, "V3", "Effects: copilot-canvas effect on a box, animated ≤ 15 fps while visible, a click clears it without starting anything", "effect_fireworks_ack", gui_only("the TUI shows a badge (snapshot tests)"), || {
        gui.canvas_focus()?;
        effect(&sb.exe, &sb.instance.home, &sb.instance.name, &["fireworks", "--on", "Fresh one", "--note", "PR !7 approved"])?;
        let tree = gui.wait_tree("the fireworks", 15, |t| t["canvas"]["effects"].as_array().is_some_and(|e| e.iter().any(|e| e["kind"] == "fireworks" && e["badge"] == "✦ PR !7 approved")))?;
        let motion = tree["canvas"]["motion"].as_str().unwrap_or_default().to_owned();
        let _ = gui.png(0, "visual-fireworks");
        let animated = frames_in(&gui, 2.0)?;
        if motion == "On" {
            anyhow::ensure!((8..=36).contains(&animated), "{animated} frames in 2 s while the fireworks run (≤ 15 fps)");
        }
        let fresh = named(&boxes(&tree), "Fresh one").and_then(|b| rect(&b["rect"])).context("box")?;
        gui.click(center(fresh), &[])?;
        gui.wait_tree("the effect cleared here", 5, |t| t["canvas"]["effects"].as_array().is_some_and(Vec::is_empty))?;
        let tree = gui.tree()?;
        anyhow::ensure!(tree["selection"].as_array().is_some_and(|s| s.iter().any(|v| v == "v-fresh")), "the box is selected");
        std::thread::sleep(Duration::from_millis(1500));
        let state = std::fs::read_to_string(sb.instance.home.join("bus").join("state.json")).unwrap_or_default();
        let effects = serde_json::from_str::<Value>(&state).ok().and_then(|s| s["effects"].as_array().map(Vec::len)).unwrap_or(99);
        anyhow::ensure!(effects == 0, "the service forgot it ({effects} left)");
        anyhow::ensure!(gui.tree()?["tiles"].as_array().is_some_and(Vec::is_empty), "no session started");
        let idle = frames_in(&gui, 2.0)?;
        anyhow::ensure!(idle <= 8, "{idle} frames in 2 s after the click (idle again)");
        Ok(format!("motion {motion}: {animated} frames in 2 s while animated, {idle} after; acknowledged everywhere, nothing started"))
    });

    check_kind(rows, "V4", "Group chips at any zoom (folders, ⟲ passive), sidebar folder and passive rows open menus and panels", "chips_low_zoom_and_sidebar", gui_only("the TUI draws the same chips and rows (snapshot tests)"), || {
        gui.canvas_focus()?;
        gui.key("Esc", &[])?;
        gui.wait("the sidebar's folder and passive rows", 10, |t| t.contains("◧ group") && t.lines().any(|l| l.contains("⟲") && l.contains("Watcher")))?;
        gui.send("-")?;
        gui.send("-")?;
        let tree = gui.wait_tree("zoomed out", 5, |t| t["canvas"]["scale"].as_f64().is_some_and(|s| s <= 0.3))?;
        let group = tree["canvas"]["groups"].as_array().and_then(|g| g.first().cloned()).context("group")?;
        let chips = group["chips"].as_array().cloned().unwrap_or_default();
        anyhow::ensure!(chips.len() >= 2 && chips.iter().all(|c| c["fixed"] == true), "compact chips at {}: {chips:?}", tree["canvas"]["scale"]);
        let passive = chips.iter().find(|c| c["label"] == "⟲ 1").context("the passive chip")?;
        let at = center(rect(&passive["rect"]).context("chip rect")?);
        gui.click((at.0 - 0.0, at.1), &[])?;
        gui.wait("the passive sessions menu", 5, |t| t.contains("Passive sessions of") && t.contains("Watcher"))?;
        let _ = gui.png(0, "visual-chips-25");
        gui.key("Enter", &[])?;
        gui.wait("the passive panel", 5, |t| t.contains("⟲ Watcher") && t.contains("Reacts to"))?;
        anyhow::ensure!(gui.tree()?["tiles"].as_array().is_some_and(Vec::is_empty), "the panel, no session started");
        gui.key("Esc", &[])?;
        gui.send("+")?;
        gui.send("+")?;
        Ok(format!("{} chips at 25 %, passive chip → menu → panel; sidebar rows", chips.len()))
    });

    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));

    check_kind(rows, "V5", "Remote mode: group folders in the canvas's browser, passive files in the viewer (never Explorer)", "remote_group_files_in_canvas", gui_only("the TUI uses the same core (unit tests)"), || {
        let mut gui = sb.start(&["--remote", "loopback", "--canvas", &canvas_arg], "gui-visuals-remote.log")?;
        gui.wait("the visuals canvas over TCP", 25, |t| t.contains("Visual group"))?;
        gui.send("f")?;
        let tree = gui.wait_tree("the chips", 5, |t| t["canvas"]["groups"][0]["chips"].as_array().is_some_and(|c| !c.is_empty()))?;
        let chips = tree["canvas"]["groups"][0]["chips"].as_array().cloned().unwrap_or_default();
        let docs = chips.iter().find(|c| c["label"].as_str().is_some_and(|l| l.starts_with("◧"))).context("the AI docs chip")?;
        gui.click(center(rect(&docs["rect"]).context("chip rect")?), &[])?;
        gui.wait("the folder browser", 10, |t| t.contains("Folder on the service's machine") && t.contains("notes.md") && t.contains("GOALS.md"))?;
        let _ = gui.png(0, "visual-remote-browser");
        let (x, y) = gui.find("notes.md").context("the file entry")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("the file in the viewer", 10, |t| t.contains("VISUAL NOTES") && t.contains("read-only"))?;
        gui.key("Esc", &[])?;
        gui.key("Esc", &[])?;
        gui.canvas_focus()?;
        // A passive session's rules: the panel's Rules button.
        let (x, y) = gui.driver.find_in("Watcher", 0, 33).context("the sidebar's passive row")?;
        gui.click((x as f64 + 2.5, y as f64 + 0.5), &[])?;
        gui.wait("the passive panel", 10, |t| t.contains("Reacts to") && t.contains("Rules"))?;
        let (x, y) = gui.find(" Rules ").or_else(|| gui.find("Rules")).context("Rules button")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("the rules in the viewer", 10, |t| t.contains("WATCHER RULES TEXT"))?;
        gui.key("Esc", &[])?;
        let log = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        anyhow::ensure!(!log.contains(" for a client: "), "the service opened Explorer or a program for the remote client");
        let _ = gui.req(json!({ "cmd": "quit" }));
        let _ = gui.driver.wait_exit(Duration::from_secs(10));
        Ok("AI docs chip → browser (files listed) → notes.md in the viewer; passive rules in the viewer; no Explorer on the service".into())
    });
}
