//! PR coordination in the service (WI-014): the registry of the routed
//! canvas's tracked pull requests (one writer: this service), requests from
//! `copilot-canvas pr …` and windows, read-only polls with the user's own
//! `gh` / `az`, the events they turn into for the canvas-wide agents, and
//! the follow-ups (dispatches) for owning sessions: queued while the owner
//! works, typed in when it is idle (the existing mailbox, never mid-turn),
//! blocked while it cannot get them, each change reported in the inbox.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Shared, lock, log};
use crate::bus::{BusState, Event, Mail, PassiveSpec, Routing, Target};
use crate::paths::now_ms;
use crate::prs_cli::{Answer, REQUEST, write_answer};
use canvas_model::prs::{self, Dispatch, DispatchRequest, DispatchState, Dispatched, MonitorStatus, Poll, PrRef, PrState, PrsView, Registry, Role, Tracked, TrackedPr};

/// Seconds between polls instead of the coordinators' minutes (tests).
const POLL_SECS_ENV: &str = "COPILOT_CANVAS_PR_POLL_SECS";
/// One `gh` / `az` command at most.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);
/// Pull requests polled per pass (the rest wait for the next second).
const PER_PASS: usize = 3;

/// The routed canvas's registry, held by the service, its only writer.
#[derive(Default)]
pub(super) struct Hub {
    canvas: String,
    registry: Registry,
    saved: String,
}

impl Hub {
    /// Holds `canvas`'s registry (the one held before is saved first).
    fn open(&mut self, home: &Path, canvas: &str) {
        if self.canvas == canvas {
            return;
        }
        self.save(home);
        self.canvas = canvas.to_owned();
        self.registry = if crate::doc::valid_canvas_id(canvas) { crate::prs_cli::load(home, canvas) } else { Registry::default() };
        self.saved = serde_json::to_string_pretty(&self.registry).unwrap_or_default();
    }

    /// Writes the registry when it changed (atomically).
    fn save(&mut self, home: &Path) {
        if !crate::doc::valid_canvas_id(&self.canvas) {
            return;
        }
        let text = serde_json::to_string_pretty(&self.registry).unwrap_or_default();
        if text != self.saved && crate::paths::write_atomic(&crate::prs_cli::registry_path(home, &self.canvas), text.as_bytes()).is_ok() {
            self.saved = text;
        }
    }
}

/// The canvas-wide agents with PR coordination that run (routed: enabled; not paused).
pub(super) fn coordinators(routing: &Routing) -> Vec<&PassiveSpec> {
    routing.passive.iter().filter(|p| p.canvas_wide && !p.paused && p.prs.is_some()).collect()
}

/// Milliseconds between polls of one pull request.
fn interval_ms(routing: &Routing) -> u64 {
    if let Some(secs) = std::env::var(POLL_SECS_ENV).ok().and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 0) {
        return secs * 1_000;
    }
    coordinators(routing).iter().filter_map(|p| p.prs.as_ref().map(|s| s.poll_minutes.max(1))).min().unwrap_or(prs::DEFAULT_POLL_MINUTES) as u64 * 60_000
}

/// What a follow-up may ask for: its coordinator's permissions (yours: the first coordinator's).
fn allow_of(routing: &Routing, from_key: &str) -> Vec<String> {
    let spec = routing.passive.iter().find(|p| p.key == from_key && p.prs.is_some()).or_else(|| coordinators(routing).into_iter().next());
    spec.and_then(|p| p.prs.as_ref()).map(|s| s.allow.clone()).unwrap_or_default()
}

/// Whether a session can get a follow-up now; `Err`: why it is blocked and what to do.
pub(super) fn reach(routing: &Routing, id: &str, running: bool, elsewhere: bool) -> Result<(), String> {
    if !routing.names.contains_key(id) || routing.passive_by_session(id).is_some() {
        return Err("the session is no longer on this canvas: reassign the follow-up or dismiss it".into());
    }
    if routing.no_messages.contains(&id.to_owned()) {
        return Err("the session does not take messages (Mute passive messages): unmute it, reassign or dismiss".into());
    }
    if running {
        return Ok(());
    }
    if elsewhere {
        return Err("the session is open elsewhere (another terminal or VS Code), where the canvas never types: reassign or dismiss, or close it there and resume it here".into());
    }
    Err("the session is not running: resume it (the follow-up is typed in once it is idle), reassign or dismiss".into())
}

pub(super) fn running(shared: &Shared, id: &str) -> bool {
    lock(&shared.terms).get(id).is_some_and(|t| t.alive.load(Ordering::Acquire))
}

pub(super) fn elsewhere(shared: &Shared, id: &str) -> bool {
    let native = shared.native_of(id);
    lock(&shared.sessions).get(&native).is_some_and(|s| s.live)
}

