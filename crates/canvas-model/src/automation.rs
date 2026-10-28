//! Automations as the canvas shows them: Copilot's scheduled workflows and
//! session schedules (`data.db`) and the manifest-defined scheduled tasks
//! (`task:<name>`) with their runs, reports and Task Scheduler state. The
//! backend loads them; front ends only read these types.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Run {
    pub id: String,
    pub status: String,
    pub trigger: String,
    pub started_at: String,
    pub completed_at: String,
    pub error: String,
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Automation {
    /// `wf:<id>` for workflows, `session:<session id>` for session schedules.
    pub key: String,
    pub name: String,
    pub prompt: String,
    pub kind: String,
    pub enabled: bool,
    pub toggleable: bool,
    pub schedule: String,
    pub model: String,
    pub reasoning_effort: String,
    pub mode: String,
    pub project: String,
    pub workspace: String,
    pub last_run_at: String,
    pub next_run_at: String,
    pub updated_at: String,
    pub runs: Vec<Run>,
    pub session_id: String,
    /// Manifest-managed scheduled task (`task:<name>`).
    pub factory: Option<Box<Live>>,
}

impl Automation {
    pub fn last_status(&self) -> &str {
        self.runs.first().map(|r| r.status.as_str()).unwrap_or("")
    }
}

/// Result of loading every automation (`Request::Automations`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct AutoLoad {
    pub list: Vec<Automation>,
    pub errors: Vec<String>,
    /// The manifest folder was readable (otherwise task boxes are kept as they are).
    pub factory_loaded: bool,
    /// The manifest folder that was used.
    pub dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// Every day at local minutes after midnight.
    Daily(u32),
    /// Days (`mon`…`sun`) at local minutes after midnight.
    Weekly(Vec<String>, u32),
    /// Every interval (ms), aligned to a local start time.
    Every(u64, u32),
    Logon,
}

pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// `HH:MM` of local minutes after midnight.
pub fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// `30m`, `2h`, `1d` (and `90s`).
pub fn format_ms(ms: u64) -> String {
    match ms {
        0 => "0".into(),
        ms if ms % 86_400_000 == 0 => format!("{}d", ms / 86_400_000),
        ms if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        ms if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms => format!("{}s", ms / 1000),
    }
}

impl Trigger {
    pub fn parse(text: &str) -> Result<Trigger> {
        let words: Vec<String> = text.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let time = |t: &str| crate::bus::parse_hhmm(t).with_context(|| format!("bad time {t:?} in schedule {text:?}"));
        Ok(match words.as_slice() {
            ["logon"] | ["at", "logon"] => Trigger::Logon,
            ["daily", t] | ["daily", "at", t] => Trigger::Daily(time(t)?),
            ["weekly", days, t] | ["weekly", days, "at", t] => {
                let days: Vec<String> = days.split(',').map(|d| d.trim().chars().take(3).collect::<String>()).collect();
                if days.is_empty() || days.iter().any(|d| !DAYS.contains(&d.as_str())) {
                    bail!("bad days in schedule {text:?} (use mon,tue,…)");
                }
                Trigger::Weekly(days, time(t)?)
            }
            ["every", n] => Trigger::Every(crate::bus::parse_duration(n).with_context(|| format!("bad interval in {text:?}"))?, 0),
            ["every", n, "from", t] => Trigger::Every(crate::bus::parse_duration(n).with_context(|| format!("bad interval in {text:?}"))?, time(t)?),
            _ => bail!("unknown schedule {text:?} (daily HH:MM · weekly fri HH:MM · every 2h [from HH:MM] · logon)"),
        })
    }

    pub fn text(&self) -> String {
        match self {
            Trigger::Daily(m) => format!("daily {}", hhmm(*m)),
            Trigger::Weekly(days, m) => format!("weekly {} {}", days.join(","), hhmm(*m)),
            Trigger::Every(ms, 0) => format!("every {}", format_ms(*ms)),
            Trigger::Every(ms, from) => format!("every {} from {}", format_ms(*ms), hhmm(*from)),
            Trigger::Logon => "logon".into(),
        }
    }
}

