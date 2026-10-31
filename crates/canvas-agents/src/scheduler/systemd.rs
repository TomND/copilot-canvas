//! systemd user units (Linux): per task a service that runs the launcher and,
//! for calendar and interval schedules, a timer; both in
//! `~/.config/systemd/user/copilot-canvas-task-<name>.{service,timer}`
//! (MAC-LINUX-PLAN §5.5). A logon trigger is the service's
//! `WantedBy=default.target` (it runs when the user manager starts).
//!
//! The definition the trait passes around is both files, each after a
//! `# <file name>` line, preceded by `# enabled: true|false` (whether the
//! timer, or the logon service, is enabled). Times are local and do not
//! drift with daylight saving. A timer never starts a service that is still
//! running, so `overlap: ignore` is systemd's own; other overlaps start the
//! launcher detached (`--detach`, `KillMode=process`) so the launcher's lock
//! decides. `start_when_available` is `Persistent=`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::common::{self, Slots};
use super::{Scheduler, TaskInfo};
use crate::factory::{Defaults, Imported, Manifest, Trigger, hhmm};

const PREFIX: &str = "copilot-canvas-task-";
const ENABLED: &str = "# enabled: ";

pub struct Systemd {
    /// The unit folder (`$XDG_CONFIG_HOME/systemd/user`).
    pub dir: PathBuf,
}