/// The name a request comes from: a session's or agent's, else `you`.
pub(super) fn who(routing: &Routing, source: &str) -> String {
    if source.is_empty() {
        return "you".into();
    }
    routing.passive_by_session(source).map(|p| p.name.clone()).or_else(|| routing.names.get(source).cloned()).unwrap_or_else(|| source.chars().take(8).collect())
}

/// A canvas-wide agent (or you, no source).
pub(super) fn wide(routing: &Routing, source: &str) -> bool {
    source.is_empty() || routing.passive_by_session(source).is_some_and(|p| p.canvas_wide)
}

/// Tracks a pull request a review session reviewed (review follow-ups):
/// only its end is watched, and only when no session owns it already.
pub(super) fn track_reviewed(shared: &Shared, routing: &Routing, pr: &PrRef, reviewer: &str, now: u64) {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return;
    }
    let mut hub = lock(&shared.prs);
    hub.open(&shared.instance.home, &routing.canvas);
    if hub.registry.get(&pr.key()).is_some_and(|p| !p.state.ended()) {
        return;
    }
    let name = routing.names.get(reviewer).cloned().unwrap_or_default();
    let tracked = TrackedPr::new(pr.clone(), Role::Reviewed, reviewer, &name, &routing.members.get(reviewer).cloned().unwrap_or_default(), "reviewed (review follow-ups)", now);
    if hub.registry.track(tracked, false).is_ok() {
        hub.save(&shared.instance.home);
    }
}

/// Keys of the routed canvas's pull requests that ended (merged, closed, abandoned).
pub(super) fn ended(shared: &Shared, routing: &Routing) -> Vec<String> {
    let hub = lock(&shared.prs);
    if hub.canvas != routing.canvas {
        return Vec::new();
    }
    hub.registry.prs.iter().filter(|p| p.state.ended()).map(|p| p.key.clone()).collect()
}

/// Handles a request (`copilot-canvas pr …`, or a window's button); answers
/// it for a waiting command. Returns events to route (`pr.tracked`).
pub(super) fn request(shared: &Shared, routing: &Routing, state: &mut BusState, event: &Event, now: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let op = event.data["op"].as_str().unwrap_or_default().to_owned();
    let answer = handle(shared, routing, state, event, now, &mut out).unwrap_or_else(|why| Answer { ok: false, message: why, state: String::new() });
    log(&shared.instance, &format!("pr {op} from {}: {}{}", who(routing, &event.source), if answer.ok { "" } else { "refused: " }, answer.message));
    write_answer(&shared.instance.home, event.data["request"].as_str().unwrap_or_default(), &answer);
    out
}

