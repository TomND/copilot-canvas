//! Watches automations' launcher logs and turns failed runs into
//! `automation.failed` events (and `automation.recovered` once a run succeeds).
//! One failure incident notifies once, then at most every 12 hours while it
//! keeps failing, so a task failing every 30 minutes does not flood the doctor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bus::{AutoRoute, Event, Routing};
use crate::factory;

const REMIND_MS: u64 = 12 * 3_600_000;
const POLL_MS: u64 = 3_000;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Watched {
    pub log_len: u64,
    pub log_modified_ms: u64,
    /// Start time of the newest finished run seen.
    pub last_run: String,
    pub last_exit: Option<i64>,
    pub failing_since: String,
    pub consecutive: u32,
    pub notified_ms: u64,
    pub headline: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Watch {
    pub automations: BTreeMap<String, Watched>,
    #[serde(skip)]
    pub polled_ms: u64,
}

pub fn path(home: &Path) -> PathBuf {
    home.join("bus").join("automations.json")
}

pub fn load(home: &Path) -> Watch {
    std::fs::read(path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Whether a finished run failed, and why. A run that exits 0 but reports
/// `failed`/`alert` for itself counts as failed too.
fn outcome(run: &factory::LogRun, report: Option<&factory::Report>, success: &[i64]) -> Option<String> {
    if run.ended.is_empty() {
        return None;
    }
    let ok = |code: i64| if success.is_empty() { code == 0 } else { success.contains(&code) };
    match run.exit {
        Some(code) if ok(code) && run.error.is_empty() => {
            let report = report.filter(|r| factory::report_is_for(r, &run.started));
            match report {
                Some(r) if matches!(r.state.as_str(), "failed" | "alert") => Some(format!("reported {}: {}", r.state, r.headline)),
                _ => None,
            }
        }
        Some(code) => Some(if run.error.is_empty() { format!("exit code {code}") } else { format!("exit code {code}: {}", run.error) }),
        None => Some(if run.error.is_empty() { "no exit code".into() } else { run.error.clone() }),
    }
}

fn event(route: &AutoRoute, kind: &str, data: serde_json::Value, now: u64) -> Event {
    Event { kind: kind.into(), group: route.group.clone(), source: format!("automation:{}", route.name), source_name: route.name.clone(), data, ms: now, ..Default::default() }
}

/// What the doctor should check first for automations Copilot Canvas itself installs.
fn hint(name: &str) -> Option<&'static str> {
    name.eq_ignore_ascii_case(canvas_common::remote::TUNNEL_TASK).then_some(
        "This hosts the Dev Tunnel for remote canvases. Check the sign-in first (`devtunnel user show`; sign in again with `devtunnel user login -g`), \
         then the tunnel and its port (`devtunnel show copilot-canvas`), then `copilot-canvas doctor` (remote listening). Never make the tunnel anonymous.",
    )
}

/// Checks every watched automation (at most every few seconds); returns the
/// events to handle and whether the watch state changed.
pub fn poll(routing: &Routing, watch: &mut Watch, now: u64) -> (Vec<Event>, bool) {
    if now.saturating_sub(watch.polled_ms) < POLL_MS {
        return (Vec::new(), false);
    }
    watch.polled_ms = now;
    let mut events = Vec::new();
    let mut changed = false;
    for route in &routing.automations {
        let log = Path::new(&route.log);
        let Ok(meta) = std::fs::metadata(log) else { continue };
        let first = !watch.automations.contains_key(&route.name);
        let w = watch.automations.entry(route.name.clone()).or_default();
        if !first && meta.len() == w.log_len && modified_ms(&meta) == w.log_modified_ms {
            continue;
        }
        w.log_len = meta.len();
        w.log_modified_ms = modified_ms(&meta);
        changed = true;
        let runs = factory::parse_runs(&factory::tail(log, 64 * 1024));
        let report = factory::read_report(Path::new(&route.status));
        let Some(latest) = runs.iter().find(|r| !r.ended.is_empty()) else { continue };
        if !first && latest.started == w.last_run && latest.exit == w.last_exit {
            continue;
        }
        let newest_report = report.as_ref().filter(|_| runs.first().is_some_and(|r| r.started == latest.started));
        let failure = outcome(latest, newest_report, &route.success_codes);
        w.last_run = latest.started.clone();
        w.last_exit = latest.exit;
        w.headline = report.as_ref().map(|r| r.headline.clone()).unwrap_or_default();
        match failure {
            None => {
                if w.consecutive > 0 && !first {
                    events.push(event(
                        route,
                        "automation.recovered",
                        serde_json::json!({"name": route.name, "family": route.family, "failing_since": w.failing_since, "failed_runs": w.consecutive, "run": latest.started}),
                        now,
                    ));
                }
                w.consecutive = 0;
                w.failing_since.clear();
                w.notified_ms = 0;
            }
            Some(reason) => {
                if first {
                    // Count the failures already in the log for this incident.
                    let streak: Vec<&factory::LogRun> = runs.iter().filter(|r| !r.ended.is_empty()).take_while(|r| outcome(r, None, &route.success_codes).is_some()).collect();
                    w.consecutive = streak.len().max(1) as u32;
                    w.failing_since = streak.last().map(|r| r.started.clone()).unwrap_or_else(|| latest.started.clone());
                } else {
                    w.consecutive += 1;
                    if w.failing_since.is_empty() {
                        w.failing_since = latest.started.clone();
                    }
                }
                let remind = w.notified_ms == 0 || now.saturating_sub(w.notified_ms) >= REMIND_MS;
                if remind {
                    w.notified_ms = now;
                    let logs: Vec<String> = route.logs.iter().filter_map(|p| factory::newest_match(p)).map(|p| p.display().to_string()).collect();
                    let mut data = serde_json::json!({
                        "name": route.name,
                        "family": route.family,
                        "reason": reason,
                        "exit": latest.exit,
                        "started": latest.started,
                        "ended": latest.ended,
                        "failed_runs_in_a_row": w.consecutive,
                        "failing_since": w.failing_since,
                        "report": report.as_ref().map(|r| serde_json::json!({"state": r.state, "headline": r.headline, "at": r.at})),
                        "manifest": route.manifest,
                        "command": route.command,
                        "uses_ai": route.ai,
                        "launcher_log": route.log,
                        "status_file": route.status,
                        "workload_logs": logs,
                        "docs": route.docs,
                        "rerun": format!("copilot-canvas automations run \"{}\"", route.name),
                    });
                    if let Some(hint) = hint(&route.name) {
                        data["hint"] = hint.into();
                    }
                    events.push(event(route, "automation.failed", data, now));
                }
            }
        }
    }
    let names: Vec<&String> = routing.automations.iter().map(|r| &r.name).collect();
    let before = watch.automations.len();
    watch.automations.retain(|name, _| names.contains(&name));
    (events, changed || before != watch.automations.len())
}

/// Lines for the group context file the doctor reads.
pub fn context(routing: &Routing, group: &str, watch: &Watch) -> String {
    let mut text = String::new();
    for route in routing.automations.iter().filter(|r| r.group == group) {
        let w = watch.automations.get(&route.name).cloned().unwrap_or_default();
        let state = match (w.consecutive, w.last_exit) {
            (0, Some(_)) => "ok".to_owned(),
            (0, None) => "no finished run yet".to_owned(),
            (n, _) => format!("FAILING ({n} run(s) in a row since {})", w.failing_since),
        };
        text.push_str(&format!("\n## {} ({})\n- state: {state}\n- last run: {} (exit {})\n", route.name, route.family, if w.last_run.is_empty() { "—" } else { &w.last_run }, w.last_exit.map(|c| c.to_string()).unwrap_or_else(|| "—".into())));
        if !w.headline.is_empty() {
            text.push_str(&format!("- latest report: {}\n", w.headline));
        }
        text.push_str(&format!("- manifest: {}\n- launcher log: {}\n", route.manifest, route.log));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (PathBuf, Routing) {
        let dir = std::env::temp_dir().join(format!("cc-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let route = AutoRoute { name: "Job".into(), group: "g".into(), log: dir.join("Job.log").display().to_string(), status: dir.join("Job.json").display().to_string(), ..Default::default() };
        (dir, Routing { automations: vec![route], ..Default::default() })
    }

    fn append(path: &str, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn failures_notify_once_per_incident_and_recovery_is_reported() {
        let (dir, routing) = setup();
        let log = routing.automations[0].log.clone();
        append(&log, "2026-10-27T06:00:00-07:00 Starting task.\n2026-10-27T06:01:00-07:00 Exited with code 1.\n");
        let mut watch = Watch::default();
        let (events, _) = poll(&routing, &mut watch, 1_000_000);
        assert_eq!(events.len(), 1, "a failure already in the log is reported once");
        assert_eq!(events[0].kind, "automation.failed");
        assert_eq!(events[0].data["failed_runs_in_a_row"], 1);
        append(&log, "2026-10-27T06:30:00-07:00 Starting task.\n2026-10-27T06:31:00-07:00 Exited with code 1.\n");
        let (events, _) = poll(&routing, &mut watch, 1_010_000);
        assert!(events.is_empty(), "the same incident does not notify again");
        assert_eq!(watch.automations["Job"].consecutive, 2);
        append(&log, "2026-10-27T07:00:00-07:00 Starting task.\n2026-10-27T07:01:00-07:00 Exited with code 1.\n");
        let (events, _) = poll(&routing, &mut watch, 1_000_000 + REMIND_MS + 1);
        assert_eq!(events.len(), 1, "reminder after 12 hours");
        append(&log, "2026-10-27T07:30:00-07:00 Starting task.\n2026-10-27T07:31:00-07:00 Exited with code 0.\n");
        let (events, _) = poll(&routing, &mut watch, 1_000_000 + REMIND_MS + 10_000);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "automation.recovered");
        assert_eq!(events[0].data["failed_runs"], 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_successful_exit_with_a_failed_report_is_a_failure() {
        let (dir, routing) = setup();
        let route = &routing.automations[0];
        append(&route.log, "2026-10-27T06:00:00-07:00 Starting task.\n");
        let mut watch = Watch::default();
        assert!(poll(&routing, &mut watch, 1_000_000).0.is_empty(), "a running task is not judged");
        factory::write_report(Path::new(&route.status), &factory::Report { state: "failed".into(), headline: "no data".into(), at: "2026-10-27T06:00:30-07:00".into(), ..Default::default() }).unwrap();
        append(&route.log, "2026-10-27T06:01:00-07:00 Exited with code 0.\n");
        let (events, _) = poll(&routing, &mut watch, 1_010_000);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["reason"], "reported failed: no data");
        assert!(events[0].data.get("hint").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_tunnel_host_failing_carries_a_hint_for_the_doctor() {
        let (dir, mut routing) = setup();
        routing.automations[0].name = canvas_common::remote::TUNNEL_TASK.into();
        append(&routing.automations[0].log, "2026-10-27T06:00:00-07:00 Starting task.\n2026-10-27T06:01:00-07:00 Exited with code 3.\n");
        let (events, _) = poll(&routing, &mut Watch::default(), 1_000_000);
        assert!(events[0].data["hint"].as_str().unwrap().contains("devtunnel user show"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
