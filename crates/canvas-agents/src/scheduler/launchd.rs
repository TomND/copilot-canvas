//! launchd (macOS; MAC-LINUX-PLAN §5.5): one LaunchAgent per task,
//! `~/Library/LaunchAgents/com.copilot-canvas.task.<name>.plist`, loaded into
//! the user's domain (`gui/<uid>` in a login session, else `user/<uid>`).
//!
//! The plist runs the launcher. Calendar slots are `StartCalendarInterval`
//! entries (local time, no drift with daylight saving); `logon` is
//! `RunAtLoad`, which also runs the task once when it is installed or
//! enabled; an interval that does not fit the calendar is `StartInterval`
//! (from when the job was loaded: its `from` time is not kept). launchd never
//! starts a job that is still running, so `overlap: ignore` is launchd's own;
//! other overlaps start the launcher detached (`AbandonProcessGroup`) and its
//! lock decides. launchd fires a missed calendar run on wake (coalesced),
//! whatever `start_when_available` says.
//!
//! A disabled task keeps its plist with `Disabled` and is not loaded; an
//! installed task counts as enabled only while launchd has it loaded. The
//! definition the trait passes around is the plist's text; the task's name,
//! manifest and schedule are comments at its top (launchd ignores them).
//! Replacing a loaded definition reloads the job, which stops a run in
//! progress. `COPILOT_CANVAS_LAUNCHD_DIR` puts the plists elsewhere (tests:
//! a job loads from any folder, only `~/Library/LaunchAgents` loads at login).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::common::{self, Slots};
use super::{Scheduler, TaskInfo};
use crate::factory::{Defaults, Imported, Manifest, Trigger};

const PREFIX: &str = "com.copilot-canvas.task.";
const NAME: &str = "copilot-canvas-name: ";
const MANIFEST: &str = "copilot-canvas-manifest: ";
const SCHEDULE: &str = "copilot-canvas-schedule: ";
const LAUNCHCTL: &str = "/bin/launchctl";
const DISABLED: &str = "\t<key>Disabled</key>\n\t<true/>\n";

pub struct Launchd {
    /// Where the plists are (`~/Library/LaunchAgents`).
    pub dir: PathBuf,
}

impl Launchd {
    pub fn user() -> Launchd {
        Launchd { dir: agents_dir() }
    }

    /// `launchctl` is there (macOS).
    pub fn available() -> bool {
        Path::new(LAUNCHCTL).is_file()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.plist", label(name)))
    }
}