fn handle(shared: &Shared, routing: &Routing, state: &mut BusState, event: &Event, now: u64, out: &mut Vec<Event>) -> Result<Answer, String> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return Err("no canvas with an id is open in Copilot Canvas (open the canvas with an up-to-date client)".into());
    }
    let source = event.source.as_str();
    if !source.is_empty() && !routing.names.contains_key(source) && routing.passive_by_session(source).is_none() {
        return Err("this session is not on the canvas Copilot Canvas has open (open its canvas first)".into());
    }
    let home = &shared.instance.home;
    let data = &event.data;
    let text = |key: &str| data[key].as_str().unwrap_or_default().trim().to_owned();
    let pr_of = |required: bool| -> Result<Option<PrRef>, String> {
        let url = text("url");
        if url.is_empty() && !required {
            return Ok(None);
        }
        PrRef::parse(&url).map(Some)
    };
    let mut hub = lock(&shared.prs);
    hub.open(home, &routing.canvas);
    let monitored = !coordinators(routing).is_empty();
    let answer = match event.data["op"].as_str().unwrap_or_default() {
        "track" => {
            let pr = pr_of(true)?.expect("required");
            let named = text("session");
            let owner = if !named.is_empty() {
                match routing.resolve_from(&named, source, "")? {
                    Target::Session(id) => id,
                    Target::Passive(_) => return Err(format!("{named:?} is a passive session or canvas-wide agent: pull requests belong to working sessions")),
                }
            } else if routing.names.contains_key(source) && routing.passive_by_session(source).is_none() {
                source.to_owned()
            } else {
                return Err("name the session that owns the pull request with --session \"<name or id>\"".into());
            };
            let owner_name = routing.names.get(&owner).cloned().unwrap_or_default();
            let by = if source.is_empty() { "you".to_owned() } else if source == owner { "handed off by the session".to_owned() } else { format!("handed off by {}", who(routing, source)) };
            let mut tracked = TrackedPr::new(pr.clone(), Role::Owned, &owner, &owner_name, &routing.members.get(&owner).cloned().unwrap_or_default(), &by, now);
            tracked.title = prs::excerpt(&text("title"), 200);
            let outcome = hub.registry.track(tracked, wide(routing, source))?;
            if outcome != Tracked::Same {
                if let Some(t) = hub.registry.get_mut(&pr.key()) {
                    t.monitor.next_ms = 0;
                }
                out.push(tracked_event(routing, hub.registry.get(&pr.key()).expect("tracked"), &by, event.hop));
            }
            let what = if outcome == Tracked::Same { "Already tracking" } else { "Tracking" };
            let monitor = if monitored {
                format!("the canvas checks it every {} for review comments, failing checks and its end; the PR coordinator sends follow-ups to “{owner_name}”", short_interval(interval_ms(routing)))
            } else {
                "nothing monitors it until a canvas-wide agent has PR coordination (◎ Agents › + New canvas agent › PR coordinator)".to_owned()
            };
            Answer { ok: true, message: format!("{what} {} for “{owner_name}”: {monitor}.", pr.short()), state: "tracked".into() }
        }
        "untrack" => {
            let pr = pr_of(true)?.expect("required");
            let key = pr.key();
            let owner = hub.registry.get(&key).map(|p| p.owner.clone()).ok_or_else(|| format!("{} is not tracked on this canvas", pr.short()))?;
            if !wide(routing, source) && owner != source {
                return Err(format!("{} belongs to another session: only its owner, a canvas-wide agent or you untrack it", pr.short()));
            }
            let removed = hub.registry.untrack(&key).expect("present");
            let ids: Vec<&str> = removed.dispatches.iter().map(|d| d.id.as_str()).collect();
            state.mailbox.retain(|m| !ids.contains(&m.id.as_str()));
            Answer { ok: true, message: format!("Stopped tracking {}; its queued follow-ups were withdrawn.", pr.short()), state: "untracked".into() }
        }
        "check" => {
            let keys: Vec<String> = match pr_of(false)? {
                Some(pr) => {
                    hub.registry.get(&pr.key()).ok_or_else(|| format!("{} is not tracked on this canvas", pr.short()))?;
                    vec![pr.key()]
                }
                None => hub.registry.prs.iter().filter(|p| !p.state.ended()).map(|p| p.key.clone()).collect(),
            };
            for key in &keys {
                if let Some(p) = hub.registry.get_mut(key).filter(|p| !p.state.ended()) {
                    p.monitor.next_ms = 0;
                }
            }
            let note = if monitored { "" } else { " (nothing monitors them until a canvas-wide agent has PR coordination)" };
            Answer { ok: true, message: format!("Checking {} pull request(s) now{note}.", keys.len()), state: "check".into() }
        }
        "dispatch" => {
            let agent = routing.passive_by_session(source).filter(|p| p.canvas_wide && p.prs.is_some());
            if !source.is_empty() && agent.is_none() {
                return Err("only a canvas-wide agent with PR coordination (or you) dispatches follow-ups; a session hands its pull request off with copilot-canvas pr track".into());
            }
            let pr = pr_of(true)?.expect("required");
            let request = DispatchRequest { key: text("key"), event: text("event"), text: data["text"].as_str().unwrap_or_default().to_owned(), from: who(routing, source), from_key: agent.map(|a| a.key.clone()).unwrap_or_default() };
            match hub.registry.dispatch(&pr.key(), request, now)? {
                Dispatched::Duplicate(id, st) => Answer { ok: true, message: format!("Already dispatched for this key ({id}, {}); nothing new was sent.", st.word()), state: st.word().into() },
                Dispatched::New(id) => {
                    let changed = route(shared, routing, state, &mut hub, now);
                    report_changes(shared, routing, &changed, now);
                    let (pr, at) = hub.registry.find_dispatch(&id).ok_or("the follow-up went missing")?;
                    let d = &pr.dispatches[at];
                    let message = match d.state {
                        DispatchState::Queued => format!("Follow-up {id} queued for “{}”: it is typed in when that session is idle (never mid-turn); Reports shows queued, then delivered.", d.to_name),
                        DispatchState::Blocked => format!("Follow-up {id} blocked: {} (◎ Agents › Pull requests).", d.reason),
                        other => format!("Follow-up {id} {}.", other.word()),
                    };
                    Answer { ok: true, message, state: d.state.word().into() }
                }
            }
        }
        op @ ("dismiss" | "retry" | "reassign") => {
            if !wide(routing, source) {
                return Err("only you (the canvas) and canvas-wide agents change follow-ups".into());
            }
            let id = text("dispatch");
            let to = match op {
                "reassign" => match routing.resolve_from(&text("to"), source, "")? {
                    Target::Session(id) => Some(id),
                    Target::Passive(_) => return Err("follow-ups go to working sessions".into()),
                },
                _ => None,
            };
            let (pr, at) = hub.registry.find_dispatch(&id).ok_or_else(|| format!("no follow-up {id:?}"))?;
            let d = &mut pr.dispatches[at];
            state.mailbox.retain(|m| m.id != d.id);
            match (op, to) {
                ("dismiss", _) => d.state = DispatchState::Dismissed,
                ("reassign", Some(to)) => {
                    d.to_name = routing.names.get(&to).cloned().unwrap_or_default();
                    d.to = to;
                    d.state = DispatchState::Intended;
                }
                _ => d.state = DispatchState::Intended,
            }
            d.reason.clear();
            d.updated_ms = now;
            let message = format!("Follow-up {id} {}.", if op == "dismiss" { "dismissed" } else { "on its way again" });
            let changed = route(shared, routing, state, &mut hub, now);
            report_changes(shared, routing, &changed, now);
            Answer { ok: true, message, state: op.into() }
        }
        other => return Err(format!("unknown pr request {other:?}")),
    };
    hub.save(home);
    Ok(answer)
}