/// One automation's manifest (`<dir>/<family>/<name>.yaml`).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub path: PathBuf,
    pub name: String,
    pub family: String,
    pub description: String,
    pub enabled: bool,
    pub command: String,
    pub args: String,
    pub cwd: String,
    pub schedule: Vec<Trigger>,
    /// Run time limit in ms; 0 = none.
    pub time_limit_ms: u64,
    /// `ignore` (default), `queue`, `parallel`, `stop`.
    pub overlap: String,
    /// Restart a failed run this many times (0 = never) after `restart_every_ms`.
    pub restart_count: u32,
    pub restart_every_ms: u64,
    pub start_when_available: bool,
    /// Uses an AI agent (shown on the canvas; the doctor knows runs cost credits).
    pub ai: bool,
    /// Owner/policy files the doctor must read before changing anything.
    pub docs: Vec<String>,
    /// Workload logs (globs; the newest match is shown).
    pub logs: Vec<String>,
    /// Outputs worth opening: (title, path or URL).
    pub outputs: Vec<(String, String)>,
    /// Command the launcher runs after each run to write the status report
    /// (gets AUTOMATION_EXIT_CODE and AUTOMATION_STATUS_FILE).
    pub report: String,
    /// Exit codes that mean the run worked (default 0); the report then tells ok from warn.
    pub success_codes: Vec<i64>,
}

impl Manifest {
    pub fn succeeded(&self, run: &LogRun) -> bool {
        !run.ended.is_empty() && run.error.is_empty() && run.exit.is_some_and(|c| self.success_codes.contains(&c) || (self.success_codes.is_empty() && c == 0))
    }
}

/// What an automation reports about its latest run (`AUTOMATION_STATUS_FILE`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// `ok`, `warn`, `alert`, `failed`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub headline: String,
    /// Markdown.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    /// RFC 3339.
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub run: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub state: String,
}

/// One run from the launcher log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogRun {
    pub started: String,
    pub ended: String,
    pub exit: Option<i64>,
    pub error: String,
}

/// A registered Task Scheduler task.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub name: String,
    /// `ready`, `running`, `queued`, `disabled`, `unknown`.
    pub state: String,
    pub enabled: bool,
    /// Unix ms; 0 when never / none.
    pub last_run_ms: u64,
    pub next_run_ms: u64,
    pub last_result: i32,
    pub xml: String,
}

/// Everything the canvas shows for one manifest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    pub manifest: Manifest,
    pub runs: Vec<LogRun>,
    pub report: Option<Report>,
    pub log_path: PathBuf,
    pub status_path: PathBuf,
    /// Newest file per `logs` pattern.
    pub logs: Vec<PathBuf>,
    pub task: Option<TaskInfo>,
    /// Manifest vs registered task.
    pub drift: Vec<String>,
}

/// Whether a report was written during or after the run that started at `started`.
pub fn report_is_for(report: &Report, started: &str) -> bool {
    match (crate::paths::parse_rfc3339_ms(&report.at), crate::paths::parse_rfc3339_ms(started)) {
        (Some(at), Some(start)) => at + 1_000 >= start,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_parse_and_print() {
        assert_eq!(Trigger::parse("daily 06:45").unwrap(), Trigger::Daily(405));
        assert_eq!(Trigger::parse("every 2h from 00:10").unwrap().text(), "every 2h from 00:10");
        assert_eq!(Trigger::parse("weekly fri 16:00").unwrap(), Trigger::Weekly(vec!["fri".into()], 960));
        assert!(Trigger::parse("hourly").is_err());
    }

    #[test]
    fn automations_round_trip_through_json() {
        let live = Live {
            manifest: Manifest { name: "A".into(), schedule: vec![Trigger::Logon], success_codes: vec![0, 1], outputs: vec![("t".into(), "C:\\o".into())], ..Default::default() },
            runs: vec![LogRun { started: "s".into(), ended: "e".into(), exit: Some(1), error: String::new() }],
            log_path: PathBuf::from(r"C:\logs\A.log"),
            task: Some(TaskInfo { name: "A".into(), enabled: true, ..Default::default() }),
            ..Default::default()
        };
        assert!(live.manifest.succeeded(&live.runs[0]));
        let load = AutoLoad { list: vec![Automation { key: "task:A".into(), factory: Some(Box::new(live)), ..Default::default() }], dir: "C:\\m".into(), ..Default::default() };
        let back: AutoLoad = serde_json::from_str(&serde_json::to_string(&load).unwrap()).unwrap();
        assert_eq!(back, load);
    }
}
