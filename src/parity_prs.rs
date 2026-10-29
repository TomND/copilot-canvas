//! The parity suite's checks of pull request coordination (row 42, WI-014)
//! with the stand-in agent and the stand-in `gh`: a session hands a pull
//! request off (`copilot-canvas pr track`, as the canvas-prs skill teaches),
//! the canvas polls it (the first poll is the baseline), three new comments
//! become one `pr.comment.new`, the PR coordinator (the stand-in following
//! the skill) dispatches one follow-up, which waits (queued) while its owner
//! is typed into and is typed in once the owner is idle (delivered); Reports
//! shows queued, then delivered, in the pull request's thread; a merged pull
//! request is never polled again; the panel lists it all.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{Gui, Row, Sandbox, check, create_from_dialog};
use crate::fake_gh::{FakeCheck, Fixture, calls, write_fixture};
use crate::selftest::wait_doc;
use canvas_agents::prs::{DispatchState, MonitorStatus, PrRef, PrState, Registry};

const COORDINATOR: &str = "Parity PRs";
const OWNER: &str = "Parity PR owner";
const URL: &str = "https://github.com/parity/app/pull/42";

/// Runs `copilot-canvas ARGS` as the canvas session `session` would (its
/// environment); answers its output.
pub(super) fn cli(sb: &Sandbox, session: &str, args: &[&str]) -> Result<String> {
    let mut command = std::process::Command::new(&sb.exe);
    command.args(args).args(["--home", &sb.instance.home.display().to_string()]).env_remove("COPILOT_CANVAS_SESSION");
    if !session.is_empty() {
        command.env("COPILOT_CANVAS_SESSION", session);
    }
    let output = command.output()?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    anyhow::ensure!(output.status.success(), "copilot-canvas {} failed: {text}", args.join(" "));
    Ok(text)
}

fn wait_registry(sb: &Sandbox, canvas: &str, what: &str, secs: u64, ok: impl Fn(&Registry) -> bool) -> Result<Registry> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let registry = canvas_agents::prs_cli::load(&sb.instance.home, canvas);
        if ok(&registry) {
            return Ok(registry);
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}: {}", serde_json::to_string(&registry).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The inbox's reports of a thread: (kind, title), oldest first.
pub(super) fn thread_reports(sb: &Sandbox, thread: &str) -> Vec<(String, String)> {
    let mut found: Vec<(u64, String, String)> = std::fs::read_dir(sb.instance.home.join("reports").join("items"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read(e.path().join("meta.json")).ok())
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|r| r["thread"] == thread)
        .map(|r| (r["created_ms"].as_u64().unwrap_or(0), r["kind"].as_str().unwrap_or_default().to_owned(), r["title"].as_str().unwrap_or_default().to_owned()))
        .collect();
    found.sort();
    found.into_iter().map(|(_, kind, title)| (kind, title)).collect()
}

fn click_text(gui: &Gui, text: &str) -> Result<()> {
    let at = gui.find(text).with_context(|| format!("{text:?} on screen\n{}", gui.screen()))?;
    gui.click(at, &[])
}

/// Makes a canvas-wide agent from a template through ◎ Agents; answers its box id.
pub(super) fn new_agent(sb: &Sandbox, gui: &mut Gui, template: &str, name: &str) -> Result<String> {
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    click_text(gui, "◎ Agents")?;
    gui.wait("the canvas agents panel", 5, |t| t.contains("+ New canvas agent"))?;
    click_text(gui, "+ New canvas agent")?;
    gui.wait("the templates", 5, |t| t.contains("New canvas-wide agent: template") && t.contains(template))?;
    gui.send(template)?;
    gui.send("\r")?;
    gui.wait("the name prompt", 5, |t| t.contains(&format!("Name the {template} canvas-wide agent")))?;
    gui.send(&format!("\x15{name}\r"))?;
    gui.wait("its panel", 10, |t| t.contains(&format!("◎ {name}")) && t.contains("canvas-wide agent ·"))?;
    let doc = wait_doc(&sb.canvas, "the agent in the canvas", |d| d.nodes.iter().any(|n| n.name == name && n.is_canvas_agent()))?;
    Ok(doc.nodes.iter().find(|n| n.name == name).map(|n| n.id.clone()).unwrap_or_default())
}