fn short_interval(ms: u64) -> String {
    if ms >= 60_000 { format!("{} min", ms / 60_000) } else { format!("{} s", ms / 1_000) }
}

/// `pr.tracked` for the canvas-wide agents (from the owner, so its group's
/// passive sessions see it too).
fn tracked_event(routing: &Routing, pr: &TrackedPr, by: &str, hop: u32) -> Event {
    Event {
        kind: "pr.tracked".into(),
        source: pr.owner.clone(),
        canvas: routing.canvas.clone(),
        data: serde_json::json!({ "pr": pr.key, "url": pr.url, "short": pr.pr.short(), "title": pr.title, "owner": pr.owner, "owner_name": pr.owner_name, "by": by }),
        hop,
        ..Default::default()
    }
}

/// A pull request a session created (seen in its history, or emitted) is
/// tracked for it when a coordinator tracks automatically.
pub(super) fn observe(shared: &Shared, routing: &Routing, event: &Event, now: u64) -> Option<Event> {
    if event.kind != "pr.created" || !coordinators(routing).iter().any(|p| p.prs.as_ref().is_some_and(|s| s.auto_track)) {
        return None;
    }
    if !routing.names.contains_key(&event.source) || routing.passive_by_session(&event.source).is_some() {
        return None;
    }
    let url = event.data["url"].as_str().unwrap_or_default();
    let Ok(pr) = PrRef::parse(url) else {
        log(&shared.instance, &format!("pr.created from {} has no pull request link ({url:?}): the session hands it off with copilot-canvas pr track", who(routing, &event.source)));
        return None;
    };
    let mut hub = lock(&shared.prs);
    hub.open(&shared.instance.home, &routing.canvas);
    let owner_name = routing.names.get(&event.source).cloned().unwrap_or_default();
    let by = "detected (pr.created in its history)";
    let tracked = TrackedPr::new(pr.clone(), Role::Owned, &event.source, &owner_name, &routing.members.get(&event.source).cloned().unwrap_or_default(), by, now);
    let outcome = hub.registry.track(tracked, false).ok()?;
    let out = (outcome != Tracked::Same).then(|| tracked_event(routing, hub.registry.get(&pr.key()).expect("tracked"), by, event.hop));
    hub.save(&shared.instance.home);
    if out.is_some() {
        log(&shared.instance, &format!("tracking {} for {owner_name} (detected)", pr.short()));
    }
    out
}

