//! `copilot-canvas pr track|untrack|check|dispatch|list` (WI-014): working
//! sessions hand the pull requests they publish to the canvas, a canvas-wide
//! PR coordinator sends their owners follow-ups. Each command checks what it
//! can, posts a request into the service's bus inbox (so nothing is lost
//! while the service restarts) and, when the service runs, waits for its
//! answer (`<home>/bus/answers/<request>.json`). The service owns the
//! registry (`<home>/canvas-agents/<canvas id>/prs/registry.json`): the
//! commands only read it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::bus::{self, Event};
use canvas_model::prs::{DispatchState, PrRef, Registry};

/// The bus event of a request (handled by the service, never routed to agents).
pub const REQUEST: &str = "canvas.pr";
/// How long a command waits for the service's answer.
const WAIT: Duration = Duration::from_secs(10);

/// The service's answer to a request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub ok: bool,
    pub message: String,
    /// A dispatch's state, `tracked`, …
    #[serde(default)]
    pub state: String,
}

pub fn answers_dir(home: &Path) -> PathBuf {
    home.join("bus").join("answers")
}

/// The service answers a request (atomically; older answers nobody took go after an hour).
pub fn write_answer(home: &Path, request: &str, answer: &Answer) {
    if request.is_empty() || !request.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return;
    }
    let dir = answers_dir(home);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3600)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let _ = crate::paths::write_atomic(&dir.join(format!("{request}.json")), &serde_json::to_vec(answer).unwrap_or_default());
}

fn session() -> String {
    std::env::var("COPILOT_CANVAS_SESSION").unwrap_or_default()
}

fn hop() -> u32 {
    std::env::var("COPILOT_CANVAS_HOP").ok().and_then(|h| h.parse().ok()).unwrap_or(0)
}

