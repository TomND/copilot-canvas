//! The parity suite's checks of canvas-wide agents (rows 40 and 40b): the
//! top bar's ◎ Agents panel, the Canvas agents frame, the box and the
//! sidebar row are one agent; viewing it never starts it; a canvas skill is
//! given to it; an event from a session of another group runs it (the
//! stand-in agent reports what it got, its skill included); paused, no event
//! triggers it, and neither does a session no canvas routes; Stop ends a run
//! without a failure; disabled, it is no longer routed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check};
use crate::selftest::{Ui, wait_doc};

const NAME: &str = "Parity coordinator";
const SKILL: &str = "parity-skill";

fn service_log(sb: &Sandbox) -> String {
    std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default()
}

/// The service's `bus/<file>` as JSON (`Null` while it cannot be read).
fn bus_file(sb: &Sandbox, file: &str) -> Value {
    std::fs::read_to_string(sb.instance.home.join("bus").join(file)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

fn wait_bus(sb: &Sandbox, file: &str, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let value = bus_file(sb, file);
        if ok(&value) {
            return Ok(value);
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}: {value}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The routed spec of a passive session (`Null`: not routed).
fn spec(routing: &Value, key: &str) -> Value {
    routing["passive"].as_array().and_then(|p| p.iter().find(|s| s["key"] == key).cloned()).unwrap_or(Value::Null)
}

/// The status line of the agent's box (whatever the zoom draws of it).
fn box_line(tree: &Value) -> String {
    tree["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|b| b["name"] == NAME)).and_then(|b| b["line"].as_str()).unwrap_or_default().to_owned()
}

/// Clicks the first place `text` shows.
fn click_text(gui: &Gui, text: &str) -> Result<()> {
    let at = gui.find(text).with_context(|| format!("{text:?} on screen\n{}", gui.screen()))?;
    gui.click(at, &[])
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(
        rows,
        "40",
        "New: canvas-wide agents: ◎ Agents (top bar), the Canvas agents frame, the box and the sidebar are one agent; templates; canvas skills; viewing never starts it",
        "canvas_agents_one_agent_every_entry",
        || entry(sb, gui),
    );
    check(
        rows,
        "40b",
        "New: a canvas-wide agent runs on an event from any group (stand-in agent, its skill delivered); paused or for another canvas: no run; Stop, Disable",
        "canvas_agent_runs_across_groups",
        || runs(sb, gui),
    );
}

fn entry(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let launches = service_log(sb).matches("launched ").count();
    gui.canvas_focus()?;
    click_text(gui, "◎ Agents")?;
    gui.wait("the canvas agents panel", 5, |t| t.contains("Canvas agents · 0") && t.contains("+ New canvas agent") && t.contains("GROUP PASSIVE SESSIONS"))?;
    click_text(gui, "+ New canvas agent")?;
    gui.wait("the templates", 5, |t| t.contains("New canvas-wide agent: template") && t.contains("PR coordinator") && t.contains("Cross-group digest") && t.contains("Custom"))?;
    gui.send("PR coordinator")?;
    gui.send("\r")?;
    gui.wait("the name prompt", 5, |t| t.contains("Name the PR coordinator canvas-wide agent"))?;
    gui.send(&format!("\x15{NAME}\r"))?;
    gui.wait("its panel", 10, |t| t.contains(&format!("◎ {NAME}")) && t.contains("canvas-wide agent ·") && t.contains("every group of this canvas"))?;
    let doc = wait_doc(&sb.canvas, "the agent in the canvas", |d| d.nodes.iter().any(|n| n.name == NAME && n.is_canvas_agent()) && d.canvas_frame().is_some())?;
    let node = doc.nodes.iter().find(|n| n.name == NAME).context("the agent")?.clone();
    let passive = node.passive.clone().context("passive")?;
    let frame = doc.canvas_frame().context("the Canvas agents frame")?;
    let canvas = doc.canvas_id.clone().context("the canvas id")?;
    anyhow::ensure!(passive.group == frame.id && passive.subscribe.iter().any(|s| s == "pr.*"), "in its frame, on pr.*: {passive:?}");
    let dir = canvas_agents::canvas_wide::canvas_dir(&sb.instance.home, &canvas);
    let rules = canvas_agents::canvas_wide::agent_paths(&dir, &passive.slug).rules;
    anyhow::ensure!(std::fs::read_to_string(&rules).unwrap_or_default().contains("PR coordinator"), "its rules from the template in {}", rules.display());
    let routing = wait_bus(sb, "routing.json", "the agent routed for this canvas", 10, |r| r["canvas"] == canvas.as_str() && spec(r, &passive.key)["canvas_wide"] == true)?;
    anyhow::ensure!(spec(&routing, &passive.key)["session_id"] == node.id.as_str(), "the routed agent is the box");
    // One agent behind every entry: its box, the panel's row, the sidebar row.
    let (b, _) = gui.find_box(NAME)?;
    anyhow::ensure!(b["prefix"].as_str().is_some_and(|p| p.contains('◎')) && (b["border"] == "Dashed" || b["selected"] == true), "a dashed ◎ box: {b}");
    let selected = |t: &Value| t["selection"].as_array().is_some_and(|s| s.len() == 1 && s[0] == node.id.as_str());
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    click_text(gui, "◎ Agents")?;
    gui.wait("the panel lists it", 5, |t| t.contains("Canvas agents · 1") && t.contains(&format!("{NAME}  ")))?;
    click_text(gui, &format!("{NAME}  "))?;
    gui.wait_tree("the panel's row selects its box", 5, selected)?;
    gui.wait("its own panel", 5, |t| t.contains("canvas-wide agent ·") && t.contains("Open chat"))?;
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    let at = gui.box_center(NAME)?;
    gui.click(at, &[])?;
    gui.wait_tree("a click on the box selects the same agent", 5, selected)?;
    let (x, y) = gui.driver.find_in(NAME, 0, 33).context("its sidebar row")?;
    gui.click((x as f64 + 0.5, y as f64 + 0.5), &[])?;
    gui.wait_tree("its sidebar row selects the same agent", 5, selected)?;
    // Viewing, even a double-click on the box, never starts it.
    gui.canvas_focus()?;
    let at = gui.box_center(NAME)?;
    gui.double_click(at)?;
    std::thread::sleep(Duration::from_millis(800));
    anyhow::ensure!(!service_log(sb).contains(&format!("launched {} ", node.id)) && service_log(sb).matches("launched ").count() == launches, "viewing the agent launched something");
    gui.wait("its panel after a double-click", 5, |t| t.contains(&format!("◎ {NAME}")) && t.contains("Open chat"))?;
    // A canvas skill (made as New skill… makes it, without opening an editor here), given with Skills….
    let request = canvas_model::rpc::Request::CreateCanvasSkill { canvas: canvas.clone(), name: SKILL.into(), description: "Parity: how PR feedback reaches its owner".into() };
    anyhow::ensure!(matches!(canvas_agents::backend::handle(&canvas_agents::backend::Ctx::new(&sb.instance), request), canvas_model::rpc::Response::Text(_)), "the skill was created");
    click_text(gui, "Skills…")?;
    gui.wait("the skills picker", 5, |t| t.contains(&format!("Skills of “{NAME}”")) && t.contains(&format!("○ {SKILL}")) && t.contains("+ New skill…"))?;
    gui.send(SKILL)?;
    gui.send("\r")?;
    gui.wait("the skill given", 5, |t| t.contains(&format!("✓ {SKILL}")))?;
    gui.key("Esc", &[])?;
    wait_doc(&sb.canvas, "the skill on the agent", |d| d.node(&node.id).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.skills == [SKILL]))?;
    wait_bus(sb, "routing.json", "the skill routed", 10, |r| spec(r, &passive.key)["skills"] == json!([SKILL]))?;
    gui.wait("its panel names the skill", 5, |t| t.contains(&format!("Skills     {SKILL}")))?;
    Ok(format!("◎ Agents → PR coordinator template → box in the frame (files under canvas-agents\\{canvas}); panel row, box and sidebar select {}; double-click launched nothing; {SKILL} given", &node.id[..8]))
}

fn runs(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let doc = sb.doc()?;
    let node = doc.nodes.iter().find(|n| n.name == NAME && n.is_canvas_agent()).context("the agent of row 40")?.clone();
    let key = node.passive.as_ref().map(|p| p.key.clone()).unwrap_or_default();
    let canvas = doc.canvas_id.clone().unwrap_or_default();
    let started = || service_log(sb).matches(&format!("passive {NAME} run started")).count();
    let post = |source: &str, pr: u32| -> Result<()> {
        let event = canvas_agents::bus::Event { kind: "pr.published".into(), source: source.into(), data: json!({ "pr": pr, "repo": "parity" }), ..Default::default() };
        canvas_agents::bus::post(&sb.instance.home, &event).map(|_| ())
    };
    // A session of the parity group (not the agent's frame) publishes a pull request: the agent runs.
    let source = sb.sessions.idle.clone();
    let before = started();
    post(&source, 4040)?;
    let deadline = Instant::now() + Duration::from_secs(40);
    while started() == before {
        anyhow::ensure!(Instant::now() < deadline, "no run after an event from another group:\n{}", service_log(sb).lines().rev().take(15).collect::<Vec<_>>().join("\n"));
        std::thread::sleep(Duration::from_millis(200));
    }
    let state = wait_bus(sb, "state.json", "the stand-in's report", 40, |s| s["passive"][&key]["headline"].as_str().is_some_and(|h| h.contains("fake run of pr.published")))?;
    let headline = state["passive"][&key]["headline"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(headline.contains(&format!("skills: {SKILL}")), "its skill reached the agent CLI: {headline}");
    anyhow::ensure!(service_log(sb).contains(&format!("terminal {}: copilot headless run", node.id)) && service_log(sb).contains("with its canvas skills"), "a headless run with its skills plugin");
    let dir = canvas_agents::canvas_wide::canvas_dir(&sb.instance.home, &canvas);
    let runs_log = std::fs::read_to_string(canvas_agents::canvas_wide::agent_paths(&dir, &node.passive.as_ref().map(|p| p.slug.clone()).unwrap_or_default()).dir.join("fake-runs.log")).unwrap_or_default();
    anyhow::ensure!(runs_log.contains(&format!("canvas-wide agent \"{NAME}\"")), "the run's prompt: {runs_log}");
    gui.wait_tree("its report on its box", 10, |t| box_line(t).contains("fake run of pr.published"))?;
    // Paused: an event of its canvas does not trigger it; a session no canvas routes never does.
    gui.canvas_focus()?;
    gui.click(gui.box_center(NAME)?, &[])?;
    gui.wait("its panel", 5, |t| t.contains("canvas-wide agent ·") && t.contains("Pause"))?;
    click_text(gui, "Pause")?;
    wait_bus(sb, "routing.json", "paused", 10, |r| spec(r, &key)["paused"] == true)?;
    let before = started();
    post(&source, 4041)?;
    post("not-on-this-canvas", 4042)?;
    std::thread::sleep(Duration::from_secs(12));
    let state = bus_file(sb, "state.json");
    anyhow::ensure!(started() == before && state["passive"][&key]["pending"].as_array().is_none_or(|p| p.is_empty()), "paused, it was triggered: {}", state["passive"][&key]);
    gui.wait_tree("paused on its box", 5, |t| box_line(t).contains("‖ paused"))?;
    click_text(gui, "Resume")?;
    wait_bus(sb, "routing.json", "resumed", 10, |r| spec(r, &key)["paused"] == false)?;
    // Run now, then Stop run: it ends as stopped, not failed.
    let before = started();
    click_text(gui, "Run now")?;
    wait_bus(sb, "state.json", "a run in progress", 15, |s| s["passive"][&key]["running"] == true)?;
    anyhow::ensure!(started() > before, "Run now started a run");
    gui.wait("the Stop run button", 5, |t| t.contains("Stop run"))?;
    click_text(gui, "Stop run")?;
    let state = wait_bus(sb, "state.json", "the run stopped", 15, |s| s["passive"][&key]["running"] == false && s["passive"][&key]["stopped_ms"].as_u64().unwrap_or(0) > 0)?;
    anyhow::ensure!(state["passive"][&key]["failures"] == 0, "a stopped run is no failure: {}", state["passive"][&key]);
    anyhow::ensure!(service_log(sb).contains(&format!("passive {NAME} run stopped by you")), "the service says so");
    // Disabled: no longer routed, no run, "off" on its box.
    click_text(gui, "Disable")?;
    wait_doc(&sb.canvas, "disabled", |d| d.node(&node.id).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.disabled))?;
    wait_bus(sb, "routing.json", "no longer routed", 10, |r| spec(r, &key).is_null())?;
    let before = started();
    post(&source, 4043)?;
    std::thread::sleep(Duration::from_secs(10));
    anyhow::ensure!(started() == before, "disabled, it ran");
    gui.wait_tree("off on its box", 5, |t| box_line(t).contains("○ off"))?;
    gui.canvas_focus()?;
    Ok(format!("pr.published from the parity group → run with {SKILL} (\"{headline}\"); paused and stranger events: no run; Stop → stopped, 0 failures; disabled: unrouted"))
}