/// Moves follow-ups along: intended or blocked ones whose owner can get them
/// go into its mailbox (queued); queued ones whose owner stopped, left or
/// was muted are blocked; queued ones whose message is gone failed. Answers
/// the changed (pull request, dispatch) pairs.
fn route(shared: &Shared, routing: &Routing, state: &mut BusState, hub: &mut Hub, now: u64) -> Vec<(TrackedPr, Dispatch)> {
    let mut changed = Vec::new();
    for pr in hub.registry.prs.iter_mut() {
        let snapshot = TrackedPr { dispatches: Vec::new(), seen: Default::default(), ..pr.clone() };
        for d in pr.dispatches.iter_mut().filter(|d| d.state.open()) {
            let is_running = running(shared, &d.to);
            let reach = reach(routing, &d.to, is_running, !is_running && elsewhere(shared, &d.to));
            let queued = state.mailbox.iter().any(|m| m.id == d.id);
            let (next, reason) = match (d.state, reach) {
                (DispatchState::Queued, Ok(())) if queued => continue,
                (DispatchState::Queued, Ok(())) => (DispatchState::Failed, if now.saturating_sub(d.created_ms) >= 86_000_000 { "not delivered within a day".to_owned() } else { "its queued message was discarded".to_owned() }),
                (DispatchState::Intended | DispatchState::Blocked, Ok(())) => {
                    let allow = allow_of(routing, &d.from_key);
                    state.mailbox.retain(|m| m.id != d.id);
                    state.mailbox.push(Mail { id: d.id.clone(), to: d.to.clone(), from: if d.from.is_empty() { "PR coordinator".into() } else { d.from.clone() }, text: prs::dispatch_text(&snapshot, d, &allow), ms: now });
                    (DispatchState::Queued, String::new())
                }
                (DispatchState::Blocked, Err(why)) => {
                    d.reason = why;
                    continue;
                }
                (_, Err(why)) => {
                    state.mailbox.retain(|m| m.id != d.id);
                    (DispatchState::Blocked, why)
                }
                _ => continue,
            };
            d.state = next;
            d.reason = reason;
            d.updated_ms = now;
            changed.push((snapshot.clone(), d.clone()));
        }
    }
    changed
}

/// Each pass of the bus loop: the routed canvas's registry, its follow-ups
/// moved along, ended pull requests pruned.
pub(super) fn tick(shared: &Shared, routing: &Routing, state: &mut BusState, now: u64) {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return;
    }
    let changed = {
        let mut hub = lock(&shared.prs);
        hub.open(&shared.instance.home, &routing.canvas);
        let changed = route(shared, routing, state, &mut hub, now);
        hub.registry.prune(now);
        hub.save(&shared.instance.home);
        changed
    };
    report_changes(shared, routing, &changed, now);
}

/// A message the bus loop typed in: a follow-up is delivered.
pub(super) fn delivered(shared: &Shared, routing: &Routing, mail: &Mail, now: u64) {
    if !mail.id.starts_with("d-") {
        return;
    }
    let changed = {
        let mut hub = lock(&shared.prs);
        let Some((pr, at)) = hub.registry.find_dispatch(&mail.id) else { return };
        let d = &mut pr.dispatches[at];
        d.state = DispatchState::Delivered;
        d.reason.clear();
        d.updated_ms = now;
        d.delivered_ms = now;
        let d = d.clone();
        let pair = (TrackedPr { dispatches: Vec::new(), seen: Default::default(), ..pr.clone() }, d);
        hub.save(&shared.instance.home);
        pair
    };
    report_changes(shared, routing, &[changed], now);
}

/// The canvas-wide agent a report is on behalf of (its box and key).
fn on_behalf<'a>(routing: &'a Routing, from_key: &str) -> Option<&'a PassiveSpec> {
    routing.passive.iter().find(|p| p.key == from_key).or_else(|| coordinators(routing).into_iter().next())
}

/// Reports follow-up changes in the inbox (one thread per pull request):
/// queued, delivered, blocked (warn, with what to do), failed.
fn report_changes(shared: &Shared, routing: &Routing, changed: &[(TrackedPr, Dispatch)], now: u64) {
    for (pr, d) in changed {
        log(&shared.instance, &format!("follow-up {} for {} on {}: {}{}", d.id, d.to_name, pr.pr.short(), d.state.word(), if d.reason.is_empty() { String::new() } else { format!(" ({})", d.reason) }));
        let Some(kind) = d.state.report_kind() else { continue };
        let topic = if d.topic.is_empty() { prs::topic(&d.event, pr) } else { d.topic.clone() };
        let title = match d.state {
            DispatchState::Queued => format!("PR {} received {topic}; follow-up for “{}” queued", pr.pr.short(), d.to_name),
            DispatchState::Delivered => format!("PR {}: follow-up delivered to “{}”", pr.pr.short(), d.to_name),
            DispatchState::Blocked => format!("PR {}: follow-up for “{}” blocked", pr.pr.short(), d.to_name),
            _ => format!("PR {}: follow-up for “{}” failed", pr.pr.short(), d.to_name),
        };
        let state = match d.state {
            DispatchState::Queued => "Queued: it is typed into the session when the session is idle (never mid-turn). Delivery is confirmed by a delivered update in this thread.".to_owned(),
            DispatchState::Delivered => "Delivered: typed into the session.".to_owned(),
            DispatchState::Blocked => format!("Blocked: {}. ◎ Agents › Pull requests has Resume owner, Reassign and Dismiss.", d.reason),
            _ => format!("Failed: {}. ◎ Agents › Pull requests can retry or reassign it.", d.reason),
        };
        let body = format!("{state}\n\nFollow-up from {}: {}\n\n{} · {}", if d.from.is_empty() { "the PR coordinator" } else { &d.from }, d.text, pr.title, pr.url);
        let agent = on_behalf(routing, &d.from_key);
        let notice = crate::reports::Notice {
            title,
            body,
            kind: kind.into(),
            source: agent.map(|a| a.session_id.clone()).unwrap_or_default(),
            passive: agent.map(|a| a.key.clone()).unwrap_or_default(),
            source_name: "PR monitor".into(),
            session: d.to.clone(),
            pr: pr.url.clone(),
            thread: prs::thread_key(&pr.key),
            ..Default::default()
        };
        if let Err(error) = crate::reports::notice(&shared.instance.home, notice, now) {
            log(&shared.instance, &format!("cannot report follow-up {}: {error:#}", d.id));
        }
    }
}