/// Posts a request for the service (`kind`: [`REQUEST`] or review
/// follow-ups') and waits for its answer while the service runs (`None`: it
/// does not run, or did not answer in time; the request stays in the inbox).
pub fn ask(home: &Path, kind: &str, mut data: serde_json::Value) -> Result<Option<Answer>> {
    let request = uuid::Uuid::new_v4().to_string();
    data["request"] = request.clone().into();
    let event = Event { kind: kind.to_owned(), source: session(), data, hop: hop(), ..Default::default() };
    bus::post(home, &event)?;
    if !crate::reports::service_alive(home) {
        return Ok(None);
    }
    let path = answers_dir(home).join(format!("{request}.json"));
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&path) {
            let _ = std::fs::remove_file(&path);
            return Ok(Some(serde_json::from_slice(&bytes).context("unreadable answer from the service")?));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// Prints an answer (a refusal is an error: the command exits non-zero).
fn answered(answer: Option<Answer>, pending: &str) -> Result<()> {
    match answer {
        Some(answer) if answer.ok => {
            println!("{}", answer.message);
            Ok(())
        }
        Some(answer) => bail!("Copilot Canvas refused: {}", answer.message),
        None => {
            println!("{pending}");
            Ok(())
        }
    }
}

fn parse(url: &str) -> Result<PrRef> {
    PrRef::parse(url).map_err(|e| anyhow::anyhow!(e))
}

/// `pr track URL [--session NAME|ID] [--title TEXT]`.
pub fn track(home: &Path, url: &str, owner: Option<String>, title: Option<String>) -> Result<()> {
    let pr = parse(url)?;
    if session().is_empty() && owner.as_deref().is_none_or(|o| o.trim().is_empty()) {
        bail!("this is not a Copilot Canvas session: name the owning session with --session \"<name or id>\"");
    }
    let data = serde_json::json!({ "op": "track", "url": pr.url(), "session": owner.unwrap_or_default(), "title": title.unwrap_or_default() });
    answered(ask(home, REQUEST, data)?, &format!("Hand-off of {} recorded; the Copilot Canvas service is not running (or busy): it is tracked when the service takes it (check with copilot-canvas pr list).", pr.short()))
}

/// `pr untrack URL`.
pub fn untrack(home: &Path, url: &str) -> Result<()> {
    let pr = parse(url)?;
    answered(ask(home, REQUEST, serde_json::json!({ "op": "untrack", "url": pr.url() }))?, &format!("Untracking {} was handed to Copilot Canvas (the service is not running or busy).", pr.short()))
}

/// `pr check [URL]`: poll now (one pull request, or every active one).
pub fn check(home: &Path, url: Option<&str>) -> Result<()> {
    let url = url.map(parse).transpose()?.map(|p| p.url()).unwrap_or_default();
    answered(ask(home, REQUEST, serde_json::json!({ "op": "check", "url": url }))?, "Check requested; the Copilot Canvas service is not running (or busy).")
}

/// `pr dispatch URL [--key KEY] [--event TYPE] TEXT`: a follow-up for the
/// pull request's owning session (canvas-wide PR coordinators and you).
pub fn dispatch(home: &Path, url: &str, key: Option<String>, event: Option<String>, text: &str) -> Result<()> {
    let pr = parse(url)?;
    if text.trim().is_empty() {
        bail!("usage: copilot-canvas pr dispatch <PR URL> --key <dispatch_key> \"<what the owning session should do>\"");
    }
    let data = serde_json::json!({ "op": "dispatch", "url": pr.url(), "key": key.unwrap_or_default(), "event": event.unwrap_or_default(), "text": text });
    answered(
        ask(home, REQUEST, data)?,
        &format!("Follow-up for {} intended, not queued yet: the Copilot Canvas service is not running (or busy); it is queued for the owning session when the service takes it.", pr.short()),
    )
}

/// The routed canvas's id (the canvas a window last opened).
fn routed_canvas(home: &Path) -> String {
    crate::cli::routing(home).canvas
}

pub fn registry_path(home: &Path, canvas: &str) -> PathBuf {
    crate::canvas_wide::canvas_dir(home, canvas).join("prs").join("registry.json")
}

/// The registry as stored (read only; empty when missing).
pub fn load(home: &Path, canvas: &str) -> Registry {
    std::fs::read(registry_path(home, canvas)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// `pr list [--json]`.
pub fn list(home: &Path, json: bool) -> Result<String> {
    let canvas = routed_canvas(home);
    if !crate::doc::valid_canvas_id(&canvas) {
        bail!("no canvas with an id is open in Copilot Canvas (open it with an up-to-date client)");
    }
    let registry = load(home, &canvas);
    if json {
        return Ok(serde_json::to_string_pretty(&registry)?);
    }
    let routing = crate::cli::routing(home);
    let coordinators: Vec<String> = routing.passive.iter().filter(|p| p.canvas_wide && p.prs.is_some() && !p.paused).map(|p| p.name.clone()).collect();
    let now = crate::paths::now_ms();
    let active = registry.prs.iter().filter(|p| !p.state.ended()).count();
    let mut out = vec![format!(
        "Copilot Canvas pull requests of canvas {canvas}: {} tracked, {active} active · {}",
        registry.prs.len(),
        if coordinators.is_empty() { "not monitored: no canvas-wide agent with PR coordination is enabled".to_owned() } else { format!("monitored for {}", coordinators.join(", ")) }
    )];
    let age = |ms: u64| if ms == 0 { "never".to_owned() } else if ms <= now { format!("{} ago", crate::paths::format_age(ms, now)) } else { format!("in {}", crate::paths::format_age(now, ms)) };
    for pr in &registry.prs {
        let mark = if pr.state.ended() { "✓" } else if pr.needs_you() { "⚠" } else { "●" };
        let who = if pr.owner_name.is_empty() { &pr.owner } else { &pr.owner_name };
        let role = if pr.role == canvas_model::prs::Role::Reviewed { "reviewed by" } else { "owner" };
        out.push(format!("  {mark} {}  {}  “{}”  {role} {who}", pr.pr.short(), pr.state.word(), pr.title));
        out.push(format!("      {}", pr.url));
        let monitor = if pr.state.ended() { "monitoring stopped".to_owned() } else { format!("{:?} · checked {} · next {}", pr.monitor.status, age(pr.monitor.last_ms), age(pr.monitor.next_ms)).to_lowercase() };
        out.push(format!("      {monitor}{}{}", if pr.monitor.reason.is_empty() { String::new() } else { format!(" · {}", pr.monitor.reason) }, if pr.new_comments > 0 { format!(" · {} new comment(s)", pr.new_comments) } else { String::new() }));
        for d in pr.dispatches.iter().rev().take(5) {
            let glyph = match d.state {
                DispatchState::Delivered => "✓",
                DispatchState::Queued | DispatchState::Intended => "◌",
                DispatchState::Blocked => "⚠",
                DispatchState::Failed => "✗",
                DispatchState::Dismissed => "·",
            };
            out.push(format!("      {glyph} {} {}  → {}  {}{}", d.state.word(), age(d.updated_ms.max(d.created_ms)), d.to_name, crate::bus::mail_text(&crate::bus::Mail { from: d.from.clone(), text: d.text.chars().take(80).collect(), ..Default::default() }), if d.reason.is_empty() { String::new() } else { format!(" ({})", d.reason) }));
        }
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_checked_then_posted_for_the_service() {
        let home = std::env::temp_dir().join(format!("cc-prs-cli-{}", uuid::Uuid::new_v4()));
        assert!(track(&home, "https://github.com/o/r/issues/1", Some("x".into()), None).unwrap_err().to_string().contains("not a pull request link"));
        assert!(dispatch(&home, "o/r#1", None, None, "  ").unwrap_err().to_string().contains("usage"));
        // No service runs here: the request waits in the inbox, the command says so.
        track(&home, "https://github.com/o/r/pull/1", Some("Fix flaky test".into()), Some("Fix it".into())).unwrap();
        dispatch(&home, "o/r#1", Some("comments:c:9".into()), Some("pr.comment.new".into()), "Address the 3 new comments").unwrap();
        let events: Vec<Event> = std::fs::read_dir(bus::inbox_dir(&home)).unwrap().flatten().map(|f| serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap()).collect();
        assert_eq!(events.len(), 2);
        let track = events.iter().find(|e| e.data["op"] == "track").unwrap();
        assert_eq!((track.kind.as_str(), track.data["url"].as_str(), track.data["session"].as_str()), (REQUEST, Some("https://github.com/o/r/pull/1"), Some("Fix flaky test")));
        assert!(track.data["request"].as_str().is_some_and(|r| r.len() == 36));
        let dispatch = events.iter().find(|e| e.data["op"] == "dispatch").unwrap();
        assert_eq!((dispatch.data["key"].as_str(), dispatch.data["text"].as_str()), (Some("comments:c:9"), Some("Address the 3 new comments")));
        // Answers: written atomically, only for plain request ids.
        write_answer(&home, "../evil", &Answer::default());
        write_answer(&home, "abc-1", &Answer { ok: true, message: "Tracking".into(), state: "tracked".into() });
        assert!(answers_dir(&home).join("abc-1.json").is_file() && std::fs::read_dir(answers_dir(&home)).unwrap().count() == 1);
        let _ = std::fs::remove_dir_all(home);
    }
}
