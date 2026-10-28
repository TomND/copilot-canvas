//! Runs the event bus: routes events to passive sessions, runs them
//! (`copilot --resume <id> -p …` in a service terminal, or typed into the
//! session when you have it open), delivers messages to idle sessions, keeps
//! schedules, and tells clients what is going on.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::Receiver;

use super::{Shared, lock, log};
use crate::agents::AgentKind;
use crate::bus::{self, BusState, Event, Mail, PassiveSpec, Routing, Target};
use crate::paths::now_ms;
use crate::protocol::{Activity, AgentLaunch, Input, Key, LaunchKind, ServiceMsg};

pub fn bus_dir(home: &Path) -> PathBuf {
    home.join("bus")
}

pub fn load_routing(home: &Path) -> Routing {
    std::fs::read_to_string(bus_dir(home).join("routing.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn load_state(home: &Path) -> BusState {
    let mut state: BusState = std::fs::read_to_string(bus_dir(home).join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    // Runs do not survive a service restart; their events are tried again.
    for passive in state.passive.values_mut() {
        if passive.running {
            passive.running = false;
            let inflight = std::mem::take(&mut passive.inflight);
            passive.pending.splice(0..0, inflight);
            passive.first_pending_ms = now_ms();
        }
    }
    state
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&temp, bytes).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

pub fn save_routing(home: &Path, routing: &Routing) {
    if let Ok(bytes) = serde_json::to_vec_pretty(routing) {
        write_atomic(&bus_dir(home).join("routing.json"), &bytes);
    }
}

fn append_log(home: &Path, event: &Event) {
    let path = bus_dir(home).join("log.jsonl");
    if std::fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
    }
    if let (Ok(mut file), Ok(line)) = (std::fs::OpenOptions::new().create(true).append(true).open(&path), serde_json::to_string(event)) {
        let _ = writeln!(file, "{line}");
    }
}

/// Idle enough to receive typed input: not working, not waiting on you, not
/// mid-progress, and you have not typed into it in the last 20 seconds.
fn idle(shared: &Shared, session: &str) -> bool {
    let Some(term) = lock(&shared.terms).get(session).cloned() else { return false };
    if !term.alive.load(Ordering::Acquire) {
        return false;
    }
    let info = term.info();
    if info.progress_seen && matches!(info.progress, 1 | 3) {
        return false;
    }
    if lock(&shared.input_at).get(session).is_some_and(|t| now_ms().saturating_sub(*t) < 20_000) {
        return false;
    }
    let native = shared.native_of(session);
    match lock(&shared.sessions).get(&native) {
        Some(summary) => summary.pending == 0 && summary.activity != Activity::Working,
        None => true,
    }
}

/// Types text into a live terminal and submits it.
fn type_in(shared: &Shared, session: &str, text: &str) -> bool {
    let Some(term) = lock(&shared.terms).get(session).cloned() else { return false };
    let ok = term.send_input(&Input::Paste(text.to_owned())).is_ok() && term.send_input(&Input::Key { key: Key::Enter, mods: 0 }).is_ok();
    let _ = shared.pump.try_send(());
    ok
}

/// First request and latest summary of a session, from its history.
fn digest(events: &Path) -> (String, String) {
    let clip = |s: &str, n: usize| {
        let s = s.replace(['\r', '\n'], " ");
        if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
    };
    let mut first = String::new();
    if let Ok(file) = std::fs::File::open(events) {
        for line in BufReader::new(file).lines().take(600).map_while(Result::ok) {
            if line.contains("\"type\":\"user.message\"")
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            {
                first = clip(v["data"]["content"].as_str().unwrap_or_default(), 400);
                break;
            }
        }
    }
    let mut last = String::new();
    if let Ok(mut file) = std::fs::File::open(events) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(512 * 1024);
        let mut text = String::new();
        if file.seek(SeekFrom::Start(start)).is_ok() && file.read_to_string(&mut text).is_ok() {
            for line in text.lines().rev() {
                let summary = if line.contains("\"type\":\"session.task_complete\"") {
                    serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v["data"]["summary"].as_str().map(str::to_owned))
                } else if line.contains("\"type\":\"assistant.message\"") {
                    serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v["data"]["content"].as_str().filter(|c| !c.trim().is_empty()).map(str::to_owned))
                } else {
                    None
                };
                if let Some(summary) = summary {
                    last = clip(&summary, 700);
                    break;
                }
            }
        }
    }
    (first, last)
}