/// A report about a pull request's monitor (its end, or why it cannot be read).
fn report_monitor(shared: &Shared, routing: &Routing, pr: &TrackedPr, kind: &str, title: String, body: String, now: u64) {
    let agent = coordinators(routing).into_iter().next();
    let notice = crate::reports::Notice {
        title,
        body,
        kind: kind.into(),
        source: agent.map(|a| a.session_id.clone()).unwrap_or_default(),
        passive: agent.map(|a| a.key.clone()).unwrap_or_default(),
        source_name: "PR monitor".into(),
        session: pr.owner.clone(),
        pr: pr.url.clone(),
        thread: prs::thread_key(&pr.key),
        ..Default::default()
    };
    if let Err(error) = crate::reports::notice(&shared.instance.home, notice, now) {
        log(&shared.instance, &format!("cannot report on {}: {error:#}", pr.pr.short()));
    }
}

/// The pull requests for clients (`None`: the canvas tracks none and has no coordinator).
pub(super) fn view(shared: &Shared, routing: &Routing) -> Option<PrsView> {
    if !crate::doc::valid_canvas_id(&routing.canvas) {
        return None;
    }
    let hub = lock(&shared.prs);
    let any_settings = routing.passive.iter().any(|p| p.canvas_wide && p.prs.is_some());
    if hub.canvas != routing.canvas || (hub.registry.prs.is_empty() && !any_settings) {
        return None;
    }
    let running = coordinators(routing);
    let paused = if !running.is_empty() {
        String::new()
    } else if any_settings {
        "its PR coordinator is paused: resume it to monitor them".into()
    } else {
        "no canvas-wide agent has PR coordination: ◎ Agents › + New canvas agent › PR coordinator".into()
    };
    let names = running.iter().map(|p| p.name.clone()).collect();
    let allow = running.first().and_then(|p| p.prs.as_ref()).map(|s| s.allow.clone()).unwrap_or_default();
    Some(PrsView::of(&routing.canvas, &hub.registry, names, paused, allow))
}

// ---------- the monitor: read-only polls with gh / az ----------

/// Why a poll did not work.
#[derive(Debug, PartialEq)]
pub(super) enum PollError {
    /// The CLI is missing, not signed in or lacks its extension: try much later.
    Unavailable(String),
    /// It ran and failed: back off.
    Failed(String),
}

/// A CLI: `env` names it, else the first of `names` on PATH.
fn program(env: &str, names: &[&str]) -> Option<PathBuf> {
    std::env::var_os(env).map(PathBuf::from).filter(|p| p.is_file()).or_else(|| names.iter().find_map(|n| super::find_on_path(n)))
}

/// Sorts a CLI failure: signing in and missing extensions make it unavailable.
pub(super) fn classify(cli: &str, error: &str) -> PollError {
    let lower = error.to_ascii_lowercase();
    let sign_in = ["auth login", "gh_token", "not logged", "authentication", "bad credentials", "az login", "tf400813", "unauthorized", "http 401", "re-authenticate"];
    if sign_in.iter().any(|w| lower.contains(w)) {
        let how = if cli == "gh" { "run gh auth login" } else { "run az login (and az devops configure)" };
        return PollError::Unavailable(format!("{cli} is not signed in on the service's machine: {how}"));
    }
    if cli == "az" && (lower.contains("misspelled or not recognized") || lower.contains("extension")) {
        return PollError::Unavailable("the Azure CLI has no azure-devops extension: az extension add --name azure-devops".into());
    }
    PollError::Failed(prs::excerpt(error, 300))
}