impl Systemd {
    pub fn user() -> Systemd {
        let config = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".config"))).unwrap_or_default();
        Systemd { dir: config.join("systemd").join("user") }
    }

    /// A user manager answers (`systemctl --user`).
    pub fn available() -> bool {
        Command::new("systemctl").args(["--user", "show-environment"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }
}

/// `copilot-canvas-task-<name>` with the name escaped as `systemd-escape`
/// does (`\xNN` for anything but letters, digits, `:`, `_` and `.`).
pub fn unit_stem(name: &str) -> String {
    let mut out = String::from(PREFIX);
    for (i, byte) in name.bytes().enumerate() {
        if byte.is_ascii_alphanumeric() || byte == b':' || byte == b'_' || (byte == b'.' && i > 0) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out
}

/// A word of an `ExecStart=` line (systemd's quoting; `%` and `$` doubled).
fn quote(word: &str) -> String {
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

/// The words of an `ExecStart=` line written by [`quote`].
fn words(line: &str) -> Vec<String> {
    common::split_words(line).into_iter().map(|w| w.replace("%%", "%").replace("$$", "$")).collect()
}

/// Text for a setting that expands specifiers (`Description=`).
fn plain(text: &str) -> String {
    text.replace('%', "%%").replace(['\n', '\r'], " ")
}

/// `OnCalendar=` (and `OnUnitActiveSec=`) lines of the schedule.
fn timer_lines(m: &Manifest) -> Vec<String> {
    let mut lines = Vec::new();
    for trigger in &m.schedule {
        for slot in common::slots(trigger) {
            match slot {
                Slots::At { days, hours, minute } => {
                    let days = if days.is_empty() { String::new() } else { format!("{} ", days.iter().map(|&d| ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][d]).collect::<Vec<_>>().join(",")) };
                    let hours = hours.iter().map(|h| format!("{h:02}")).collect::<Vec<_>>().join(",");
                    lines.push(format!("OnCalendar={days}*-*-* {hours}:{minute:02}:00"));
                }
                Slots::Minutes { first, step } => lines.push(format!("OnCalendar=*-*-* *:{first:02}/{step}:00")),
                Slots::Interval { first, ms } => {
                    lines.push(format!("OnCalendar=*-*-* {}:00", hhmm(first)));
                    lines.push(format!("OnUnitActiveSec={}s", ms / 1000));
                }
                Slots::Logon => {}
            }
        }
    }
    lines
}

/// The two unit files of a manifest (the timer only for calendar and
/// interval schedules) and whether they are enabled.
fn files(m: &Manifest, d: &Defaults) -> Vec<(String, String)> {
    let stem = unit_stem(&m.name);
    let description = if m.description.is_empty() { format!("{} (Copilot Canvas automation)", m.name) } else { m.description.clone() };
    let detach = !matches!(m.overlap.as_str(), "" | "ignore");
    let mut argv = common::action(m, d);
    if detach && argv.get(1).is_some_and(|w| w == "automations") {
        argv.insert(3, "--detach".into());
    }
    let mut service = format!("[Unit]\nDescription={}\nX-CopilotCanvas-Name={}\nX-CopilotCanvas-Manifest={}\n", plain(&description), m.name, m.path.display());
    for trigger in &m.schedule {
        service.push_str(&format!("X-CopilotCanvas-Schedule={}\n", trigger.text()));
    }
    service.push_str(&format!("\n[Service]\nType=oneshot\nExecStart={}\n", argv.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")));
    if detach {
        service.push_str("KillMode=process\n");
    }
    if m.schedule.contains(&Trigger::Logon) {
        service.push_str("\n[Install]\nWantedBy=default.target\n");
    }
    let mut out = vec![(format!("{stem}.service"), service)];
    let calendar = timer_lines(m);
    if !calendar.is_empty() {
        let timer = format!(
            "[Unit]\nDescription=Schedule of {}\nX-CopilotCanvas-Name={}\n\n[Timer]\n{}\nPersistent={}\nAccuracySec=1s\n\n[Install]\nWantedBy=timers.target\n",
            plain(&m.name),
            m.name,
            calendar.join("\n"),
            m.start_when_available
        );
        out.push((format!("{stem}.timer"), timer));
    }
    out
}

/// The definition text: `# enabled: …`, then each file after `# <name>`.
fn definition(enabled: bool, files: &[(String, String)]) -> String {
    let mut out = format!("{ENABLED}{enabled}\n");
    for (name, text) in files {
        out.push_str(&format!("# {name}\n{text}"));
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// A definition's enabled flag and files.
fn parse_definition(text: &str) -> (bool, Vec<(String, String)>) {
    let mut enabled = true;
    let mut files: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(flag) = line.strip_prefix(ENABLED) {
            enabled = flag.trim() == "true";
        } else if let Some(name) = line.strip_prefix("# ").filter(|n| n.starts_with(PREFIX) && (n.ends_with(".service") || n.ends_with(".timer"))) {
            files.push((name.trim().to_owned(), String::new()));
        } else if let Some((_, text)) = files.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    (enabled, files)
}

/// The values of `key=` lines of a unit text.
fn values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines().filter_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('='))).collect()
}

/// `Sat 2026-10-31 18:00:00 UTC` (systemctl's time format under `TZ=UTC`) → Unix ms.
fn utc_ms(text: &str) -> u64 {
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.as_slice() {
        [_, date, time, "UTC"] => canvas_common::paths::parse_rfc3339_ms(&format!("{date}T{time}Z")).unwrap_or(0),
        _ => 0,
    }
}

impl Systemd {
    fn systemctl(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("systemctl").arg("--user").args(args).env("TZ", "UTC").env("LC_ALL", "C").stdin(Stdio::null()).output().context("cannot run systemctl")?;
        if !out.status.success() {
            bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The installed files of a task, by unit stem.
    fn installed(&self, stem: &str) -> Vec<(String, String)> {
        ["service", "timer"]
            .iter()
            .filter_map(|ext| {
                let name = format!("{stem}.{ext}");
                std::fs::read_to_string(self.dir.join(&name)).ok().map(|text| (name, text))
            })
            .collect()
    }

    /// Enables (and starts the timer) or disables (and stops it) a task's units.
    fn switch(&self, files: &[(String, String)], enabled: bool) -> Result<()> {
        for (name, _) in files.iter().filter(|(_, t)| t.contains("[Install]")) {
            let timer = name.ends_with(".timer");
            let args: Vec<&str> = match (enabled, timer) {
                (true, true) => vec!["enable", "--now", name],
                (true, false) => vec!["enable", name],
                (false, true) => vec!["disable", "--now", name],
                (false, false) => vec!["disable", name],
            };
            self.systemctl(&args)?;
        }
        Ok(())
    }
}

impl Scheduler for Systemd {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn list(&self) -> Result<BTreeMap<String, TaskInfo>> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Ok(out) };
        let mut stems: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".service")).filter(|n| n.starts_with(PREFIX)).map(str::to_owned))
            .collect();
        stems.sort();
        if stems.is_empty() {
            return Ok(out);
        }
        // One query for every unit: blocks of Key=value separated by blank lines.
        let mut args = vec!["show", "-p", "Id", "-p", "ActiveState", "-p", "UnitFileState", "-p", "NextElapseUSecRealtime", "-p", "LastTriggerUSec", "-p", "ExecMainStatus", "-p", "ExecMainStartTimestamp"];
        let units: Vec<String> = stems.iter().flat_map(|s| [format!("{s}.service"), format!("{s}.timer")]).collect();
        args.extend(units.iter().map(String::as_str));
        let shown = self.systemctl(&args).unwrap_or_default();
        let mut props: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for block in shown.split("\n\n") {
            let map: BTreeMap<String, String> = block.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
            if let Some(id) = map.get("Id").cloned() {
                props.insert(id, map);
            }
        }
        for stem in stems {
            let files = self.installed(&stem);
            let service = files.iter().find(|(n, _)| n.ends_with(".service")).map(|(_, t)| t.as_str()).unwrap_or_default();
            let name = values(service, "X-CopilotCanvas-Name").first().map(|n| n.to_string()).unwrap_or_else(|| stem[PREFIX.len()..].to_owned());
            let empty = BTreeMap::new();
            let svc = props.get(&format!("{stem}.service")).unwrap_or(&empty);
            let timer = props.get(&format!("{stem}.timer"));
            let has_timer = files.iter().any(|(n, _)| n.ends_with(".timer"));
            let enabled = if has_timer { timer.and_then(|t| t.get("UnitFileState")).is_some_and(|s| s == "enabled") } else { svc.get("UnitFileState").is_some_and(|s| s == "enabled") };
            let json = values(service, "ExecStart").first().map(|l| words(l)).and_then(|w| w.last().cloned()).map(PathBuf::from);
            let launched = json.as_deref().and_then(super::state_of).is_some_and(|state| crate::launcher::running(state, &name));
            let active = svc.get("ActiveState").is_some_and(|s| matches!(s.as_str(), "active" | "activating" | "reloading"));
            let state = if active || launched { "running" } else if !enabled { "disabled" } else { "ready" };
            let last_run_ms = timer.and_then(|t| t.get("LastTriggerUSec")).map(|t| utc_ms(t)).filter(|&ms| ms > 0).unwrap_or_else(|| svc.get("ExecMainStartTimestamp").map(|t| utc_ms(t)).unwrap_or(0));
            let info = TaskInfo {
                name: name.clone(),
                state: state.into(),
                enabled,
                last_run_ms,
                next_run_ms: timer.and_then(|t| t.get("NextElapseUSecRealtime")).map(|t| utc_ms(t)).unwrap_or(0),
                last_result: svc.get("ExecMainStatus").and_then(|c| c.parse().ok()).unwrap_or(0),
                xml: definition(enabled, &files),
            };
            out.insert(name.to_lowercase(), info);
        }
        Ok(out)
    }

    fn run(&self, name: &str) -> Result<()> {
        self.systemctl(&["start", "--no-block", &format!("{}.service", unit_stem(name))])?;
        Ok(())
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let files = self.installed(&unit_stem(name));
        anyhow::ensure!(!files.is_empty(), "{name} is not installed (run apply)");
        self.switch(&files, enabled)
    }

    fn register(&self, name: &str, definition: &str) -> Result<()> {
        let (enabled, files) = parse_definition(definition);
        let stem = unit_stem(name);
        anyhow::ensure!(files.iter().all(|(n, _)| n.starts_with(&stem)), "the definition is not {name}'s");
        std::fs::create_dir_all(&self.dir)?;
        // A unit the new definition no longer has (a schedule that became logon only).
        for (old, _) in self.installed(&stem) {
            if !files.iter().any(|(n, _)| *n == old) {
                let _ = self.systemctl(&["disable", "--now", &old]);
                let _ = std::fs::remove_file(self.dir.join(&old));
            }
        }
        for (file, text) in &files {
            let path = self.dir.join(file);
            let temp = self.dir.join(format!(".{file}.tmp"));
            std::fs::write(&temp, text)?;
            std::fs::rename(&temp, &path)?;
        }
        self.systemctl(&["daemon-reload"])?;
        self.switch(&files, enabled)
    }

    fn delete(&self, name: &str) -> Result<()> {
        let stem = unit_stem(name);
        let files = self.installed(&stem);
        let _ = self.switch(&files, false);
        let _ = self.systemctl(&["stop", &format!("{stem}.service")]);
        for (file, _) in &files {
            std::fs::remove_file(self.dir.join(file))?;
        }
        let _ = self.systemctl(&["daemon-reload"]);
        let _ = self.systemctl(&["reset-failed", &format!("{stem}.service")]);
        Ok(())
    }

    fn render(&self, m: &Manifest, d: &Defaults, _existing: Option<&TaskInfo>) -> String {
        definition(m.enabled, &files(m, d))
    }

    fn import(&self, task: &TaskInfo) -> Imported {
        let (enabled, files) = parse_definition(&task.xml);
        let service = files.iter().find(|(n, _)| n.ends_with(".service")).map(|(_, t)| t.as_str()).unwrap_or_default();
        let timer = files.iter().find(|(n, _)| n.ends_with(".timer")).map(|(_, t)| t.as_str()).unwrap_or_default();
        let mut out = Imported::default();
        let m = &mut out.manifest;
        m.name = task.name.clone();
        m.enabled = enabled;
        m.description = values(service, "Description").first().map(|d| d.replace("%%", "%")).filter(|d| *d != format!("{} (Copilot Canvas automation)", task.name)).unwrap_or_default();
        for text in values(service, "X-CopilotCanvas-Schedule") {
            match Trigger::parse(text) {
                Ok(trigger) => m.schedule.push(trigger),
                Err(_) => out.unsupported.push(text.to_owned()),
            }
        }
        m.start_when_available = values(timer, "Persistent").first().is_none_or(|p| *p == "true");
        m.success_codes = vec![0];
        m.overlap = "ignore".into();
        let argv = values(service, "ExecStart").first().map(|l| words(l)).unwrap_or_default();
        let task_file = argv.last().filter(|w| w.ends_with(".json")).map(PathBuf::from);
        match (&task_file, argv.get(1).map(String::as_str)) {
            (Some(file), Some("automations")) => {
                let state = common::read_task_json(file, m);
                out.defaults = state.map(|state| Defaults { state, ..Defaults::default() });
            }
            (Some(file), _) if argv.len() >= 3 => {
                let state = common::read_task_json(file, m);
                out.defaults = state.map(|state| Defaults { launcher: argv[1].clone(), python: argv[0].clone(), state });
            }
            _ => {
                m.command = argv.first().cloned().unwrap_or_default();
                m.args = argv.iter().skip(1).map(|w| common::sh_quote(w)).collect::<Vec<_>>().join(" ");
            }
        }
        out
    }

    fn drift(&self, m: &Manifest, d: &Defaults, task: &TaskInfo) -> Vec<String> {
        let rendered = self.render(m, d, Some(task));
        let strip = |text: &str| text.lines().filter(|l| !l.starts_with(ENABLED) && !l.starts_with("X-CopilotCanvas-Schedule=")).collect::<Vec<_>>().join("\n");
        let mut out = Vec::new();
        if task.enabled != m.enabled {
            out.push(format!("enabled: {} → {}", task.enabled, m.enabled));
        }
        let (_, files) = parse_definition(&task.xml);
        let service = files.iter().find(|(n, _)| n.ends_with(".service")).map(|(_, t)| t.as_str()).unwrap_or_default();
        let have: Vec<&str> = values(service, "X-CopilotCanvas-Schedule");
        let want: Vec<String> = m.schedule.iter().map(Trigger::text).collect();
        if have.iter().map(|s| s.to_string()).collect::<Vec<_>>() != want {
            out.push(format!("schedule: {} → {}", have.join(" · "), want.join(" · ")));
        }
        out.extend(common::definition_drift(&strip(&task.xml), &strip(&rendered)));
        out.extend(common::task_json_drift(m, d));
        out
    }

    fn uses_task_json(&self, _d: &Defaults) -> bool {
        true
    }

    fn task_json(&self, m: &Manifest, d: &Defaults) -> String {
        common::task_json(m, d)
    }

    fn drift_covers_task_json(&self) -> bool {
        true
    }

    fn importable(&self, _task: &TaskInfo) -> bool {
        true
    }

    fn launched_by(&self, _task: &TaskInfo, _d: &Defaults) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn manifest() -> Manifest {
        crate::factory::parse_manifest(
            "name: 'Morning brief'\nfamily: 'Notes'\ncommand: '/usr/bin/python3'\nargs: 'run.py --kind \"morning 100%\"'\ncwd: '/home/ana/notes'\nschedule:\n  - daily 06:45\n  - weekly mon,fri 16:00\n  - every 2h from 00:10\n  - logon\ntime_limit: 3h\n",
            Path::new("/home/ana/automations/notes/morning.yaml"),
        )
        .unwrap()
    }

    fn defaults() -> Defaults {
        Defaults { state: std::env::temp_dir().join(format!("cc-sd-{}", uuid::Uuid::new_v4().simple())), ..Defaults::default() }
    }

    #[test]
    fn units_name_schedule_and_launch_a_manifest() {
        assert_eq!(unit_stem("Morning brief"), "copilot-canvas-task-Morning\\x20brief");
        assert_eq!(unit_stem(".x-y"), "copilot-canvas-task-\\x2ex\\x2dy");
        let (m, d) = (manifest(), defaults());
        let files = files(&m, &d);
        assert_eq!(files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["copilot-canvas-task-Morning\\x20brief.service", "copilot-canvas-task-Morning\\x20brief.timer"]);
        let (service, timer) = (&files[0].1, &files[1].1);
        assert!(service.contains("Type=oneshot\n") && service.contains("WantedBy=default.target"), "{service}");
        let exec = values(service, "ExecStart")[0];
        let argv = words(exec);
        assert_eq!(&argv[1..3], ["automations", "launch"]);
        assert_eq!(argv[3], d.task_json("Morning brief").display().to_string());
        assert_eq!(
            values(timer, "OnCalendar"),
            ["*-*-* 06:45:00", "Mon,Fri *-*-* 16:00:00", "*-*-* 00,02,04,06,08,10,12,14,16,18,20,22:10:00"]
        );
        assert_eq!(values(timer, "Persistent"), ["true"]);
        // A logon-only task has no timer; other overlaps start the launcher detached.
        let mut logon = manifest();
        logon.schedule = vec![Trigger::Logon];
        logon.overlap = "queue".into();
        let only = super::files(&logon, &d);
        assert_eq!(only.len(), 1);
        assert!(only[0].1.contains("KillMode=process") && words(values(&only[0].1, "ExecStart")[0])[3] == "--detach");
    }

    #[test]
    fn a_rendered_definition_imports_back_and_shows_no_drift() {
        let (m, d) = (manifest(), defaults());
        std::fs::create_dir_all(d.state.join("tasks")).unwrap();
        std::fs::write(d.task_json(&m.name), common::task_json(&m, &d)).unwrap();
        let backend = Systemd { dir: d.state.join("units") };
        let text = backend.render(&m, &d, None);
        let (enabled, files) = parse_definition(&text);
        assert!(enabled && files.len() == 2);
        assert_eq!(definition(enabled, &files), text, "the definition survives a round trip");
        let task = TaskInfo { name: m.name.clone(), enabled: true, xml: text.clone(), ..Default::default() };
        assert_eq!(backend.drift(&m, &d, &task), Vec::<String>::new());
        let imported = backend.import(&task);
        let i = &imported.manifest;
        assert_eq!((i.command.as_str(), i.args.as_str(), i.cwd.as_str()), (m.command.as_str(), m.args.as_str(), m.cwd.as_str()));
        assert_eq!((&i.schedule, i.time_limit_ms, i.path.as_path()), (&m.schedule, m.time_limit_ms, m.path.as_path()));
        assert_eq!(imported.defaults.map(|x| x.state), Some(d.state.clone()));
        // A changed manifest: its schedule, its command and enabled show as drift.
        let mut changed = m.clone();
        changed.schedule = vec![Trigger::Daily(7 * 60)];
        changed.command = "/usr/bin/python3.12".into();
        changed.enabled = false;
        let drift = backend.drift(&changed, &d, &task);
        assert!(drift.contains(&"enabled: true → false".to_owned()), "{drift:?}");
        assert!(drift.iter().any(|l| l.starts_with("schedule: daily 06:45")), "{drift:?}");
        assert!(drift.iter().any(|l| l.starts_with("OnCalendar: ")), "{drift:?}");
        assert!(drift.contains(&"command: /usr/bin/python3 → /usr/bin/python3.12".to_owned()), "{drift:?}");
        assert_eq!(utc_ms("Sat 2026-10-31 18:00:00 UTC"), 1_793_469_600_000);
        assert_eq!(utc_ms("n/a"), 0);
        let _ = std::fs::remove_dir_all(&d.state);
    }
}