/// Writes what the group's sessions are doing for a passive run to read.
fn write_context(shared: &Shared, routing: &Routing, group: &bus::RouteGroup, state: &BusState, path: &Path) {
    let mut text = format!("# Sessions in group \"{}\"\n\nWritten by Copilot Canvas at {} (UTC). Open a history file for details.\n", group.name, crate::paths::rfc3339_now());
    let sessions = lock(&shared.sessions).clone();
    for (id, _) in routing.members.iter().filter(|(_, g)| **g == group.id) {
        let name = routing.names.get(id).cloned().unwrap_or_else(|| id.clone());
        let summary = sessions.get(&shared.native_of(id));
        let history = shared.instance.sessions_dir.join(id).join("events.jsonl");
        let (first, last) = digest(&history);
        text.push_str(&format!("\n## {name}\n- session: `{id}`\n"));
        if let Some(s) = summary {
            text.push_str(&format!("- status: {:?}{} · folder: {} · branch: {}\n", s.activity, if s.live { " (running)" } else { "" }, s.cwd, if s.branch.is_empty() { "—" } else { &s.branch }));
        }
        if !first.is_empty() {
            text.push_str(&format!("- first request: {first}\n"));
        }
        if !last.is_empty() {
            text.push_str(&format!("- latest: {last}\n"));
        }
        text.push_str(&format!("- history: {}\n", history.display()));
    }
    let automations = super::factory_watch::context(routing, &group.id, &super::factory_watch::load(&shared.instance.home));
    if !automations.is_empty() {
        text.push_str("\n# Automations in this group\n\nScheduled tasks managed from manifests (`copilot-canvas automations show <name>` for details).\n");
        text.push_str(&automations);
    }
    text.push_str("\n## Passive sessions\n");
    for p in routing.passive.iter().filter(|p| p.group == group.id) {
        let s = state.passive.get(&p.key);
        let status = s.map(|s| format!("{} {}", s.status, s.headline)).unwrap_or_default();
        text.push_str(&format!("- {} (on {}){}\n", p.name, if p.subscribe.is_empty() { "schedule".into() } else { p.subscribe.join(", ") }, if status.trim().is_empty() { String::new() } else { format!(": {}", status.trim()) }));
    }
    write_atomic(path, text.as_bytes());
}

/// Creates the passive session's files when missing (the canvas normally does).
fn ensure_files(paths: &bus::PassivePaths, spec: &PassiveSpec) {
    let _ = std::fs::create_dir_all(&paths.dir);
    let defaults = [
        (&paths.rules, format!("# Rules for {}\n\nDescribe the job: what to check, when to alert, what to report.\n", spec.name)),
        (&paths.state, "# Memory\n\n(Nothing yet.)\n".to_owned()),
        (&paths.log, format!("# Run log of {}\n\n", spec.name)),
    ];
    for (path, text) in defaults {
        if !path.exists() {
            let _ = std::fs::write(path, text);
        }
    }
}

