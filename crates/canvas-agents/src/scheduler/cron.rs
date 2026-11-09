//! The user's crontab (Linux without a user systemd; MAC-LINUX-PLAN §5.5):
//! one marked block holds a section per task, a header line and its cron
//! lines, which run the launcher. A disabled task's lines are commented out
//! (`#off `). Times are local. Cron has no catch-up for missed runs
//! (`start_when_available`), `logon` is `@reboot`, and intervals that do not
//! divide a day restart each day at their first time; the launcher does the
//! rest (time limit, overlap, retries). The definition the trait passes
//! around is the task's section.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::common::{self, Slots};
use super::{Scheduler, TaskInfo};
use crate::factory::{Defaults, Imported, Manifest, Trigger};

const BEGIN: &str = "# >>> Atelier automations (generated from manifests: edit those, then run `atelier automations apply`) >>>";
const END: &str = "# <<< Atelier automations <<<";
const TASK: &str = "# atelier-task: ";
// Blocks and sections written before the rename are the same tasks; any
// edit of the crontab writes them back under the new markers.
use canvas_common::legacy::{CRON_BEGIN as LEGACY_BEGIN, CRON_END as LEGACY_END, CRON_TASK as LEGACY_TASK};
const OFF: &str = "#off ";

pub struct Cron {
    /// The `crontab` program (`ATELIER_CRONTAB` overrides it, for tests).
    pub program: PathBuf,
}

impl Cron {
    pub fn user() -> Cron {
        let program = canvas_common::env::var_os("ATELIER_CRONTAB").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("crontab"));
        Cron { program }
    }

    /// `crontab` is on `PATH`.
    pub fn available() -> bool {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("crontab").is_file()))
    }

    fn read(&self) -> Result<String> {
        let out = Command::new(&self.program).arg("-l").stdin(Stdio::null()).output().with_context(|| format!("cannot run {}", self.program.display()))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let error = String::from_utf8_lossy(&out.stderr);
        if error.contains("no crontab") {
            return Ok(String::new());
        }
        bail!("crontab -l: {}", error.trim())
    }

    fn write(&self, text: &str) -> Result<()> {
        let mut child = Command::new(&self.program).arg("-").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().with_context(|| format!("cannot run {}", self.program.display()))?;
        child.stdin.take().context("crontab stdin")?.write_all(text.as_bytes())?;
        let out = child.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "crontab -: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    }

    /// Replaces (`Some`) or removes (`None`) a task's section of the block.
    fn edit(&self, name: &str, section: Option<&str>) -> Result<()> {
        let text = self.read()?;
        let (before, sections, after) = split(&text);
        let mut kept: Vec<String> = sections.into_iter().filter(|s| !task_name(s).is_some_and(|n| n.eq_ignore_ascii_case(name))).collect();
        if let Some(section) = section {
            kept.push(section.trim_end().to_owned() + "\n");
        }
        let mut out = before;
        if !kept.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(BEGIN);
            out.push('\n');
            for s in &kept {
                out.push_str(s);
            }
            out.push_str(END);
            out.push('\n');
        }
        out.push_str(&after);
        self.write(&out)
    }

    fn section(&self, name: &str) -> Result<Option<String>> {
        Ok(split(&self.read()?).1.into_iter().find(|s| task_name(s).is_some_and(|n| n.eq_ignore_ascii_case(name))))
    }
}

/// The crontab around the block, and the block's task sections (also of a
/// block from before the rename, and of both when both are there).
fn split(text: &str) -> (String, Vec<String>, String) {
    let (mut before, mut after) = (String::new(), String::new());
    let mut sections: Vec<String> = Vec::new();
    let mut place = 0; // 0 before, 1 inside, 2 after
    for line in text.lines() {
        match place {
            0 | 2 if line == BEGIN || line == LEGACY_BEGIN => place = 1,
            0 => {
                before.push_str(line);
                before.push('\n');
            }
            1 if line == END || line == LEGACY_END => place = 2,
            1 if line.starts_with(TASK) || line.starts_with(LEGACY_TASK) => sections.push(format!("{line}\n")),
            1 => {
                if let Some(last) = sections.last_mut() {
                    last.push_str(line);
                    last.push('\n');
                }
            }
            _ => {
                after.push_str(line);
                after.push('\n');
            }
        }
    }
    (before, sections, after)
}