fn run_cli(cli: &str, program: &Path, args: &[&str]) -> Result<String, PollError> {
    // Read-only commands; no prompts, no update checks, no extension installs.
    let env: &[(&str, &str)] = if cli == "gh" {
        &[("GH_PROMPT_DISABLED", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("NO_COLOR", "1")]
    } else {
        &[("AZURE_CORE_ONLY_SHOW_ERRORS", "1"), ("AZURE_CORE_NO_COLOR", "1"), ("AZURE_EXTENSION_USE_DYNAMIC_INSTALL", "no")]
    };
    crate::agents::config::run(&program.display().to_string(), args, env, CLI_TIMEOUT).map_err(|e| classify(cli, &format!("{e:#}")))
}

/// Reads a pull request with its host's CLI (read-only).
pub(super) fn poll(pr: &PrRef) -> Result<Poll, PollError> {
    match pr.host {
        prs::Host::GitHub => {
            let gh = program("COPILOT_CANVAS_GH", &["gh.exe"]).ok_or_else(|| PollError::Unavailable("gh (GitHub CLI) is not installed on the service's machine".into()))?;
            let url = pr.url();
            let view = run_cli("gh", &gh, &["pr", "view", &url, "--json", "number,url,title,state,isDraft,headRefOid,reviewDecision,author,comments,reviews,statusCheckRollup"])?;
            let path = format!("repos/{}/{}/pulls/{}/comments?per_page=100", pr.owner, pr.repo, pr.number);
            let mut args = vec!["api", "--paginate", path.as_str()];
            if pr.server != "github.com" {
                args.extend(["--hostname", pr.server.as_str()]);
            }
            let inline = run_cli("gh", &gh, &args)?;
            prs::parse_github(&view, &inline).map_err(PollError::Failed)
        }
        prs::Host::Ado => {
            let az = program("COPILOT_CANVAS_AZ", &["az.cmd", "az.exe", "az.bat"]).ok_or_else(|| PollError::Unavailable("az (Azure CLI with the azure-devops extension) is not installed on the service's machine".into()))?;
            let id = pr.number.to_string();
            let common = ["--org", pr.server.as_str(), "--output", "json", "--only-show-errors"];
            let show = run_cli("az", &az, &[&["repos", "pr", "show", "--id", &id][..], &common[..]].concat())?;
            let parsed: serde_json::Value = serde_json::from_str(crate::copilot_cfg::json_payload(&show).unwrap_or("{}")).unwrap_or_default();
            let project = parsed["repository"]["project"]["id"].as_str().map(str::to_owned).unwrap_or_else(|| pr.project.clone());
            let repo = parsed["repository"]["id"].as_str().map(str::to_owned).unwrap_or_else(|| pr.repo.clone());
            let route = [format!("project={project}"), format!("repositoryId={repo}"), format!("pullRequestId={id}")];
            let threads = run_cli(
                "az",
                &az,
                &[&["devops", "invoke", "--area", "git", "--resource", "pullRequestThreads", "--route-parameters", route[0].as_str(), route[1].as_str(), route[2].as_str(), "--api-version", "7.1", "--http-method", "GET"][..], &common[..]].concat(),
            )?;
            let policies = run_cli("az", &az, &[&["repos", "pr", "policy", "list", "--id", &id][..], &common[..]].concat())?;
            prs::parse_ado(pr, &show, &threads, &policies).map_err(PollError::Failed)
        }
    }
}

/// Applies a poll's outcome under the lock; answers the events to send.
fn apply(shared: &Shared, routing: &Routing, key: &str, result: Result<Poll, PollError>, interval: u64, now: u64) -> Vec<Event> {
    let mut hub = lock(&shared.prs);
    if hub.canvas != routing.canvas {
        return Vec::new();
    }
    let Some(pr) = hub.registry.get_mut(key) else { return Vec::new() };
    let mut events = Vec::new();
    let mut report: Option<(&str, String, String)> = None;
    match result {
        Ok(poll) => {
            let changes = pr.apply(&poll, now);
            pr.monitor.next_ms = now + interval;
            for change in &changes {
                let (kind, data) = change.event(pr);
                events.push(Event { kind, source: pr.owner.clone(), canvas: routing.canvas.clone(), data, ..Default::default() });
                if let prs::Change::Ended(state) = change {
                    let what = if *state == PrState::Merged { "merged" } else { state.word() };
                    report = Some(("ok", format!("PR {} {what}: monitoring stopped", pr.pr.short()), format!("{} · {}\nIt is no longer polled; its follow-ups stay listed in ◎ Agents › Pull requests.", pr.title, pr.url)));
                }
            }
            if !changes.is_empty() {
                log(&shared.instance, &format!("{}: {}", pr.pr.short(), changes.iter().map(|c| c.summary()).collect::<Vec<_>>().join("; ")));
            }
        }
        Err(PollError::Unavailable(reason)) => {
            let first = pr.monitor.status != MonitorStatus::Unavailable || pr.monitor.reason != prs::excerpt(&reason, 300);
            pr.poll_failed(true, &reason, interval, now);
            if first {
                log(&shared.instance, &format!("{} cannot be monitored: {reason}", pr.pr.short()));
                report = Some(("warn", format!("PR {} cannot be monitored", pr.pr.short()), format!("{reason}.\nThe canvas tries again every 30 minutes, or at once with Check now (◎ Agents › Pull requests).\n{}", pr.url)));
            }
        }
        Err(PollError::Failed(reason)) => {
            pr.poll_failed(false, &reason, interval, now);
            log(&shared.instance, &format!("{} poll failed ({} in a row): {reason}", pr.pr.short(), pr.monitor.failures));
            if pr.monitor.failures == 3 {
                report = Some(("warn", format!("PR {} cannot be read", pr.pr.short()), format!("Its last 3 polls failed: {reason}.\nThe canvas keeps trying, less often.\n{}", pr.url)));
            }
        }
    }
    let pr = pr.clone();
    hub.save(&shared.instance.home);
    drop(hub);
    if let Some((kind, title, body)) = report {
        report_monitor(shared, routing, &pr, kind, title, body, now);
    }
    events
}

/// The monitor thread: polls due pull requests of the routed canvas while a
/// PR coordinator runs, and sends what changed to the bus.
pub(super) fn run(shared: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let routing = lock(&shared.routing).clone();
        if !crate::doc::valid_canvas_id(&routing.canvas) || coordinators(&routing).is_empty() {
            continue;
        }
        let interval = interval_ms(&routing);
        let due: Vec<TrackedPr> = {
            let mut hub = lock(&shared.prs);
            hub.open(&shared.instance.home, &routing.canvas);
            hub.registry.due(now_ms()).into_iter().take(PER_PASS).filter_map(|k| hub.registry.get(&k).cloned()).collect()
        };
        for pr in due {
            if shared.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            let result = poll(&pr.pr);
            for event in apply(&shared, &routing, &pr.key, result, interval, now_ms()) {
                let _ = shared.bus_tx.send(event);
            }
        }
    }
}