pub(super) fn handle_event(shared: &Shared, routing: &Routing, state: &mut BusState, mut event: Event, now: u64) {
    event.id = uuid::Uuid::new_v4().to_string();
    event.ms = now;
    if event.group.is_empty() {
        event.group = routing.group_of(&event.source).unwrap_or_default();
    }
    if event.source_name.is_empty() {
        event.source_name = routing.names.get(&event.source).cloned().or_else(|| routing.passive_by_session(&event.source).map(|p| p.name.clone())).unwrap_or_default();
    }
    let mut targets: Vec<String> = Vec::new();
    match event.kind.as_str() {
        "passive.status" => {
            if let Some(spec) = routing.passive_by_session(&event.source) {
                let s = state.passive.entry(spec.key.clone()).or_default();
                s.status = event.data["state"].as_str().unwrap_or("ok").to_owned();
                s.headline = event.data["headline"].as_str().unwrap_or_default().chars().take(200).collect();
                s.status_ms = now;
            }
        }
        "passive.followup" => {
            if let Some(spec) = routing.passive_by_session(&event.source) {
                let s = state.passive.entry(spec.key.clone()).or_default();
                let at = event.data["at_ms"].as_u64().unwrap_or(now + 86_400_000);
                if s.followups.len() < 20 {
                    s.followups.push((at, event.data["note"].as_str().unwrap_or_default().to_owned()));
                }
            }
        }
        "message" => {
            let text = event.data["text"].as_str().map(str::to_owned).unwrap_or_else(|| event.data.to_string());
            match routing.resolve(&event.to, &event.group) {
                Some(Target::Passive(key)) => {
                    if let Some(spec) = routing.passive.iter().find(|p| p.key == key) {
                        targets.push(spec.session_id.clone());
                    }
                    let mut forwarded = event.clone();
                    forwarded.hop += 1;
                    state.passive.entry(key).or_default().enqueue(forwarded, now);
                }
                Some(Target::Session(id)) if !routing.no_messages.contains(&id) => {
                    targets.push(id.clone());
                    let from = if event.source_name.is_empty() { "Copilot Canvas".to_owned() } else { event.source_name.clone() };
                    state.mailbox.push(Mail { id: event.id.clone(), to: id, from, text, ms: now });
                }
                Some(Target::Session(id)) => log(&shared.instance, &format!("message for {id} dropped: it does not accept messages")),
                None => log(&shared.instance, &format!("message for {:?} dropped: no such session", event.to)),
            }
        }
        _ => {
            for key in routing.subscribers(&event) {
                if let Some(spec) = routing.passive.iter().find(|p| p.key == key) {
                    targets.push(spec.session_id.clone());
                }
                let mut forwarded = event.clone();
                forwarded.hop += 1;
                state.passive.entry(key).or_default().enqueue(forwarded, now);
            }
        }
    }
    append_log(&shared.instance.home, &event);
    state.remember(&event);
    let pulse = serde_json::json!({ "event": event, "targets": targets });
    shared.broadcast(ServiceMsg::BusEvent { json: pulse.to_string() });
}

/// Starts (or types in) one passive run with its pending events.
fn start_run(shared: &Shared, routing: &Routing, spec: &PassiveSpec, state: &mut BusState, now: u64) {
    let snapshot_state = state.clone();
    let Some(s) = state.passive.get_mut(&spec.key) else { return };
    let Some(group) = routing.group(&spec.group).cloned() else { return };
    let Some(folder) = group.folder.clone().map(PathBuf::from) else {
        s.fail("the group has no folder (set one from the group's menu)", now);
        return;
    };
    let paths = bus::passive_paths(&folder, &spec.slug);
    ensure_files(&paths, spec);
    if s.session_id.is_empty() {
        s.session_id = spec.session_id.clone();
    }
    let mut fresh = !s.started;
    if s.needs_rollover(spec) {
        s.session_id = uuid::Uuid::new_v4().to_string();
        s.native_id.clear();
        s.episode += 1;
        s.runs_since_rollover = 0;
        s.started = false;
        fresh = true;
        log(&shared.instance, &format!("passive {} rolls over to a fresh conversation {}", spec.name, s.session_id));
    }
    let session = s.session_id.clone();
    let events = std::mem::take(&mut s.pending);
    let hop = events.iter().map(|e| e.hop).max().unwrap_or(0) + 1;
    write_context(shared, routing, &group, &snapshot_state, &paths.context);
    let prompt = bus::run_prompt(spec, &group, &paths, &events, fresh);
    // You have it open: type the run in when it is idle (never mid-turn).
    let open = lock(&shared.terms).get(&session).is_some_and(|t| t.alive.load(Ordering::Acquire));
    if open {
        if idle(shared, &session) && type_in(shared, &session, &prompt) {
            s.last_run_ms = now;
            s.runs_today += 1;
            s.runs_since_rollover += 1;
            s.started = true;
        } else {
            s.pending = events;
            s.retry_at_ms = now + 15_000;
        }
        return;
    }
    let kind = AgentKind::parse(&spec.agent).unwrap_or(AgentKind::Copilot);
    let native = (!s.native_id.is_empty()).then(|| s.native_id.clone());
    if kind.provider().is_live(native.as_deref().unwrap_or(&session)) {
        // Open in another terminal or VS Code: try later.
        s.pending = events;
        s.retry_at_ms = now + 60_000;
        return;
    }
    // The agent's provider adds its headless flags (Copilot: -p … --allow-all
    // --no-ask-user) and the canvas integration (skills, identity, PATH).
    let launch = AgentLaunch {
        agent: kind.id().to_owned(),
        native_id: native.clone(),
        resume: !fresh,
        name: Some(spec.name.clone()),
        allow_all: true,
        mcp_off: spec.mcp_off.clone(),
        group_folder: Some(folder.display().to_string()),
        headless_prompt: Some(prompt),
        env: vec![("COPILOT_CANVAS_PASSIVE".to_owned(), spec.key.clone()), ("COPILOT_CANVAS_HOP".to_owned(), hop.to_string())],
        ..Default::default()
    };
    match shared.launch(&session, LaunchKind::Agent(Box::new(launch)), &folder.display().to_string(), 120, 40) {
        Ok(()) => {
            if kind == AgentKind::OpenCode && native.is_none() {
                // `opencode run` names its session itself: learn it from the folder.
                shared.expect_session(&session, Some(folder.display().to_string()), false);
            }
            s.running = true;
            s.started = true;
            s.inflight = events;
            log(&shared.instance, &format!("passive {} run started ({} event(s))", spec.name, s.inflight.len()));
        }
        Err(error) => {
            s.pending = events;
            s.fail(&format!("{error:#}"), now);
        }
    }
}