/// `~/Library/LaunchAgents`, or `COPILOT_CANVAS_LAUNCHD_DIR`.
fn agents_dir() -> PathBuf {
    std::env::var_os("COPILOT_CANVAS_LAUNCHD_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join("Library").join("LaunchAgents")))
        .unwrap_or_default()
}

/// `com.copilot-canvas.task.<name>`: letters, digits, `.`, `-` and `_` kept,
/// any other byte as `%XX`.
pub fn label(name: &str) -> String {
    let mut out = String::from(PREFIX);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The name a label stands for (a plist without its name comment).
fn unlabel(label: &str) -> Option<String> {
    let rest = label.strip_prefix(PREFIX)?.as_bytes();
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%' {
            let hex = std::str::from_utf8(rest.get(i + 1..i + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(rest[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

// ---- launchctl ----

/// The user's domain: `gui/<uid>` in a login session, else `user/<uid>`.
fn domain() -> &'static str {
    static DOMAIN: OnceLock<String> = OnceLock::new();
    DOMAIN.get_or_init(|| {
        let uid = canvas_sys::host::user_id();
        let gui = format!("gui/{uid}");
        let ok = Command::new(LAUNCHCTL).args(["print", &gui]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if ok { gui } else { format!("user/{uid}") }
    })
}

fn launchctl(args: &[&str]) -> Result<String> {
    let out = Command::new(LAUNCHCTL).args(args).stdin(Stdio::null()).output().context("cannot run launchctl")?;
    if !out.status.success() {
        let said = format!("{} {}", String::from_utf8_lossy(&out.stderr).trim(), String::from_utf8_lossy(&out.stdout).trim());
        bail!("launchctl {}: {}", args.join(" "), said.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A loaded job's process (while it runs) and last exit code.
struct Loaded {
    pid: Option<u32>,
    last_exit: Option<i32>,
}

/// The job, if launchd has it loaded.
fn loaded(label: &str) -> Option<Loaded> {
    let text = launchctl(&["print", &format!("{}/{label}", domain())]).ok()?;
    // Top-level properties are indented by one tab.
    let value = |key: &str| {
        text.lines()
            .filter_map(|l| l.strip_prefix('\t').filter(|r| !r.starts_with('\t')))
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.trim_start().strip_prefix('=')).map(|v| v.trim().to_owned()))
    };
    Some(Loaded { pid: value("pid").and_then(|v| v.parse().ok()), last_exit: value("last exit code").and_then(|v| v.parse().ok()) })
}

/// Unloads a job and waits until launchd lets it go.
fn bootout(label: &str) {
    let _ = launchctl(&["bootout", &format!("{}/{label}", domain())]);
    for _ in 0..50 {
        if loaded(label).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Loads a plist (retried: a job just unloaded can still be on its way out).
fn bootstrap(path: &Path) -> Result<()> {
    let path = path.display().to_string();
    let mut tries = 0;
    loop {
        match launchctl(&["bootstrap", domain(), &path]) {
            Ok(_) => return Ok(()),
            Err(e) if tries >= 10 => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no folder")?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("plist")));
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

// ---- plists ----

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn unxml(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Text for an XML comment (which cannot hold `--`).
fn to_comment(text: &str) -> String {
    text.replace('%', "%25").replace("--", "-%2D").replace(['\n', '\r'], " ")
}

fn from_comment(text: &str) -> String {
    text.replace("%2D", "-").replace("%25", "%")
}

/// A plist value (the subset launchd job definitions use).
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Value>),
    Dict(Vec<(String, Value)>),
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Open(&'a str),
    Close(&'a str),
    Empty(&'a str),
    Text(&'a str),
}

fn tokens(text: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        if !rest[..start].trim().is_empty() {
            out.push(Tok::Text(&rest[..start]));
        }
        rest = &rest[start..];
        if rest.starts_with("<!--") {
            rest = &rest[rest.find("-->").map_or(rest.len(), |e| e + 3)..];
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            out.push(Tok::Close(name.trim()));
        } else if let Some(name) = tag.strip_suffix('/') {
            out.push(Tok::Empty(name.trim()));
        } else {
            out.push(Tok::Open(tag.split_whitespace().next().unwrap_or_default()));
        }
    }
    out
}

fn value(toks: &[Tok], i: &mut usize) -> Option<Value> {
    let tok = toks.get(*i)?;
    *i += 1;
    match tok {
        Tok::Empty("true") => Some(Value::Bool(true)),
        Tok::Empty("false") => Some(Value::Bool(false)),
        Tok::Empty("string") => Some(Value::Str(String::new())),
        Tok::Empty("array") => Some(Value::Array(Vec::new())),
        Tok::Empty("dict") => Some(Value::Dict(Vec::new())),
        Tok::Open(name @ ("string" | "integer" | "real" | "date" | "data")) => {
            let text = match toks.get(*i) {
                Some(Tok::Text(t)) => {
                    *i += 1;
                    unxml(t)
                }
                _ => String::new(),
            };
            (toks.get(*i)? == &Tok::Close(name)).then_some(())?;
            *i += 1;
            Some(if *name == "integer" { Value::Int(text.trim().parse().ok()?) } else { Value::Str(text) })
        }
        Tok::Open("array") => {
            let mut items = Vec::new();
            while toks.get(*i)? != &Tok::Close("array") {
                items.push(value(toks, i)?);
            }
            *i += 1;
            Some(Value::Array(items))
        }
        Tok::Open("dict") => {
            let mut entries = Vec::new();
            while toks.get(*i)? != &Tok::Close("dict") {
                (toks.get(*i)? == &Tok::Open("key")).then_some(())?;
                let key = match toks.get(*i + 1)? {
                    Tok::Text(t) => {
                        *i += 3;
                        unxml(t)
                    }
                    _ => {
                        *i += 2;
                        String::new()
                    }
                };
                entries.push((key, value(toks, i)?));
            }
            *i += 1;
            Some(Value::Dict(entries))
        }
        _ => None,
    }
}

/// The plist's top-level dictionary.
fn parse(text: &str) -> Option<Vec<(String, Value)>> {
    let toks = tokens(text);
    let mut i = toks.iter().position(|t| *t == Tok::Open("dict") || *t == Tok::Empty("dict"))?;
    match value(&toks, &mut i)? {
        Value::Dict(entries) => Some(entries),
        _ => None,
    }
}

/// The comments of a plist (trimmed).
fn comments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        let body = &rest[start + 4..];
        let end = body.find("-->").unwrap_or(body.len());
        out.push(body[..end].trim().to_owned());
        rest = &body[end..];
    }
    out
}

/// What an installed plist says.
#[derive(Default)]
struct Installed {
    name: Option<String>,
    label: String,
    enabled: bool,
    schedule: Vec<String>,
    argv: Vec<String>,
}

fn installed(text: &str) -> Installed {
    let notes = comments(text);
    let note = |prefix: &str| notes.iter().filter_map(|c| c.strip_prefix(prefix)).map(from_comment).collect::<Vec<_>>();
    let dict = parse(text).unwrap_or_default();
    let get = |key: &str| dict.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    Installed {
        name: note(NAME).into_iter().next(),
        label: match get("Label") {
            Some(Value::Str(s)) => s.clone(),
            _ => String::new(),
        },
        enabled: get("Disabled") != Some(&Value::Bool(true)),
        schedule: note(SCHEDULE),
        argv: match get("ProgramArguments") {
            Some(Value::Array(items)) => items.iter().filter_map(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None }).collect(),
            _ => Vec::new(),
        },
    }
}

/// A value as one line of text.
fn show(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(items) => items.iter().map(show).collect::<Vec<_>>().join(", "),
        Value::Dict(entries) if entries.is_empty() => "every minute".into(),
        Value::Dict(entries) => entries.iter().map(|(k, v)| format!("{k} {}", show(v))).collect::<Vec<_>>().join(", "),
    }
}

/// `Key=value` lines of a plist for [`common::definition_drift`] (an array
/// gives a line per item; `Disabled` is compared as `enabled`).
fn flat(text: &str) -> String {
    let Some(dict) = parse(text) else { return text.to_owned() };
    let mut lines = Vec::new();
    for (key, value) in dict.iter().filter(|(k, _)| k != "Disabled") {
        match value {
            Value::Array(items) => lines.extend(items.iter().map(|item| format!("{key}={}", show(item)))),
            other => lines.push(format!("{key}={}", show(other))),
        }
    }
    lines.join("\n")
}

/// The calendar entries (`Weekday` 0 = Sunday), the interval in seconds and
/// whether it runs at load (logon) for a schedule.
type Calendar = Vec<Vec<(&'static str, u32)>>;

fn schedule(m: &Manifest) -> (Calendar, Option<u64>, bool) {
    let (mut calendar, mut interval, mut at_load) = (Vec::new(), None::<u64>, false);
    for trigger in &m.schedule {
        for slot in common::slots(trigger) {
            match slot {
                Slots::At { days, hours, minute } => {
                    let days: Vec<Option<u32>> = if days.is_empty() { vec![None] } else { days.iter().map(|&d| Some(((d + 1) % 7) as u32)).collect() };
                    for day in &days {
                        for &hour in &hours {
                            let mut entry = Vec::new();
                            if let Some(day) = day {
                                entry.push(("Weekday", *day));
                            }
                            entry.push(("Hour", hour));
                            entry.push(("Minute", minute));
                            calendar.push(entry);
                        }
                    }
                }
                // An empty entry: every minute.
                Slots::Minutes { step, .. } if step <= 1 => calendar.push(Vec::new()),
                Slots::Minutes { first, step } => calendar.extend((first..60).step_by(step as usize).map(|minute| vec![("Minute", minute)])),
                Slots::Interval { ms, .. } => interval = Some(interval.map_or(ms / 1000, |s| s.min(ms / 1000))),
                Slots::Logon => at_load = true,
            }
        }
    }
    (calendar, interval, at_load)
}

/// The plist of a manifest.
fn render_plist(m: &Manifest, d: &Defaults) -> String {
    let detach = !matches!(m.overlap.as_str(), "" | "ignore");
    let mut argv = common::action(m, d);
    if detach && argv.get(1).is_some_and(|w| w == "automations") {
        argv.insert(3, "--detach".into());
    }
    let (calendar, interval, at_load) = schedule(m);
    let mut out = String::from(PLIST_HEAD);
    out.push_str(&format!("<!-- {NAME}{} -->\n", to_comment(&m.name)));
    out.push_str(&format!("<!-- {MANIFEST}{} -->\n", to_comment(&m.path.display().to_string())));
    for trigger in &m.schedule {
        out.push_str(&format!("<!-- {SCHEDULE}{} -->\n", to_comment(&trigger.text())));
    }
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    out.push_str(&format!("\t<key>Label</key>\n\t<string>{}</string>\n", xml(&label(&m.name))));
    if !m.enabled {
        out.push_str(DISABLED);
    }
    push_argv(&mut out, &argv);
    if !calendar.is_empty() {
        out.push_str("\t<key>StartCalendarInterval</key>\n\t<array>\n");
        for entry in &calendar {
            let fields: String = entry.iter().map(|(k, v)| format!("<key>{k}</key><integer>{v}</integer>")).collect();
            out.push_str(&format!("\t\t<dict>{fields}</dict>\n"));
        }
        out.push_str("\t</array>\n");
    }
    if let Some(seconds) = interval {
        out.push_str(&format!("\t<key>StartInterval</key>\n\t<integer>{seconds}</integer>\n"));
    }
    if at_load {
        out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    }
    if detach {
        out.push_str("\t<key>AbandonProcessGroup</key>\n\t<true/>\n");
    }
    out.push_str("</dict>\n</plist>\n");
    out
}

const PLIST_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n";

fn push_argv(out: &mut String, argv: &[String]) {
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for word in argv {
        out.push_str(&format!("\t\t<string>{}</string>\n", xml(word)));
    }
    out.push_str("\t</array>\n");
}

/// The plist with `Disabled` taken out (enabled) or put in after the label.
fn switched(text: &str, enabled: bool) -> String {
    let without = text.replace(DISABLED, "");
    if enabled {
        return without;
    }
    match without.find("\t<key>Label</key>").and_then(|at| without[at..].find("</string>\n").map(|end| at + end + "</string>\n".len())) {
        Some(at) => format!("{}{DISABLED}{}", &without[..at], &without[at..]),
        None => without,
    }
}

impl Scheduler for Launchd {
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn list(&self) -> Result<BTreeMap<String, TaskInfo>> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Ok(out) };
        let mut files: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(".plist"))).collect();
        files.sort();
        let (now, offset) = (canvas_common::paths::now_ms(), canvas_sys::host::local_offset_minutes());
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let plist = installed(&text);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let Some(name) = plist.name.clone().or_else(|| unlabel(stem)) else { continue };
            let job = loaded(&label(&name));
            let task_file = plist.argv.last().filter(|w| w.ends_with(".json")).map(PathBuf::from);
            let launched = task_file.as_deref().and_then(super::state_of).is_some_and(|state| crate::launcher::running(state, &name));
            let last = task_file
                .as_deref()
                .and_then(|f| crate::launcher::Task::read(f).ok())
                .and_then(|t| std::fs::read_to_string(&t.log_path).ok())
                .and_then(|log| crate::factory::parse_runs(&log).into_iter().find(|r| !r.ended.is_empty()));
            let enabled = plist.enabled && job.is_some();
            let running = job.as_ref().is_some_and(|j| j.pid.is_some()) || launched;
            let state = if running {
                "running"
            } else if !plist.enabled {
                "disabled"
            } else if job.is_none() {
                "not loaded"
            } else {
                "ready"
            };
            let triggers: Vec<Trigger> = plist.schedule.iter().filter_map(|s| Trigger::parse(s).ok()).collect();
            out.insert(
                name.to_lowercase(),
                TaskInfo {
                    name: name.clone(),
                    state: state.into(),
                    enabled,
                    last_run_ms: last.as_ref().and_then(|r| canvas_common::paths::parse_rfc3339_ms(&r.started)).unwrap_or(0),
                    next_run_ms: if enabled { common::next_from_schedule(&triggers, now, offset) } else { 0 },
                    last_result: last.and_then(|r| r.exit).map(|e| e as i32).or_else(|| job.and_then(|j| j.last_exit)).unwrap_or(0),
                    xml: text,
                },
            );
        }
        Ok(out)
    }

    fn definition(&self, name: &str) -> Result<Option<String>> {
        Ok(std::fs::read_to_string(self.path(name)).ok())
    }

    /// Kicks the loaded job; a disabled one runs the launcher directly, detached.
    fn run(&self, name: &str) -> Result<()> {
        let label = label(name);
        if loaded(&label).is_some() {
            launchctl(&["kickstart", &format!("{}/{label}", domain())])?;
            return Ok(());
        }
        let text = std::fs::read_to_string(self.path(name)).with_context(|| format!("{name} is not installed (run apply)"))?;
        let argv = installed(&text).argv;
        let (program, args) = argv.split_first().context("the task has no command")?;
        use canvas_sys::spawn::CommandExt;
        Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn_flags(canvas_sys::spawn::DETACHED).spawn().with_context(|| format!("cannot start {program}"))?;
        Ok(())
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let path = self.path(name);
        let text = std::fs::read_to_string(&path).with_context(|| format!("{name} is not installed (run apply)"))?;
        write_atomic(&path, &switched(&text, enabled))?;
        let label = label(name);
        match (enabled, loaded(&label).is_some()) {
            (true, false) => bootstrap(&path)?,
            (false, true) => bootout(&label),
            _ => {}
        }
        Ok(())
    }

    fn register(&self, name: &str, definition: &str) -> Result<()> {
        let label = label(name);
        let plist = installed(definition);
        anyhow::ensure!(plist.label == label, "the definition is not {name}'s");
        let path = self.path(name);
        write_atomic(&path, definition)?;
        if loaded(&label).is_some() {
            bootout(&label);
        }
        if plist.enabled {
            bootstrap(&path)?;
        }
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<()> {
        let label = label(name);
        if loaded(&label).is_some() {
            bootout(&label);
        }
        let path = self.path(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn render(&self, m: &Manifest, d: &Defaults, _existing: Option<&TaskInfo>) -> String {
        render_plist(m, d)
    }

    fn import(&self, task: &TaskInfo) -> Imported {
        let plist = installed(&task.xml);
        let mut out = Imported::default();
        let m = &mut out.manifest;
        m.name = plist.name.clone().unwrap_or_else(|| task.name.clone());
        m.enabled = plist.enabled;
        for text in &plist.schedule {
            match Trigger::parse(text) {
                Ok(trigger) => m.schedule.push(trigger),
                Err(_) => out.unsupported.push(text.clone()),
            }
        }
        m.success_codes = vec![0];
        m.overlap = "ignore".into();
        m.start_when_available = true;
        let argv = plist.argv;
        let task_file = argv.last().filter(|w| w.ends_with(".json")).map(PathBuf::from);
        match (&task_file, argv.get(1).map(String::as_str)) {
            (Some(file), Some("automations")) => out.defaults = common::read_task_json(file, m).map(|state| Defaults { state, ..Defaults::default() }),
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
        let mut out = Vec::new();
        if task.enabled != m.enabled {
            out.push(format!("enabled: {} → {}", task.enabled, m.enabled));
        }
        let have = installed(&task.xml).schedule;
        let want: Vec<String> = m.schedule.iter().map(Trigger::text).collect();
        if have != want {
            out.push(format!("schedule: {} → {}", have.join(" · "), want.join(" · ")));
        }
        out.extend(common::definition_drift(&flat(&task.xml), &flat(&self.render(m, d, Some(task)))));
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

/// The service at login (`copilot-canvas service install`, MAC-LINUX-PLAN
/// D5, §5.3): `~/Library/LaunchAgents/com.copilot-canvas.service[.<instance>].plist`
/// runs `copilot-canvas service start` when the user logs in. That starts
/// the service in its own session and exits, so launchd never owns the
/// service: it still exits after its idle minutes, clients start it again on
/// demand, and uninstalling never stops it or its terminals.
pub mod service_agent {
    use std::path::{Path, PathBuf};

    use anyhow::Result;

    use super::{PLIST_HEAD, bootout, bootstrap, loaded, push_argv, write_atomic, xml};

    /// The job's label for an instance.
    pub fn label(instance: &str) -> String {
        if instance == "default" { "com.copilot-canvas.service".into() } else { format!("com.copilot-canvas.service.{}", super::label(instance).trim_start_matches(super::PREFIX)) }
    }

    pub fn path(instance: &str) -> PathBuf {
        super::agents_dir().join(format!("{}.plist", label(instance)))
    }

    /// The plist: `exe service start` with the instance's folders.
    pub fn text(exe: &Path, instance: &str, home: &Path, sessions: &Path) -> String {
        let argv = [exe.display().to_string(), "service".into(), "start".into(), "--instance".into(), instance.into(), "--home".into(), home.display().to_string(), "--sessions-dir".into(), sessions.display().to_string()];
        let mut out = String::from(PLIST_HEAD);
        out.push_str(&format!("<!-- Copilot Canvas service ({}): started at login -->\n<plist version=\"1.0\">\n<dict>\n", super::to_comment(instance)));
        out.push_str(&format!("\t<key>Label</key>\n\t<string>{}</string>\n", xml(&label(instance))));
        push_argv(&mut out, &argv);
        out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>AbandonProcessGroup</key>\n\t<true/>\n");
        out.push_str(&format!("\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n", xml(&home.display().to_string())));
        out.push_str("</dict>\n</plist>\n");
        out
    }

    /// Writes the plist and loads it (which starts the service now unless one
    /// runs); answers its path.
    pub fn install(exe: &Path, instance: &str, home: &Path, sessions: &Path) -> Result<PathBuf> {
        anyhow::ensure!(super::Launchd::available(), "launchctl is not there");
        let path = path(instance);
        write_atomic(&path, &text(exe, instance, home, sessions))?;
        if loaded(&label(instance)).is_some() {
            bootout(&label(instance));
        }
        bootstrap(&path)?;
        Ok(path)
    }

    /// Unloads and removes the plist (a running service keeps running).
    pub fn uninstall(instance: &str) -> Result<Option<PathBuf>> {
        let path = path(instance);
        let was_loaded = loaded(&label(instance)).is_some();
        if was_loaded {
            bootout(&label(instance));
        }
        if !path.exists() {
            return Ok(was_loaded.then_some(path));
        }
        std::fs::remove_file(&path)?;
        Ok(Some(path))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_agent_starts_the_instances_service_at_login() {
            let text = super::text(std::path::Path::new("/Applications/Copilot Canvas.app/Contents/MacOS/copilot-canvas"), "work", std::path::Path::new("/Users/ana/.copilot-canvas-work"), std::path::Path::new("/Users/ana/.copilot/session-state"));
            let plist = super::super::parse(&text).unwrap();
            let args = plist.iter().find(|(k, _)| k == "ProgramArguments").map(|(_, v)| super::super::show(v)).unwrap();
            assert_eq!(args, "/Applications/Copilot Canvas.app/Contents/MacOS/copilot-canvas, service, start, --instance, work, --home, /Users/ana/.copilot-canvas-work, --sessions-dir, /Users/ana/.copilot/session-state");
            assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>") && text.contains("<key>AbandonProcessGroup</key>"), "{text}");
            assert_eq!(super::label("default"), "com.copilot-canvas.service");
            assert_eq!(super::label("my work"), "com.copilot-canvas.service.my%20work");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        crate::factory::parse_manifest(
            "name: 'Morning brief & co'\nfamily: 'Notes'\ncommand: '/usr/bin/python3'\nargs: 'run.py --kind \"morning 100%\"'\ncwd: '/Users/ana/notes'\nschedule:\n  - daily 06:45\n  - weekly mon,sun 16:00\n  - every 15m from 00:05\n  - every 7h\n  - logon\ntime_limit: 3h\n",
            Path::new("/Users/ana/automations/notes--x/morning.yaml"),
        )
        .unwrap()
    }

    fn defaults() -> Defaults {
        Defaults { state: std::env::temp_dir().join(format!("cc-ld-{}", uuid::Uuid::new_v4().simple())), ..Defaults::default() }
    }

    #[test]
    fn labels_escape_names_and_read_back() {
        assert_eq!(label("Morning brief & co"), "com.copilot-canvas.task.Morning%20brief%20%26%20co");
        assert_eq!(unlabel(&label("Morning brief & co")).as_deref(), Some("Morning brief & co"));
        assert_eq!(unlabel(&label("Café-1.x_y")).as_deref(), Some("Café-1.x_y"));
        assert_eq!(unlabel("com.other.task"), None);
    }

    #[test]
    fn a_manifest_becomes_a_plist_launchd_reads_and_back() {
        let (m, d) = (manifest(), defaults());
        let text = render_plist(&m, &d);
        let dict = parse(&text).expect("a plist");
        let get = |key: &str| dict.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("Label"), Some(Value::Str(label(&m.name))));
        assert_eq!(get("Disabled"), None);
        let Some(Value::Array(argv)) = get("ProgramArguments") else { panic!("{text}") };
        assert_eq!(&argv[1..3], [Value::Str("automations".into()), Value::Str("launch".into())]);
        assert_eq!(argv[3], Value::Str(d.task_json(&m.name).display().to_string()));
        let Some(Value::Array(calendar)) = get("StartCalendarInterval") else { panic!("{text}") };
        let shown: Vec<String> = calendar.iter().map(show).collect();
        assert_eq!(&shown[..3], ["Hour 6, Minute 45", "Weekday 1, Hour 16, Minute 0", "Weekday 0, Hour 16, Minute 0"]);
        assert_eq!(&shown[3..], ["Minute 5", "Minute 20", "Minute 35", "Minute 50"]);
        assert_eq!(get("StartInterval"), Some(Value::Int(7 * 3600)));
        assert_eq!(get("RunAtLoad"), Some(Value::Bool(true)));
        assert_eq!(get("AbandonProcessGroup"), None, "overlap ignore: launchd's own");
        // The comments keep what the plist cannot (the name, `--` in a path).
        let plist = installed(&text);
        assert_eq!(plist.name.as_deref(), Some("Morning brief & co"));
        assert_eq!(plist.schedule, ["daily 06:45", "weekly mon,sun 16:00", "every 15m from 00:05", "every 7h", "logon"]);
        assert!(comments(&text).iter().any(|c| from_comment(c.strip_prefix(MANIFEST).unwrap_or_default()) == "/Users/ana/automations/notes--x/morning.yaml"));
        // Every minute is one empty entry; other overlaps start the launcher detached.
        let mut often = manifest();
        often.schedule = vec![Trigger::Every(60_000, 0)];
        often.overlap = "queue".into();
        let text = render_plist(&often, &d);
        assert!(text.contains("\t\t<dict></dict>\n") && text.contains("<string>--detach</string>") && text.contains("<key>AbandonProcessGroup</key>"), "{text}");
        assert_eq!(parse(&text).unwrap().iter().find(|(k, _)| k == "StartCalendarInterval").map(|(_, v)| show(v)).as_deref(), Some("every minute"));
        // Apple's own parser takes both plists.
        if cfg!(target_os = "macos") {
            let dir = std::env::temp_dir().join(format!("cc-plist-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let agent = service_agent::text(Path::new("/x/copilot-canvas"), "default", Path::new("/Users/ana/.copilot-canvas"), Path::new("/Users/ana/s"));
            for (file, text) in [("task.plist", render_plist(&manifest(), &d)), ("agent.plist", agent)] {
                std::fs::write(dir.join(file), text).unwrap();
                let out = Command::new("/usr/bin/plutil").arg("-lint").arg(dir.join(file)).output().unwrap();
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn disabling_switches_the_plist_and_drift_and_import_read_it() {
        let (m, d) = (manifest(), defaults());
        std::fs::create_dir_all(d.state.join("tasks")).unwrap();
        std::fs::write(d.task_json(&m.name), common::task_json(&m, &d)).unwrap();
        let backend = Launchd { dir: d.state.join("agents") };
        let text = backend.render(&m, &d, None);
        let off = switched(&text, false);
        assert!(!installed(&off).enabled && installed(&text).enabled);
        assert_eq!(switched(&off, true), text);
        let mut disabled = m.clone();
        disabled.enabled = false;
        assert_eq!(render_plist(&disabled, &d), off, "a disabled manifest renders the switched plist");
        let task = TaskInfo { name: m.name.clone(), enabled: true, xml: text.clone(), ..Default::default() };
        assert_eq!(backend.drift(&m, &d, &task), Vec::<String>::new());
        let imported = backend.import(&task);
        let i = &imported.manifest;
        assert_eq!((i.name.as_str(), i.command.as_str(), i.args.as_str(), i.cwd.as_str()), (m.name.as_str(), m.command.as_str(), m.args.as_str(), m.cwd.as_str()));
        assert_eq!((&i.schedule, i.time_limit_ms), (&m.schedule, m.time_limit_ms));
        assert_eq!(imported.defaults.map(|x| x.state), Some(d.state.clone()));
        let mut changed = m.clone();
        changed.schedule = vec![Trigger::Daily(7 * 60)];
        changed.command = "/opt/homebrew/bin/python3".into();
        changed.enabled = false;
        let drift = backend.drift(&changed, &d, &task);
        assert!(drift.contains(&"enabled: true → false".to_owned()), "{drift:?}");
        assert!(drift.iter().any(|l| l.starts_with("schedule: daily 06:45")), "{drift:?}");
        assert!(drift.iter().any(|l| l.starts_with("StartCalendarInterval: ")), "{drift:?}");
        assert!(drift.contains(&"command: /usr/bin/python3 → /opt/homebrew/bin/python3".to_owned()), "{drift:?}");
        let _ = std::fs::remove_dir_all(&d.state);
    }
}