/// The header's fields: name, enabled, schedule.
fn header(section: &str) -> Option<(String, bool, String)> {
    let first = section.lines().next()?;
    let line = first.strip_prefix(TASK).or_else(|| first.strip_prefix(LEGACY_TASK))?;
    let mut parts = line.splitn(3, " | ");
    let name = parts.next()?.to_owned();
    let enabled = parts.next().is_none_or(|e| e.trim() == "enabled");
    Some((name, enabled, parts.next().unwrap_or_default().to_owned()))
}

fn task_name(section: &str) -> Option<String> {
    header(section).map(|(name, _, _)| name)
}

/// The command part of a cron line (after the schedule fields).
fn command_of(line: &str) -> &str {
    let line = line.strip_prefix(OFF).unwrap_or(line);
    if let Some(rest) = line.strip_prefix('@') {
        return rest.split_once(' ').map(|(_, c)| c).unwrap_or_default();
    }
    let mut rest = line;
    for _ in 0..5 {
        rest = rest.trim_start().split_once(' ').map(|(_, r)| r).unwrap_or_default();
    }
    rest.trim_start()
}

/// The schedule fields of a slot.
fn when(slot: &Slots) -> Vec<String> {
    match slot {
        Slots::At { days, hours, minute } => {
            let dow = if days.is_empty() { "*".to_owned() } else { days.iter().map(|d| ((d + 1) % 7).to_string()).collect::<Vec<_>>().join(",") };
            vec![format!("{minute} {} * * {dow}", hours.iter().map(u32::to_string).collect::<Vec<_>>().join(","))]
        }
        Slots::Minutes { first, step } => vec![format!("{first}-59/{step} * * * *")],
        Slots::Interval { first, ms } => {
            let step = (*ms / 60_000).max(1) as u32;
            (*first..1440).step_by(step as usize).map(|t| format!("{} {} * * *", t % 60, t / 60)).collect()
        }
        Slots::Logon => vec!["@reboot".into()],
    }
}

/// The task's section for a manifest.
fn section(m: &Manifest, d: &Defaults) -> String {
    let schedule = m.schedule.iter().map(Trigger::text).collect::<Vec<_>>().join(" · ");
    let command = common::action(m, d).iter().map(|w| common::sh_quote(w)).collect::<Vec<_>>().join(" ").replace('%', "\\%");
    let mut out = format!("{TASK}{} | {} | {schedule}\n", m.name, if m.enabled { "enabled" } else { "disabled" });
    for trigger in &m.schedule {
        for slot in common::slots(trigger) {
            for fields in when(&slot) {
                out.push_str(&format!("{}{fields} {command}\n", if m.enabled { "" } else { OFF }));
            }
        }
    }
    out
}

/// The section with its lines switched on or off.
fn switched(section: &str, enabled: bool) -> String {
    let mut out = String::new();
    for (i, line) in section.lines().enumerate() {
        if i == 0 {
            let (name, _, schedule) = header(section).unwrap_or_default();
            out.push_str(&format!("{TASK}{name} | {} | {schedule}\n", if enabled { "enabled" } else { "disabled" }));
        } else {
            let bare = line.strip_prefix(OFF).unwrap_or(line);
            out.push_str(&format!("{}{bare}\n", if enabled { "" } else { OFF }));
        }
    }
    out
}

impl Scheduler for Cron {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn list(&self) -> Result<BTreeMap<String, TaskInfo>> {
        let (now, offset) = (canvas_common::paths::now_ms(), canvas_sys::host::local_offset_minutes());
        let mut out = BTreeMap::new();
        for section in split(&self.read()?).1 {
            let Some((name, enabled, schedule)) = header(&section) else { continue };
            let triggers: Vec<Trigger> = schedule.split(" · ").filter(|s| !s.trim().is_empty()).filter_map(|s| Trigger::parse(s).ok()).collect();
            let words = section.lines().nth(1).map(|l| common::split_words(&command_of(l).replace("\\%", "%"))).unwrap_or_default();
            let task_file = words.last().filter(|w| w.ends_with(".json")).map(PathBuf::from);
            let task = task_file.as_deref().and_then(|f| crate::launcher::Task::read(f).ok());
            let running = task_file.as_deref().and_then(super::state_of).is_some_and(|state| crate::launcher::running(state, &name));
            let last = task.as_ref().and_then(|t| std::fs::read_to_string(&t.log_path).ok()).and_then(|log| crate::factory::parse_runs(&log).into_iter().find(|r| !r.ended.is_empty()));
            out.insert(
                name.to_lowercase(),
                TaskInfo {
                    name: name.clone(),
                    state: (if running { "running" } else if enabled { "ready" } else { "disabled" }).into(),
                    enabled,
                    last_run_ms: last.as_ref().and_then(|r| canvas_common::paths::parse_rfc3339_ms(&r.started)).unwrap_or(0),
                    next_run_ms: if enabled { common::next_from_schedule(&triggers, now, offset) } else { 0 },
                    last_result: last.and_then(|r| r.exit).unwrap_or(0) as i32,
                    xml: section,
                },
            );
        }
        Ok(out)
    }

