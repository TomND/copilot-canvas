//! The parity suite's check of folded subagent boxes (row 46), on a canvas
//! of its own: a live session whose history has a running subagent, and its
//! subagent boxes saved at places of their own by a build that did not fold
//! (no choice saved: folded by default).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Row, Sandbox, check};
use crate::client::doc::{Doc, Group, Node};
use crate::selftest::{Ui, load, wait_doc};

/// The subagents canvas: "Folded parent" (the sandbox's "With agents"
/// session) at 2,2 with its subagent boxes "Parity explorer" (running, in
/// its history) saved at 6,7 and "Older helper" (no longer in its history)
/// at 6,11, in "Subagent group".
fn canvas(home: &Path, work: &Path, parent: &str, agents: [&str; 2]) -> Result<PathBuf> {
    let mut doc = Doc::default();
    let mut session: Node = serde_json::from_value(json!({ "id": parent, "kind": "session", "name": "Folded parent", "x": 2, "y": 2 }))?;
    session.cwd = Some(work.display().to_string());
    session.reviewed_ms = Some(crate::paths::now_ms());
    doc.nodes.push(session);
    doc.nodes.push(serde_json::from_value(json!({ "id": agents[0], "kind": "agent", "name": "Parity explorer", "x": 6, "y": 7, "parent": parent, "agent_key": "a1" }))?);
    doc.nodes.push(serde_json::from_value(json!({ "id": agents[1], "kind": "agent", "name": "Older helper", "x": 6, "y": 11, "parent": parent, "agent_key": "a0" }))?);
    doc.groups.push(Group { id: "g-sub".into(), name: "Subagent group".into(), x: 0, y: 0, w: 40, h: 18, color: 4, ..Default::default() });
    doc.view.x = -1;
    doc.view.y = -1;
    let path = home.join("subagents.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

fn boxes(tree: &Value) -> Vec<Value> {
    tree["canvas"]["boxes"].as_array().cloned().unwrap_or_default()
}

fn named(tree: &Value, name: &str) -> Option<Value> {
    boxes(tree).into_iter().find(|b| b["name"] == name)
}

fn rect(v: &Value) -> Option<[f64; 4]> {
    let r: Vec<f64> = v.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
}

fn place(doc: &Doc, id: &str) -> Option<(i32, i32)> {
    doc.node(id).map(|n| (n.x, n.y))
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(
        rows,
        "46",
        "New: subagent boxes fold into a deck beneath their session by default (working while one runs); K shows them at their saved places and the choice survives a restart; H hides them; nothing starts",
        "subagents_folded_by_default",
        || folded_by_default(sb),
    );
}

fn folded_by_default(sb: &Sandbox) -> Result<String> {
    let parent = sb.sessions.agents.clone();
    let (agent, older) = (format!("{parent}:a1"), format!("{parent}:a0"));
    let path = canvas(&sb.instance.home, &sb.work, &parent, [&agent, &older])?;
    let arg = path.display().to_string();
    let launches = || std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count();
    let before = launches();
    let deck = |t: &Value| boxes(t).into_iter().find(|b| b["deck"]["root"] == parent.as_str());
    let places = |d: &Doc| (place(d, &agent), place(d, &older));
    let mut gui = sb.start(&["--canvas", &arg], "gui-subagents.log")?;
    gui.wait("the subagents canvas", 25, |t| t.contains("Subagent group"))?;
    // Folded by default: a slim deck beneath the session, working while a subagent runs.
    let tree = gui.wait_tree("the deck, working", 20, |t| deck(t).is_some_and(|d| d["status"] == "working" && d["deck"]["agents"] == 2))?;
    let card = deck(&tree).context("deck")?;
    anyhow::ensure!(card["name"] == "2 subagents" && card["deck"]["back"] == 1, "one “2 subagents” card with a card behind it: {card}");
    anyhow::ensure!(named(&tree, "Parity explorer").is_none() && named(&tree, "Older helper").is_none(), "no box of their own");
    let saved = places(&load(&path)?);
    anyhow::ensure!(saved == (Some((6, 7)), Some((6, 11))), "their saved places are kept while folded: {saved:?}");
    let _ = gui.png(0, "subagent-deck");
    gui.send("-")?;
    std::thread::sleep(Duration::from_millis(300));
    let _ = gui.png(0, "subagent-deck-50");
    gui.send("+")?;
    // A click on the deck lists them in its session's details (and starts nothing).
    let tree = gui.wait_tree("back at 100 %", 5, |t| t["canvas"]["scale"].as_f64().is_some_and(|s| s >= 0.99))?;
    let r = deck(&tree).and_then(|d| rect(&d["rect"])).context("deck rect")?;
    gui.click((r[0] + 2.5, r[1] + r[3] / 2.0), &[])?;
    gui.wait("the subagents in their session's details", 5, |t| t.contains("SUBAGENTS 2") && t.contains("Parity explorer") && t.contains("Older helper"))?;
    let _ = gui.png(0, "subagent-deck-details");
    // K shows them at exactly their saved places (the GUI animates it), saved in the canvas file.
    gui.canvas_focus()?;
    gui.send("k")?;
    let started = Instant::now();
    let mut midway = false;
    while started.elapsed() < Duration::from_millis(600) && !midway {
        midway = boxes(&gui.tree()?).iter().any(|b| b["alpha"].as_f64().is_some_and(|a| a < 0.99));
        std::thread::sleep(Duration::from_millis(15));
    }
    let tree = gui.wait_tree("the subagent boxes shown", 5, |t| ["Parity explorer", "Older helper"].iter().all(|n| named(t, n).is_some_and(|b| b["alpha"] == 1.0)) && deck(t).is_none())?;
    wait_doc(&path, "shown in the canvas file, at their saved places", |d| d.node(&parent).is_some_and(|n| n.subagents_shown == Some(true)) && places(d) == saved)?;
    let scale = tree["canvas"]["scale"].as_f64().unwrap_or(1.0);
    let at = |name: &str| named(&tree, name).and_then(|b| rect(&b["rect"])).with_context(|| name.to_owned());
    let (p, a, o) = (at("Folded parent")?, at("Parity explorer")?, at("Older helper")?);
    let drawn = |r: [f64; 4], dx: f64, dy: f64| (r[0] - p[0] - dx * scale).abs() < 0.01 && (r[1] - p[1] - dy * scale).abs() < 0.01;
    anyhow::ensure!(drawn(a, 4.0, 5.0) && drawn(o, 4.0, 9.0), "drawn at their saved places: parent {p:?}, boxes {a:?} {o:?} at scale {scale}");
    let _ = gui.png(0, "subagent-deck-unfolded");
    // The choice survives a restart of the GUI (the canvas file has it: see above).
    let _ = gui.req(json!({ "cmd": "quit" }));
    gui.driver.wait_exit(Duration::from_secs(10))?;
    let mut gui = sb.start(&["--canvas", &arg], "gui-subagents-again.log")?;
    gui.wait("the subagents canvas again", 25, |t| t.contains("Subagent group"))?;
    gui.wait_tree("still shown after a restart", 20, |t| named(t, "Parity explorer").is_some() && named(t, "Older helper").is_some() && deck(t).is_none())?;
    // H hides them (◇ 2 hidden on their session, counted in the toolbar), H shows them again in place.
    gui.canvas_focus()?;
    gui.send("h")?;
    gui.wait_tree("hidden, counted on their session", 5, |t| named(t, "Parity explorer").is_none() && named(t, "Folded parent").is_some_and(|b| b["hidden"] == 2 && b["hidden_agents"] == 2))?;
    gui.wait("the toolbar's count and the marker", 5, |t| t.contains("↳ 2 hidden") && t.contains("◇ 2 hidden"))?;
    gui.send("h")?;
    gui.wait_tree("shown again", 5, |t| named(t, "Parity explorer").is_some_and(|b| b["alpha"] == 1.0))?;
    // K on the session stacks them again: the deck is back, their places kept.
    let at = {
        let tree = gui.tree()?;
        let p = named(&tree, "Folded parent").and_then(|b| rect(&b["rect"])).context("parent")?;
        (p[0] + 2.5, p[1] + p[3] / 2.0)
    };
    gui.click(at, &[])?;
    gui.canvas_focus()?;
    gui.send("k")?;
    gui.wait_tree("folded again", 5, |t| deck(t).is_some_and(|d| d["alpha"] == 1.0) && named(t, "Parity explorer").is_none())?;
    wait_doc(&path, "folded in the canvas file, their places kept", |d| d.node(&parent).is_some_and(|n| n.subagents_shown == Some(false)) && places(d) == saved)?;
    std::thread::sleep(Duration::from_millis(300));
    anyhow::ensure!(launches() == before, "a fold, unfold, hide or click started a session");
    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
    Ok(format!("deck “2 subagents” working; a click listed them; K showed them at their saved places (a transition frame seen: {midway}); still shown after a restart; H hid them (◇ 2 hidden, ↳ 2 hidden) and showed them; K folded them again; nothing started"))
}