/// Finishes runs whose terminal exited.
fn reap(shared: &Shared, routing: &Routing, state: &mut BusState, now: u64) {
    for spec in &routing.passive {
        let Some(s) = state.passive.get_mut(&spec.key).filter(|s| s.running) else { continue };
        let term = lock(&shared.terms).get(&s.session_id).cloned();
        let exit = match &term {
            Some(t) if t.alive.load(Ordering::Acquire) => continue,
            Some(t) => t.info().exit_code,
            None => None,
        };
        if exit == Some(0) {
            s.running = false;
            s.inflight.clear();
            s.failures = 0;
            s.last_error.clear();
            s.last_run_ms = now;
            s.runs_today += 1;
            s.runs_since_rollover += 1;
        } else {
            let inflight = std::mem::take(&mut s.inflight);
            s.pending.splice(0..0, inflight);
            s.fail(&format!("the run ended with exit code {}", exit.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into())), now);
        }
    }
}

/// Types queued messages into their sessions when they are open and idle.
/// One message per pass, so the session reacts before the next one arrives.
fn deliver_mail(shared: &Shared, state: &mut BusState) {
    for mail in state.mailbox.clone() {
        if idle(shared, &mail.to) && type_in(shared, &mail.to, &bus::mail_text(&mail)) {
            log(&shared.instance, &format!("delivered a message from {} to {}", mail.from, mail.to));
            state.mailbox.retain(|m| m.id != mail.id);
            break;
        }
    }
}