    fn definition(&self, name: &str) -> Result<Option<String>> {
        self.section(name)
    }

    /// Starts the launcher now, detached (cron itself has no "run now").
    fn run(&self, name: &str) -> Result<()> {
        let section = self.section(name)?.with_context(|| format!("{name} is not installed (run apply)"))?;
        let line = section.lines().nth(1).context("the task has no cron line")?;
        let words = common::split_words(&command_of(line).replace("\\%", "%"));
        let (program, args) = words.split_first().context("the task has no command")?;
        use canvas_sys::spawn::CommandExt;
        Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn_flags(canvas_sys::spawn::DETACHED).spawn().with_context(|| format!("cannot start {program}"))?;
        Ok(())
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let section = self.section(name)?.with_context(|| format!("{name} is not installed (run apply)"))?;
        self.edit(name, Some(&switched(&section, enabled)))
    }

    fn register(&self, name: &str, definition: &str) -> Result<()> {
        anyhow::ensure!(task_name(definition).is_some_and(|n| n.eq_ignore_ascii_case(name)), "the definition is not {name}'s");
        self.edit(name, Some(definition))
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.edit(name, None)
    }

    fn render(&self, m: &Manifest, d: &Defaults, _existing: Option<&TaskInfo>) -> String {
        section(m, d)
    }

    fn import(&self, task: &TaskInfo) -> Imported {
        let mut out = Imported::default();
        let (name, enabled, schedule) = header(&task.xml).unwrap_or_default();
        let m = &mut out.manifest;
        m.name = if name.is_empty() { task.name.clone() } else { name };
        m.enabled = enabled;
        for text in schedule.split(" · ").filter(|s| !s.trim().is_empty()) {
            match Trigger::parse(text) {
                Ok(trigger) => m.schedule.push(trigger),
                Err(_) => out.unsupported.push(text.to_owned()),
            }
        }
        m.success_codes = vec![0];
        m.overlap = "ignore".into();
        m.start_when_available = true;
        let words = task.xml.lines().nth(1).map(|l| common::split_words(&command_of(l).replace("\\%", "%"))).unwrap_or_default();
        let task_file = words.last().filter(|w| w.ends_with(".json")).map(PathBuf::from);
        match (&task_file, words.get(1).map(String::as_str)) {
            (Some(file), Some("automations")) => out.defaults = common::read_task_json(file, m).map(|state| Defaults { state, ..Defaults::default() }),
            (Some(file), _) if words.len() >= 3 => {
                let state = common::read_task_json(file, m);
                out.defaults = state.map(|state| Defaults { launcher: words[1].clone(), python: words[0].clone(), state });
            }
            _ => {
                m.command = words.first().cloned().unwrap_or_default();
                m.args = words.iter().skip(1).map(|w| common::sh_quote(w)).collect::<Vec<_>>().join(" ");
            }
        }
        out
    }