/// Is this bus event a request for this module?
pub(super) fn is_request(event: &Event) -> bool {
    event.kind == REQUEST
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{PassiveSpec, RouteGroup};

    fn routing() -> Routing {
        Routing {
            canvas: "c1".into(),
            groups: vec![RouteGroup { id: "g1".into(), name: "Pay".into(), folder: None }],
            members: [("s1".to_owned(), "g1".to_owned())].into(),
            names: [("s1".to_owned(), "Fix flaky test".to_owned()), ("s2".to_owned(), "Muted".to_owned()), ("w".to_owned(), "PR coordinator".to_owned())].into(),
            no_messages: vec!["s2".into()],
            passive: vec![PassiveSpec { key: "kw".into(), session_id: "w".into(), name: "PR coordinator".into(), canvas_wide: true, prs: Some(Default::default()), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn follow_ups_reach_running_sessions_and_say_why_others_are_blocked() {
        let r = routing();
        assert_eq!(reach(&r, "s1", true, false), Ok(()));
        assert!(reach(&r, "s1", false, false).unwrap_err().contains("not running: resume it"));
        assert!(reach(&r, "s1", false, true).unwrap_err().contains("open elsewhere"));
        assert!(reach(&r, "s2", true, false).unwrap_err().contains("does not take messages"));
        assert!(reach(&r, "gone", true, false).unwrap_err().contains("no longer on this canvas"));
        assert!(reach(&r, "w", true, false).is_err(), "an agent is no owner");
    }

    #[test]
    fn only_coordinators_that_run_monitor_and_cli_failures_are_sorted() {
        let mut r = routing();
        assert_eq!(coordinators(&r).len(), 1);
        assert_eq!(interval_ms(&r), 5 * 60_000);
        r.passive[0].paused = true;
        assert!(coordinators(&r).is_empty(), "paused: no polls");
        r.passive[0].paused = false;
        r.passive[0].prs = None;
        assert!(coordinators(&r).is_empty(), "no PR coordination");
        assert!(matches!(classify("gh", "Alternatively, populate the GH_TOKEN environment variable"), PollError::Unavailable(why) if why.contains("gh auth login")));
        assert!(matches!(classify("az", "ERROR: Please run 'az login' to setup account."), PollError::Unavailable(why) if why.contains("az login")));
        assert!(matches!(classify("az", "ERROR: 'repos' is misspelled or not recognized by the system."), PollError::Unavailable(why) if why.contains("azure-devops")));
        assert!(matches!(classify("gh", "GraphQL: Could not resolve to a PullRequest"), PollError::Failed(_)));
        assert_eq!(allow_of(&routing(), "kw"), Vec::<String>::new(), "read-only by default");
    }
}
