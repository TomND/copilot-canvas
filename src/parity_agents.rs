//! The parity suite's checks of canvas agents (rows 40, 40b and 53): the
//! top bar's ◎ Agents panel, the Canvas agents frame, the box and the
//! sidebar row are one agent, made from the *New canvas agent* dialog
//! (*Start from…* a template); viewing it never starts it; a canvas skill is
//! given to it from its settings; an event from a session of another group
//! runs it (the stand-in agent reports what it got, its skill included);
//! paused, no event triggers it, and neither does a session no canvas
//! routes; Stop ends a run without a failure; disabled, it is no longer
//! routed. Row 53 talks to one like a session: *Create & start* opens its
//! chat with the stand-in, a typed message reaches it, a double-click on its
//! box resumes the same conversation, its instructions are edited into
//! rules.md, a schedule shows its next run, and with no triggers it never
//! runs on its own.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check};
use crate::selftest::{Ui, wait_doc};

const NAME: &str = "Parity coordinator";
const SKILL: &str = "parity-skill";
const HELPER: &str = "Parity helper";

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

fn wait_log(sb: &Sandbox, what: &str, secs: u64, ok: impl Fn(&str) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok(&service_log(sb)) {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what} in the service log:\n{}", service_log(sb).lines().rev().take(15).collect::<Vec<_>>().join("\n"));
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
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

/// A settings row of an agent's panel: its label as drawn (padded to the value column).
pub(super) fn setting(label: &str) -> String {
    format!("{label:<13}")
}

/// The *New canvas agent* dialog, from the top bar's ◎ Agents (its panel
/// first: *+ New canvas agent* tops it).
pub(super) fn open_dialog(gui: &mut Gui) -> Result<()> {
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    click_text(gui, " Agents ")?;
    gui.wait("the canvas agents panel", 5, |t| t.contains("Canvas agents ·") && t.contains("+ New canvas agent") && t.contains("GROUP PASSIVE SESSIONS"))?;
    click_text(gui, "+ New canvas agent")?;
    gui.wait("the new canvas agent dialog", 5, |t| t.contains("New canvas agent") && t.contains("Start from") && t.contains("Only when you talk to it"))
}

/// Makes a canvas agent from the dialog: `template` (*Start from…*, `None`:
/// blank) and `name`; `start`: *Create & start*, else *Create*. Answers its box id.
pub(super) fn create_agent(sb: &Sandbox, gui: &mut Gui, template: Option<&str>, name: &str, start: bool) -> Result<String> {
    open_dialog(gui)?;
    if let Some(template) = template {
        click_text(gui, "Blank  ▾")?;
        gui.wait("the templates", 5, |t| t.contains("Start from (prefills the new agent)") && t.contains(template))?;
        gui.send(template)?;
        gui.send("\r")?;
        gui.wait("the dialog prefilled", 5, |t| t.contains(&format!("{template}  ▾")))?;
        // From Start from to its name (Ctrl+U clears the template's).
        gui.send("\t")?;
    }
    gui.send(&format!("\x15{name}"))?;
    gui.wait("its name", 5, |t| t.contains(name))?;
    click_text(gui, if start { "Create & start" } else { "Create    Cancel" })?;
    let doc = wait_doc(&sb.canvas, "the agent in the canvas", |d| d.nodes.iter().any(|n| n.name == name && n.is_canvas_agent()))?;
    Ok(doc.nodes.iter().find(|n| n.name == name).map(|n| n.id.clone()).unwrap_or_default())
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(
        rows,
        "40",
        "New: canvas agents: ◎ Agents (top bar), the Canvas agents frame, the box and the sidebar are one agent; made from the New canvas agent dialog (Start from a template); canvas skills; viewing never starts it",
        "canvas_agents_one_agent_every_entry",
        || entry(sb, gui),
    );
    check(
        rows,
        "40b",
        "New: a canvas agent runs on an event from any group (stand-in agent, its skill delivered); paused or for another canvas: no run; Stop, Disable",
        "canvas_agent_runs_across_groups",
        || runs(sb, gui),
    );
    check(
        rows,
        "53",
        "New (WI-028): a canvas agent is a session that manages the canvas: Create & start opens its chat (stand-in), typing reaches it, a double-click resumes the same conversation; Instructions write rules.md; a schedule shows its next run; with no triggers it never runs on its own",
        "canvas_agent_chat_settings_and_triggers",
        || {
            let result = talk(sb, gui);
            // Whatever happened: no chat left open, the agent off for the rows after this one.
            let _ = tidy_helper(sb, gui);
            result
        },
    );
}

fn entry(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let launches = service_log(sb).matches("launched ").count();
    gui.canvas_focus()?;
    let id = create_agent(sb, gui, Some("PR coordinator"), NAME, false)?;
    gui.wait("its panel", 10, |t| t.contains(&format!("◎ {NAME}")) && t.contains("canvas agent ·") && t.contains("every group of this canvas"))?;
    let doc = wait_doc(&sb.canvas, "the agent in its frame", |d| d.nodes.iter().any(|n| n.name == NAME && n.is_canvas_agent()) && d.canvas_frame().is_some())?;
    let node = doc.node(&id).context("the agent")?.clone();
    let passive = node.passive.clone().context("passive")?;
    let frame = doc.canvas_frame().context("the Canvas agents frame")?;
    let canvas = doc.canvas_id.clone().context("the canvas id")?;
    anyhow::ensure!(passive.group == frame.id && passive.subscribe.iter().any(|s| s == "pr.*") && passive.prs.is_some(), "in its frame, on pr.*, PR coordination on: {passive:?}");
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
    click_text(gui, " Agents ")?;
    gui.wait("the panel lists it", 5, |t| t.contains("Canvas agents · 1") && t.contains(&format!("◎ {NAME}  ")))?;
    // Its row in the panel ("◎ name  state"; the sidebar's row has its state first: a second
    // click there would be a double-click, the explicit start).
    click_text(gui, &format!("◎ {NAME}  "))?;
    gui.wait_tree("the panel's row selects its box", 5, selected)?;
    gui.wait("its own panel", 5, |t| t.contains("canvas agent ·") && t.contains("▸ Chat") && t.contains("SETTINGS"))?;
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    let at = gui.box_center(NAME)?;
    gui.click(at, &[])?;
    gui.wait_tree("a click on the box selects the same agent", 5, selected)?;
    let (x, y) = gui.driver.find_in(NAME, 0, 33).context("its sidebar row")?;
    gui.click((x as f64 + 0.5, y as f64 + 0.5), &[])?;
    gui.wait_tree("its sidebar row selects the same agent", 5, selected)?;
    // Viewing it never starts it.
    std::thread::sleep(Duration::from_millis(800));
    anyhow::ensure!(!service_log(sb).contains(&format!("launched {} ", node.id)) && service_log(sb).matches("launched ").count() == launches, "viewing the agent launched something");
    gui.wait("its panel after the views", 5, |t| t.contains(&format!("◎ {NAME}")) && t.contains("▸ Chat"))?;
    // A canvas skill (made as New skill… makes it, without opening an editor here), given from its Skills row.
    let request = canvas_model::rpc::Request::CreateCanvasSkill { canvas: canvas.clone(), name: SKILL.into(), description: "Parity: how PR feedback reaches its owner".into() };
    anyhow::ensure!(matches!(canvas_agents::backend::handle(&canvas_agents::backend::Ctx::new(&sb.instance), request), canvas_model::rpc::Response::Text(_)), "the skill was created");
    click_text(gui, &format!("{}none", setting("Skills")))?;
    gui.wait("the skills picker", 5, |t| t.contains(&format!("Skills of “{NAME}”")) && t.contains(&format!("○ {SKILL}")) && t.contains("+ New skill…"))?;
    gui.send(SKILL)?;
    gui.send("\r")?;
    gui.wait("the skill given", 5, |t| t.contains(&format!("✓ {SKILL}")))?;
    gui.key("Esc", &[])?;
    wait_doc(&sb.canvas, "the skill on the agent", |d| d.node(&node.id).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.skills == [SKILL]))?;
    wait_bus(sb, "routing.json", "the skill routed", 10, |r| spec(r, &passive.key)["skills"] == json!([SKILL]))?;
    gui.wait("its settings name the skill", 5, |t| t.contains(&format!("{}{SKILL}", setting("Skills"))))?;
    Ok(format!("◎ Agents → + New canvas agent → Start from PR coordinator → Create: box in the frame (files under canvas-agents\\{canvas}); panel row, box and sidebar select {}; viewing launched nothing; {SKILL} given", &node.id[..8]))
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
    gui.wait("its panel", 5, |t| t.contains("canvas agent ·") && t.contains("Pause"))?;
    click_text(gui, " Pause ")?;
    wait_bus(sb, "routing.json", "paused", 10, |r| spec(r, &key)["paused"] == true)?;
    let before = started();
    post(&source, 4041)?;
    post("not-on-this-canvas", 4042)?;
    std::thread::sleep(Duration::from_secs(12));
    let state = bus_file(sb, "state.json");
    anyhow::ensure!(started() == before && state["passive"][&key]["pending"].as_array().is_none_or(|p| p.is_empty()), "paused, it was triggered: {}", state["passive"][&key]);
    gui.wait_tree("paused on its box", 5, |t| box_line(t).contains("‖ paused"))?;
    click_text(gui, " Resume ")?;
    wait_bus(sb, "routing.json", "resumed", 10, |r| spec(r, &key)["paused"] == false)?;
    // Run now, then Stop run: it ends as stopped, not failed.
    let before = started();
    click_text(gui, " Run now ")?;
    wait_bus(sb, "state.json", "a run in progress", 15, |s| s["passive"][&key]["running"] == true)?;
    anyhow::ensure!(started() > before, "Run now started a run");
    gui.wait("the Stop run button", 5, |t| t.contains("Stop run"))?;
    click_text(gui, " Stop run ")?;
    let state = wait_bus(sb, "state.json", "the run stopped", 15, |s| s["passive"][&key]["running"] == false && s["passive"][&key]["stopped_ms"].as_u64().unwrap_or(0) > 0)?;
    anyhow::ensure!(state["passive"][&key]["failures"] == 0, "a stopped run is no failure: {}", state["passive"][&key]);
    anyhow::ensure!(service_log(sb).contains(&format!("passive {NAME} run stopped by you")), "the service says so");
    // Disabled (its Enabled row): no longer routed, no run, "off" on its box.
    click_text(gui, &format!("{}yes", setting("Enabled")))?;
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

/// The helper agent's box, shown and selected (its details on the right).
fn show_helper(gui: &mut Gui) -> Result<()> {
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    click_text(gui, " Agents ")?;
    let row = format!("◎ {HELPER}  ");
    gui.wait("its row in the agents panel", 5, |t| t.contains(&row))?;
    click_text(gui, &row)?;
    gui.wait("its details", 5, |t| t.contains(&format!("◎ {HELPER}")) && t.contains("SETTINGS"))
}

/// Stops the helper's chat from its details (Stop run) and waits until its
/// terminal is gone (its details offer Stop run only while it runs).
fn stop_chat(gui: &mut Gui) -> Result<()> {
    show_helper(gui)?;
    gui.wait("Stop run while its chat runs", 5, |t| t.contains("Stop run"))?;
    click_text(gui, " Stop run ")?;
    gui.wait("its chat ended", 15, |t| !t.contains("Stop run"))
}

fn talk(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let started = || service_log(sb).matches(&format!("passive {HELPER} run started")).count();
    // A blank agent from the dialog: its instructions, a first message, Create & start.
    open_dialog(gui)?;
    gui.send(HELPER)?;
    // Name → Agent → Model → Instructions.
    gui.send("\t\t\t")?;
    gui.send("Answer the user's questions about this canvas.")?;
    click_text(gui, "optional: what to ask it first")?;
    gui.send("hello from the dialog")?;
    let launched_before = service_log(sb).matches("launched ").count();
    click_text(gui, "Create & start")?;
    let doc = wait_doc(&sb.canvas, "the helper in the canvas", |d| d.nodes.iter().any(|n| n.name == HELPER && n.is_canvas_agent()))?;
    let node = doc.nodes.iter().find(|n| n.name == HELPER).context("the helper")?.clone();
    let id = node.id.clone();
    let passive = node.passive.clone().context("passive")?;
    anyhow::ensure!(!passive.has_triggers() && passive.groups.is_empty(), "the dialog's defaults: no triggers, every group: {passive:?}");
    let canvas = doc.canvas_id.clone().context("the canvas id")?;
    let paths = canvas_agents::canvas_wide::agent_paths(&canvas_agents::canvas_wide::canvas_dir(&sb.instance.home, &canvas), &passive.slug);
    anyhow::ensure!(std::fs::read_to_string(&paths.rules).unwrap_or_default() == "Answer the user's questions about this canvas.\n", "its instructions are its rules.md: {:?}", std::fs::read_to_string(&paths.rules));
    // Create & start: its chat with the stand-in, in a tile with the keyboard.
    gui.wait_tree("its chat with the keyboard", 25, |t| t["focus"] == "Terminal" && t["focused_term"] == id.as_str())?;
    gui.wait_tile("the stand-in in its chat", 25, |t| t.contains("Fake Copilot"))?;
    wait_log(sb, "its chat launched", 10, |log| log.contains(&format!("terminal {id}: copilot new session")))?;
    anyhow::ensure!(service_log(sb).matches("launched ").count() > launched_before, "Create & start launched its chat");
    gui.send("parity says hi")?;
    gui.wait_tile("a typed message in its chat", 10, |t| t.contains("parity says hi"))?;
    // Its chat ends; a double-click on its box resumes the same conversation (its box id).
    gui.chord(']', &["ctrl"])?;
    stop_chat(gui)?;
    gui.canvas_focus()?;
    if gui.box_center(HELPER).is_err() {
        gui.send("f")?;
    }
    let at = gui.box_center(HELPER)?;
    gui.double_click(at)?;
    gui.wait_tree("its chat again with the keyboard", 25, |t| t["focus"] == "Terminal" && t["focused_term"] == id.as_str())?;
    wait_log(sb, "its conversation resumed", 15, |log| log.contains(&format!("terminal {id}: copilot resume")))?;
    gui.wait_tile("the stand-in again", 25, |t| t.contains("Fake Copilot"))?;
    gui.chord(']', &["ctrl"])?;
    stop_chat(gui)?;
    // Its instructions from its settings: the editor writes rules.md.
    click_text(gui, &setting("Instructions"))?;
    gui.wait("its instructions in the editor", 10, |t| t.contains(&format!("Instructions of “{HELPER}”")) && t.contains("Answer the user's questions"))?;
    gui.key("End", &["ctrl"])?;
    gui.send(" Keep answers short.")?;
    gui.chord('s', &["ctrl"])?;
    gui.wait("the editor closed", 10, |t| !t.contains(&format!("Instructions of “{HELPER}”")))?;
    let rules = std::fs::read_to_string(&paths.rules).unwrap_or_default();
    anyhow::ensure!(rules.contains("Keep answers short."), "rules.md written: {rules:?}");
    // With no triggers it never runs on its own: an event from a parity session wakes nothing.
    let before = started();
    let event = canvas_agents::bus::Event { kind: "pr.published".into(), source: sb.sessions.idle.clone(), data: json!({ "pr": 5353 }), ..Default::default() };
    canvas_agents::bus::post(&sb.instance.home, &event)?;
    std::thread::sleep(Duration::from_secs(10));
    let key = passive.key.clone();
    let state = bus_file(sb, "state.json");
    anyhow::ensure!(started() == before && state["passive"][&key]["pending"].as_array().is_none_or(|p| p.is_empty()), "with no triggers it ran: {}", state["passive"][&key]);
    gui.wait("only when you talk to it", 5, |t| t.contains("Next run   when you talk to it"))?;
    // A schedule from its Wakes on row: every 1h; its next run shows.
    click_text(gui, &format!("{}Only when you talk to it", setting("Wakes on")))?;
    gui.wait("what wakes it", 5, |t| t.contains(&format!("Wakes “{HELPER}” on")) && t.contains("Every … off"))?;
    gui.send("Every")?;
    gui.send("\r")?;
    gui.wait("the interval prompt", 5, |t| t.contains("Wakes every"))?;
    gui.send("1h\r")?;
    gui.wait("the picker again", 5, |t| t.contains("Every … 1h"))?;
    gui.key("Esc", &[])?;
    wait_doc(&sb.canvas, "its schedule", |d| d.node(&id).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.every_minutes == 60))?;
    wait_bus(sb, "routing.json", "its schedule routed", 10, |r| spec(r, &key)["every_minutes"] == 60)?;
    gui.wait("its next run", 15, |t| t.contains("Next run   in ") && t.contains(&format!("{}every 1h", setting("Wakes on"))))?;
    anyhow::ensure!(started() == before, "a schedule an hour away ran it");
    Ok(format!("Create & start → its chat (stand-in, new session) took typing; double-click → resume of {}; rules.md edited; no run without triggers (10 s, an event posted); every 1h → next run shown", &id[..8]))
}

/// Leaves the helper stopped and disabled, the canvas focused.
fn tidy_helper(sb: &Sandbox, gui: &mut Gui) -> Result<()> {
    gui.canvas_focus()?;
    let doc = sb.doc()?;
    let Some(node) = doc.nodes.iter().find(|n| n.name == HELPER).cloned() else { return Ok(()) };
    show_helper(gui)?;
    if gui.screen().contains("Stop run") {
        click_text(gui, " Stop run ")?;
        gui.wait("its chat stopped", 15, |t| !t.contains("Stop run"))?;
    }
    if node.passive.as_ref().is_some_and(|p| !p.disabled) {
        click_text(gui, &format!("{}yes", setting("Enabled")))?;
        wait_doc(&sb.canvas, "the helper disabled", |d| d.node(&node.id).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.disabled))?;
    }
    gui.canvas_focus()
}