    fn drift(&self, m: &Manifest, d: &Defaults, task: &TaskInfo) -> Vec<String> {
        let mut out = Vec::new();
        if task.xml.starts_with(LEGACY_TASK) {
            out.push("in the crontab block from before the rename (apply rewrites it under the new markers)".to_owned());
        }
        if task.enabled != m.enabled {
            out.push(format!("enabled: {} → {}", task.enabled, m.enabled));
        }
        let (_, _, have) = header(&task.xml).unwrap_or_default();
        let want = m.schedule.iter().map(Trigger::text).collect::<Vec<_>>().join(" · ");
        if have != want {
            out.push(format!("schedule: {have} → {want}"));
        }
        // The cron lines themselves (as if both were enabled: enabled is above).
        let lines = |text: &str| switched(text, true).lines().skip(1).map(str::to_owned).collect::<Vec<_>>().join("\n");
        let rendered = section(m, d);
        let (old, new) = (lines(&task.xml), lines(&rendered));
        if old != new {
            out.push(format!("cron lines: {} → {}", old.replace('\n', " · "), new.replace('\n', " · ")));
        }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    fn manifest() -> Manifest {
        crate::factory::parse_manifest(
            "name: 'Morning brief'\nfamily: 'Notes'\ncommand: '/usr/bin/python3'\nargs: 'run.py'\nschedule:\n  - daily 06:45\n  - weekly mon,sun 16:00\n  - every 15m from 00:05\n  - logon\n",
            Path::new("/home/ana/automations/notes/morning.yaml"),
        )
        .unwrap()
    }

    #[test]
    fn sections_render_switch_and_split() {
        let m = manifest();
        let d = Defaults { state: PathBuf::from("/home/ana/.local/state/background-automations"), ..Defaults::default() };
        let text = section(&m, &d);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# atelier-task: Morning brief | enabled | daily 06:45 · weekly mon,sun 16:00 · every 15m from 00:05 · logon");
        let fields: Vec<&str> = lines[1..].iter().map(|l| &l[..l.len() - command_of(l).len()]).map(str::trim).collect();
        assert_eq!(fields, ["45 6 * * *", "0 16 * * 1,0", "5-59/15 * * * *", "@reboot"]);
        assert!(command_of(lines[1]).ends_with("automations launch '/home/ana/.local/state/background-automations/tasks/Morning brief.json'"), "{}", lines[1]);
        let off = switched(&text, false);
        assert!(off.lines().skip(1).all(|l| l.starts_with(OFF)) && header(&off).is_some_and(|(_, e, _)| !e));
        assert_eq!(switched(&off, true), text);
        let crontab = format!("MAILTO=\"\"\n0 1 * * * backup\n{BEGIN}\n{text}{END}\n# after\n");
        let (before, sections, after) = split(&crontab);
        assert_eq!((before.as_str(), sections, after.as_str()), ("MAILTO=\"\"\n0 1 * * * backup\n", vec![text.clone()], "# after\n"));
    }

    #[test]
    fn a_fake_crontab_holds_the_block_and_the_users_own_lines() {
        let dir = std::env::temp_dir().join(format!("cc-cron-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // A crontab stand-in: `-l` prints the file (or "no crontab"), `-` stores stdin.
        let store = dir.join("crontab.txt");
        let fake = dir.join("crontab");
        std::fs::write(&fake, format!("#!/bin/sh\nif [ \"$1\" = -l ]; then [ -f '{0}' ] && cat '{0}' && exit 0; echo 'no crontab for x' >&2; exit 1; fi\ncat > '{0}'\n", store.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cron = Cron { program: fake };
        assert!(cron.list().unwrap().is_empty());
        std::fs::write(&store, "0 1 * * * backup\n").unwrap();
        let (m, d) = (manifest(), Defaults { state: dir.join("state"), ..Defaults::default() });
        std::fs::create_dir_all(d.state.join("tasks")).unwrap();
        std::fs::write(d.task_json(&m.name), common::task_json(&m, &d)).unwrap();
        cron.register(&m.name, &cron.render(&m, &d, None)).unwrap();
        let tasks = cron.list().unwrap();
        let task = &tasks["morning brief"];
        assert!(task.enabled && task.state == "ready" && task.next_run_ms > 0, "{task:?}");
        assert!(cron.drift(&m, &d, task).is_empty(), "{:?}", cron.drift(&m, &d, task));
        let imported = cron.import(task).manifest;
        assert_eq!((imported.command.as_str(), &imported.schedule), (m.command.as_str(), &m.schedule));
        cron.set_enabled(&m.name, false).unwrap();
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.starts_with("0 1 * * * backup\n") && text.contains(&format!("{OFF}45 6 * * *")), "{text}");
        assert_eq!(cron.drift(&m, &d, &cron.list().unwrap()["morning brief"]), vec!["enabled: false → true"]);
        cron.delete(&m.name).unwrap();
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "0 1 * * * backup\n", "only the user's own line is left");
        // A block written before the rename: the same task; apply rewrites it under the new markers.
        let rendered = cron.render(&m, &d, None);
        let old = format!("0 1 * * * backup\n{LEGACY_BEGIN}\n{}{LEGACY_END}\n", rendered.replacen(TASK, LEGACY_TASK, 1));
        std::fs::write(&store, &old).unwrap();
        let tasks = cron.list().unwrap();
        let task = tasks.get("morning brief").expect("the old block's task is listed");
        assert_eq!(cron.drift(&m, &d, task), vec!["in the crontab block from before the rename (apply rewrites it under the new markers)"]);
        cron.register(&m.name, &rendered).unwrap();
        let text = std::fs::read_to_string(&store).unwrap();
        assert_eq!(text, format!("0 1 * * * backup\n{BEGIN}\n{rendered}{END}\n"), "one block, the new markers");
        assert!(cron.drift(&m, &d, &cron.list().unwrap()["morning brief"]).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