pub(super) fn rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(
        rows,
        "42",
        "New: PR coordination: a session hands a PR off; the canvas polls it read-only; new comments → one follow-up, queued while its owner works, delivered when idle; Reports; merged stops polling",
        "pr_handoff_follow_up_and_end",
        || coordination(sb, gui),
    );
}

fn coordination(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    let launches_before = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count();
    // The owner: a new session (the stand-in agent) from the dialog.
    gui.canvas_focus()?;
    gui.send("n")?;
    gui.wait("the new-session dialog", 5, |t| t.contains("New session"))?;
    gui.send(OWNER)?;
    let owner = create_from_dialog(gui, sb, OWNER)?;
    gui.wait("its stand-in terminal", 25, |t| t.contains("Fake Copilot"))?;
    // The coordinator: the PR coordinator template turns PR coordination on.
    let agent = new_agent(sb, gui, "PR coordinator", COORDINATOR)?;
    let doc = wait_doc(&sb.canvas, "its PR coordination", |d| d.node(&agent).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.prs.is_some()))?;
    let canvas = doc.canvas_id.clone().context("the canvas id")?;
    gui.wait("its PR settings in its panel", 5, |t| t.contains("PRs        on: every 5 min"))?;
    // A pull request with an old comment and a passing build.
    let pr = PrRef::parse(URL).map_err(|e| anyhow::anyhow!(e))?;
    let key = pr.key();
    let mut fixture = Fixture::open("Parity: retry uploads", "parity-dev");
    fixture.comment(1, "ana", "an old comment");
    fixture.checks.push(FakeCheck { name: "build".into(), conclusion: "SUCCESS".into() });
    write_fixture(&sb.prs, &pr, &fixture)?;
    // The owning session hands it off, as the canvas-prs skill teaches.
    let out = cli(sb, &owner, &["pr", "track", URL])?;
    anyhow::ensure!(out.contains(&format!("Tracking parity/app#42 for “{OWNER}”")) && out.contains("every 2 s"), "{out}");
    wait_registry(sb, &canvas, "the first poll (the baseline)", 30, |r| r.get(&key).is_some_and(|p| p.monitor.polls >= 1 && p.monitor.status == MonitorStatus::Ok && p.comments == 1 && p.new_comments == 0))?;
    // Three new comments while the owner is busy (typed into): the follow-up waits.
    gui.open(OWNER, "Fake Copilot")?;
    fixture.comment(3, "ben", "please handle the 429 retry");
    write_fixture(&sb.prs, &pr, &fixture)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        gui.send("x")?;
        let registry = canvas_agents::prs_cli::load(&sb.instance.home, &canvas);
        let dispatches = registry.get(&key).map(|p| p.dispatches.clone()).unwrap_or_default();
        if dispatches.iter().any(|d| d.state == DispatchState::Queued) {
            break;
        }
        anyhow::ensure!(!dispatches.iter().any(|d| d.state == DispatchState::Delivered), "delivered while its owner was typed into: {dispatches:?}");
        anyhow::ensure!(Instant::now() < deadline, "no follow-up was queued: {}", serde_json::to_string(&registry).unwrap_or_default());
        std::thread::sleep(Duration::from_secs(2));
    }
    let thread = canvas_agents::prs::thread_key(&key);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !thread_reports(sb, &thread).iter().any(|(kind, title)| kind == "queued" && title.contains("received 3 new review comment(s)")) {
        anyhow::ensure!(Instant::now() < deadline, "no queued report: {:?}", thread_reports(sb, &thread));
        std::thread::sleep(Duration::from_millis(250));
    }
    // Idle 20 s after the last key: typed in.
    let registry = wait_registry(sb, &canvas, "the follow-up delivered", 60, |r| r.get(&key).is_some_and(|p| p.dispatches.iter().any(|d| d.state == DispatchState::Delivered)))?;
    let dispatch = registry.get(&key).and_then(|p| p.dispatches.first()).cloned().context("the dispatch")?;
    anyhow::ensure!(dispatch.to == owner && dispatch.event == "pr.comment.new" && dispatch.from == COORDINATOR, "{dispatch:?}");
    gui.wait_tile("the follow-up in the owner's terminal", 10, |t| t.contains(&format!("[From {COORDINATOR} via Copilot Canvas] PR parity/app#42")))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !thread_reports(sb, &thread).iter().any(|(kind, _)| kind == "delivered") {
        anyhow::ensure!(Instant::now() < deadline, "no delivered report: {:?}", thread_reports(sb, &thread));
        std::thread::sleep(Duration::from_millis(250));
    }
    // More polls, no second follow-up for the same comments.
    std::thread::sleep(Duration::from_secs(6));
    let registry = canvas_agents::prs_cli::load(&sb.instance.home, &canvas);
    anyhow::ensure!(registry.get(&key).is_some_and(|p| p.dispatches.len() == 1), "one follow-up: {:?}", registry.get(&key).map(|p| &p.dispatches));
    // Merged: monitoring stops for good.
    fixture.state = "MERGED".into();
    write_fixture(&sb.prs, &pr, &fixture)?;
    wait_registry(sb, &canvas, "merged", 20, |r| r.get(&key).is_some_and(|p| p.state == PrState::Merged && p.monitor.status == MonitorStatus::Stopped))?;
    let polled = |calls: &[String]| calls.iter().filter(|l| l.contains("parity/app")).count();
    let before = polled(&calls(&sb.prs));
    std::thread::sleep(Duration::from_secs(6));
    let after = calls(&sb.prs);
    anyhow::ensure!(polled(&after) == before, "polled after it merged ({before} → {})", polled(&after));
    anyhow::ensure!(!after.iter().any(|l| l.starts_with("REFUSED")), "the canvas tried a write: {after:?}");
    let reports = thread_reports(sb, &thread);
    anyhow::ensure!(reports.iter().any(|(kind, title)| kind == "ok" && title.contains("merged: monitoring stopped")), "{reports:?}");
    // The panel: ◎ Agents › Pull requests.
    gui.canvas_focus()?;
    gui.key("Esc", &[])?;
    click_text(gui, "◎ Agents")?;
    gui.wait("the PR section", 10, |t| t.contains("PULL REQUESTS ·") && t.contains("Tracked pull requests"))?;
    click_text(gui, "Tracked pull requests")?;
    gui.wait("the pull requests panel", 10, |t| t.contains("Pull requests ·") && t.contains("parity/app#42") && t.contains("· merged"))?;
    // The agent is turned off for the rows after this one.
    gui.canvas_focus()?;
    gui.click(gui.box_center(COORDINATOR)?, &[])?;
    gui.wait("its panel", 5, |t| t.contains(&format!("◎ {COORDINATOR}")) && t.contains("Disable"))?;
    click_text(gui, "Disable")?;
    wait_doc(&sb.canvas, "the coordinator disabled", |d| d.node(&agent).and_then(|n| n.passive.as_ref()).is_some_and(|p| p.disabled))?;
    gui.canvas_focus()?;
    let launches = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count();
    Ok(format!(
        "tracked by its owner; baseline poll; 3 comments → 1 follow-up, queued while typed into, delivered when idle; Reports {:?}; merged → no more polls ({} gh calls, none a write); {} launch(es) (the owner and the coordinator's runs)",
        reports.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        before,
        launches - launches_before
    ))
}