pub(super) fn bus_loop(shared: Arc<Shared>, incoming: Receiver<Event>) {
    let home = shared.instance.home.clone();
    let inbox = bus::inbox_dir(&home);
    let _ = std::fs::create_dir_all(&inbox);
    let mut last_snapshot = String::new();
    let mut last_saved = String::new();
    let mut watch = super::factory_watch::load(&home);
    loop {
        let mut events: Vec<Event> = Vec::new();
        if let Ok(event) = incoming.recv_timeout(Duration::from_secs(1)) {
            events.push(event);
        }
        events.extend(incoming.try_iter());
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&inbox)
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
            .unwrap_or_default();
        files.sort();
        for file in files {
            if let Ok(bytes) = std::fs::read(&file) {
                match serde_json::from_slice::<Event>(&bytes) {
                    Ok(event) => events.push(event),
                    Err(error) => log(&shared.instance, &format!("unreadable event {}: {error}", file.display())),
                }
            }
            let _ = std::fs::remove_file(&file);
        }
        let now = now_ms();
        let local = bus::Local::now();
        let routing = lock(&shared.routing).clone();
        let mut state = lock(&shared.bus).clone();
        let (failures, watch_changed) = super::factory_watch::poll(&routing, &mut watch, now);
        for event in &failures {
            log(&shared.instance, &format!("{} {}: {}", event.kind, event.source_name, event.data["reason"].as_str().unwrap_or("ok")));
        }
        events.extend(failures);
        if watch_changed {
            write_atomic(&super::factory_watch::path(&home), serde_json::to_string(&watch).unwrap_or_default().as_bytes());
        }
        for event in events {
            handle_event(&shared, &routing, &mut state, event, now);
        }
        for spec in &routing.passive {
            let s = state.passive.entry(spec.key.clone()).or_default();
            if s.session_id.is_empty() {
                s.session_id = spec.session_id.clone();
            }
            if s.native_id.is_empty()
                && let Some(native) = lock(&shared.agent_sessions).get(&s.session_id)
            {
                s.native_id = native.clone();
            }
            s.roll_day(&local.date());
            for event in s.due_timers(spec, now, local) {
                s.enqueue(event, now);
            }
        }
        reap(&shared, &routing, &mut state, now);
        let mut running = state.passive.values().filter(|s| s.running).count();
        for spec in &routing.passive {
            if running >= bus::MAX_CONCURRENT {
                break;
            }
            if state.passive.get(&spec.key).is_some_and(|s| s.ready(spec, now)) {
                start_run(&shared, &routing, spec, &mut state, now);
                if state.passive.get(&spec.key).is_some_and(|s| s.running) {
                    running += 1;
                }
            }
        }
        // Forget state of removed passive sessions; mail for gone sessions stays a day.
        state.passive.retain(|key, _| routing.passive.iter().any(|p| &p.key == key));
        state.mailbox.retain(|m| now.saturating_sub(m.ms) < 86_400_000);
        deliver_mail(&shared, &mut state);
        *lock(&shared.bus) = state.clone();
        let snapshot = serde_json::to_string(&state.snapshot(&routing, now)).unwrap_or_default();
        if snapshot != last_snapshot {
            shared.broadcast(ServiceMsg::Bus { json: snapshot.clone() });
            last_snapshot = snapshot;
        }
        let saved = serde_json::to_string(&state).unwrap_or_default();
        if saved != last_saved {
            write_atomic(&bus_dir(&home).join("state.json"), saved.as_bytes());
            last_saved = saved;
        }
    }
}

/// Client requests: run now, resume after done, discard mail, emit from the UI.
pub(super) fn action(shared: &Shared, json: &str, incoming: &crossbeam_channel::Sender<Event>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else { return };
    let key = value["key"].as_str().unwrap_or_default().to_owned();
    let now = now_ms();
    let mut state = lock(&shared.bus);
    match value["action"].as_str().unwrap_or_default() {
        "run" => {
            if let Some(spec) = lock(&shared.routing).passive.iter().find(|p| p.key == key) {
                let s = state.passive.entry(key.clone()).or_default();
                let event = Event { kind: "manual.run".into(), group: spec.group.clone(), source_name: "you".into(), ms: now, ..Default::default() };
                s.enqueue(event, now);
                s.first_pending_ms = now.saturating_sub(bus::DEBOUNCE_MS);
                s.retry_at_ms = 0;
                s.failures = 0;
                if s.status == "done" {
                    s.status.clear();
                }
            }
        }
        "resume" => {
            if let Some(s) = state.passive.get_mut(&key) {
                if s.status == "done" {
                    s.status.clear();
                    s.headline.clear();
                }
                s.failures = 0;
                s.retry_at_ms = 0;
            }
        }
        "discard_mail" => {
            let to = value["to"].as_str().unwrap_or_default();
            state.mailbox.retain(|m| m.to != to);
        }
        "emit" => {
            if let Ok(event) = serde_json::from_value::<Event>(value["event"].clone()) {
                let _ = incoming.try_send(event);
            }
        }
        _ => {}
    }
}

/// Client snapshot on connect.
pub(super) fn snapshot(shared: &Shared) -> String {
    let routing = lock(&shared.routing).clone();
    serde_json::to_string(&lock(&shared.bus).snapshot(&routing, now_ms())).unwrap_or_default()
}

/// The service keeps running for passive schedules even with no window open.
pub(super) fn has_work(shared: &Shared) -> bool {
    lock(&shared.routing).passive.iter().any(|p| !p.paused) || !lock(&shared.bus).mailbox.is_empty()
}

pub(super) fn input_noted(shared: &Shared, id: &str) {
    lock(&shared.input_at).insert(id.to_owned(), now_ms());
}

#[allow(dead_code)]
fn _assert_send(_: HashMap<String, String>) {}
