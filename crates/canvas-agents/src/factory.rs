//! Automations defined by manifests ("the factory").
//!
//! One YAML manifest per automation (`<dir>/<family>/<name>.yaml`) is the source
//! of truth; Windows Task Scheduler is generated from it (`automations apply`).
//! Each run goes through a hidden launcher that appends `Starting task.` /
//! `Exited with code N.` to `<state>/logs/<name>.log` and passes
//! `AUTOMATION_STATUS_FILE`, where the automation writes a status report
//! (state, headline, summary, links, metrics). The canvas shows all of it and
//! the service turns failed runs into `automation.failed` events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use yaml_rust2::{Yaml, YamlLoader};

pub const DEFAULTS_FILE: &str = "automations.yaml";

/// Repository-wide settings (`<dir>/automations.yaml`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Defaults {
    /// Hidden launcher script (run with `python`); empty: run commands directly.
    pub launcher: String,
    /// Windowless Python (`pythonw.exe`) for the launcher.
    pub python: String,
    /// Launcher state: `tasks/`, `logs/`, `status/`, `backups/`.
    pub state: PathBuf,
}

pub use canvas_model::automation::{DAYS, Link, Live, LogRun, Manifest, Metric, Report, Trigger, format_ms, hhmm, report_is_for};

// ---------- parsing ----------

const XML_DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

fn yaml_str(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_list(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(items) => items.iter().filter_map(yaml_str).collect(),
        other => yaml_str(other).into_iter().collect(),
    }
}

fn yaml_bool(y: &Yaml, default: bool) -> bool {
    match y {
        Yaml::Boolean(b) => *b,
        Yaml::String(s) => matches!(s.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"),
        Yaml::Integer(i) => *i != 0,
        _ => default,
    }
}

fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 && after[..end].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("%{name}%")));
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn parse_defaults(text: &str) -> Result<Defaults> {
    let docs = YamlLoader::load_from_str(text).context("automations.yaml is not valid YAML")?;
    let doc = docs.first().cloned().unwrap_or(Yaml::Null);
    let get = |k: &str| yaml_str(&doc[k]).unwrap_or_default();
    let state = get("state");
    Ok(Defaults {
        launcher: expand(&get("launcher")),
        python: expand(&get("python")),
        state: if state.is_empty() { default_state() } else { PathBuf::from(expand(&state)) },
    })
}

pub fn default_state() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("BackgroundAutomations")
}

pub fn parse_manifest(text: &str, path: &Path) -> Result<Manifest> {
    let docs = YamlLoader::load_from_str(text).with_context(|| format!("{} is not valid YAML", path.display()))?;
    let doc = docs.first().cloned().unwrap_or(Yaml::Null);
    let get = |k: &str| yaml_str(&doc[k]).unwrap_or_default();
    let name = get("name");
    if name.trim().is_empty() {
        bail!("{} has no name", path.display());
    }
    if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("{}: the name {name:?} cannot be a task name", path.display());
    }
    let family = match get("family") {
        f if f.is_empty() => path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        f => f,
    };
    let schedule = yaml_list(&doc["schedule"]).iter().map(|s| Trigger::parse(s)).collect::<Result<Vec<_>>>().with_context(|| path.display().to_string())?;
    let duration = |k: &str| -> Result<u64> {
        let text = get(k);
        if text.is_empty() || text == "0" || text.eq_ignore_ascii_case("none") {
            return Ok(0);
        }
        crate::bus::parse_duration(&text).with_context(|| format!("{}: bad {k} {text:?}", path.display()))
    };
    let (restart_count, restart_every_ms) = match get("restart_on_failure") {
        r if r.is_empty() || r == "0" || r.eq_ignore_ascii_case("never") => (0, 0),
        r => {
            let words: Vec<&str> = r.split_whitespace().collect();
            match words.as_slice() {
                [n, "every", d] => (
                    n.parse().with_context(|| format!("{}: bad restart_on_failure {r:?}", path.display()))?,
                    crate::bus::parse_duration(d).with_context(|| format!("{}: bad restart_on_failure {r:?}", path.display()))?,
                ),
                _ => bail!("{}: restart_on_failure must look like \"3 every 1m\"", path.display()),
            }
        }
    };
    let overlap = match get("overlap").to_ascii_lowercase().as_str() {
        "" | "ignore" => "ignore",
        "queue" => "queue",
        "parallel" => "parallel",
        "stop" => "stop",
        other => bail!("{}: overlap {other:?} must be ignore, queue, parallel or stop", path.display()),
    };
    let outputs = match &doc["outputs"] {
        Yaml::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Yaml::Hash(_) => Some((yaml_str(&item["title"]).unwrap_or_default(), expand(&yaml_str(&item["path"]).or_else(|| yaml_str(&item["url"])).unwrap_or_default()))),
                other => yaml_str(other).map(|p| (String::new(), expand(&p))),
            })
            .map(|(t, p)| (if t.is_empty() { Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone()) } else { t }, p))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Manifest {
        path: path.to_path_buf(),
        name,
        family,
        description: get("description"),
        enabled: yaml_bool(&doc["enabled"], true),
        command: expand(&get("command")),
        args: get("args"),
        cwd: expand(&get("cwd")),
        schedule,
        time_limit_ms: duration("time_limit")?,
        overlap: overlap.into(),
        restart_count,
        restart_every_ms,
        start_when_available: yaml_bool(&doc["start_when_available"], true),
        ai: yaml_bool(&doc["ai"], false),
        report: get("report"),
        success_codes: match yaml_list(&doc["success_codes"]) {
            codes if codes.is_empty() => vec![0],
            codes => codes.iter().map(|c| c.trim().parse::<i64>().with_context(|| format!("{}: bad success code {c:?}", path.display()))).collect::<Result<_>>()?,
        },
        docs: yaml_list(&doc["docs"]).iter().map(|d| expand(d)).collect(),
        logs: yaml_list(&doc["logs"]).iter().map(|d| expand(d)).collect(),
        outputs,
    })
}

/// YAML single-quoted scalar (no escapes besides `''`).
fn q(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Manifest text (import writes these; comments explain the fields).
pub fn manifest_yaml(m: &Manifest) -> String {
    let mut out = String::new();
    out.push_str(&format!("name: {}\nfamily: {}\n", q(&m.name), q(&m.family)));
    if !m.description.is_empty() {
        out.push_str(&format!("description: {}\n", q(&m.description)));
    }
    out.push_str(&format!("enabled: {}\nai: {}\n", m.enabled, m.ai));
    out.push_str(&format!("command: {}\n", q(&m.command)));
    if !m.args.is_empty() {
        out.push_str(&format!("args: {}\n", q(&m.args)));
    }
    if !m.cwd.is_empty() {
        out.push_str(&format!("cwd: {}\n", q(&m.cwd)));
    }
    out.push_str("# daily HH:MM · weekly mon,fri HH:MM · every 2h [from HH:MM] · logon (local time)\nschedule:\n");
    for t in &m.schedule {
        out.push_str(&format!("  - {}\n", q(&t.text())));
    }
    out.push_str(&format!("time_limit: {}\n", q(&format_ms(m.time_limit_ms))));
    out.push_str(&format!("overlap: {}\n", m.overlap));
    if m.restart_count > 0 {
        out.push_str(&format!("restart_on_failure: {}\n", q(&format!("{} every {}", m.restart_count, format_ms(m.restart_every_ms)))));
    }
    out.push_str(&format!("start_when_available: {}\n", m.start_when_available));
    out.push_str("# Run after each run to write the status report (AUTOMATION_EXIT_CODE, AUTOMATION_STATUS_FILE)\n");
    out.push_str(&format!("report: {}\n", q(&m.report)));
    if m.success_codes != [0] && !m.success_codes.is_empty() {
        out.push_str(&format!("success_codes: [{}]\n", m.success_codes.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")));
    }
    let list = |out: &mut String, key: &str, comment: &str, items: &[String]| {
        out.push_str(comment);
        if items.is_empty() {
            out.push_str(&format!("{key}: []\n"));
        } else {
            out.push_str(&format!("{key}:\n"));
            for item in items {
                out.push_str(&format!("  - {}\n", q(item)));
            }
        }
    };
    list(&mut out, "docs", "# Owner/policy files the automation doctor must read before changing anything\n", &m.docs);
    list(&mut out, "logs", "# Workload logs (globs; the newest match is shown and given to the doctor)\n", &m.logs);
    out.push_str("# Outputs worth opening from the canvas\n");
    if m.outputs.is_empty() {
        out.push_str("outputs: []\n");
    } else {
        out.push_str("outputs:\n");
        for (title, path) in &m.outputs {
            out.push_str(&format!("  - title: {}\n    path: {}\n", q(title), q(path)));
        }
    }
    out
}

// ---------- the repository ----------

#[derive(Clone, Debug, Default)]
pub struct Repo {
    pub dir: PathBuf,
    pub defaults: Defaults,
    pub manifests: Vec<Manifest>,
    /// Files that could not be read (shown, never fatal).
    pub errors: Vec<String>,
}

pub fn load_repo(dir: &Path) -> Repo {
    let mut repo = Repo { dir: dir.to_path_buf(), ..Default::default() };
    match std::fs::read_to_string(dir.join(DEFAULTS_FILE)) {
        Ok(text) => match parse_defaults(&text) {
            Ok(d) => repo.defaults = d,
            Err(e) => repo.errors.push(format!("{e:#}")),
        },
        Err(_) => repo.defaults = Defaults { state: default_state(), ..Default::default() },
    }
    let mut files = Vec::new();
    collect_yaml(dir, 0, &mut files);
    files.sort();
    for file in files {
        if file.file_name().is_some_and(|n| n.eq_ignore_ascii_case(DEFAULTS_FILE)) && file.parent() == Some(dir) {
            continue;
        }
        match std::fs::read_to_string(&file).map_err(anyhow::Error::from).and_then(|t| parse_manifest(&t, &file)) {
            Ok(m) => {
                if repo.manifests.iter().any(|x| x.name.eq_ignore_ascii_case(&m.name)) {
                    repo.errors.push(format!("{}: duplicate name {}", file.display(), m.name));
                } else {
                    repo.manifests.push(m);
                }
            }
            Err(e) => repo.errors.push(format!("{e:#}")),
        }
    }
    repo
}

fn collect_yaml(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() && depth < 3 {
            collect_yaml(&path, depth + 1, out);
        } else if name.ends_with(".yaml") || name.ends_with(".yml") {
            out.push(path);
        }
    }
}

impl Defaults {
    pub fn log_path(&self, name: &str) -> PathBuf {
        self.state.join("logs").join(format!("{name}.log"))
    }
    pub fn status_path(&self, name: &str) -> PathBuf {
        self.state.join("status").join(format!("{name}.json"))
    }
    pub fn task_json(&self, name: &str) -> PathBuf {
        self.state.join("tasks").join(format!("{name}.json"))
    }
}

// ---------- run history and reports ----------

/// Runs in a launcher log, newest first.
pub fn parse_runs(text: &str) -> Vec<LogRun> {
    let mut runs: Vec<LogRun> = Vec::new();
    for line in text.lines() {
        let Some((at, message)) = line.split_once(' ') else { continue };
        if message.starts_with("Starting task.") {
            runs.push(LogRun { started: at.to_owned(), ..Default::default() });
        } else if let Some(code) = message.strip_prefix("Exited with code ") {
            let code = code.trim_end_matches('.').trim().parse::<i64>().ok();
            match runs.last_mut().filter(|r| r.ended.is_empty()) {
                Some(run) => {
                    run.ended = at.to_owned();
                    run.exit = code;
                }
                None => runs.push(LogRun { started: at.to_owned(), ended: at.to_owned(), exit: code, ..Default::default() }),
            }
        } else if let Some(error) = message.strip_prefix("Launch failed: ") {
            match runs.last_mut().filter(|r| r.ended.is_empty()) {
                Some(run) => run.error = error.to_owned(),
                None => runs.push(LogRun { started: at.to_owned(), error: error.to_owned(), ..Default::default() }),
            }
        }
    }
    // A run whose launcher died leaves no exit line; a newer start closes it.
    let count = runs.len();
    for (i, run) in runs.iter_mut().enumerate() {
        if i + 1 < count && run.ended.is_empty() {
            run.ended = run.started.clone();
            if run.error.is_empty() {
                run.error = "no exit recorded (stopped or the machine went down)".into();
            }
        }
    }
    runs.reverse();
    runs
}

/// The last `max` bytes of a file (lossy UTF-8), from a line start.
pub fn tail(path: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 { text.split_once('\n').map(|(_, rest)| rest.to_owned()).unwrap_or(text) } else { text }
}

pub fn read_report(path: &Path) -> Option<Report> {
    let bytes = std::fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(bytes).ok()
}

/// Newest file matching a glob (`*` within path segments).
pub fn newest_match(pattern: &str) -> Option<PathBuf> {
    let pattern = pattern.replace('/', "\\");
    if !pattern.contains('*') {
        let path = PathBuf::from(&pattern);
        return path.is_file().then_some(path);
    }
    let parts: Vec<&str> = pattern.split('\\').collect();
    let first_glob = parts.iter().position(|p| p.contains('*'))?;
    let mut candidates = vec![PathBuf::from(parts[..first_glob].join("\\") + "\\")];
    for (i, part) in parts.iter().enumerate().skip(first_glob) {
        let last = i + 1 == parts.len();
        let mut next = Vec::new();
        for dir in &candidates {
            if !part.contains('*') {
                next.push(dir.join(part));
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if wildcard(part, &name) {
                    next.push(entry.path());
                }
            }
        }
        if !last && next.len() > 400 {
            // Keep the newest directories only (run folders grow without bound).
            next.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
            next.truncate(400);
        }
        candidates = next;
    }
    candidates.into_iter().filter(|p| p.is_file()).max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
    let pieces: Vec<&str> = p.split('*').collect();
    let mut pos = 0;
    for (i, piece) in pieces.iter().enumerate() {
        if piece.is_empty() {
            continue;
        }
        if i == 0 {
            if !n.starts_with(piece) {
                return false;
            }
            pos = piece.len();
        } else if let Some(found) = n[pos..].find(piece) {
            pos += found + piece.len();
        } else {
            return false;
        }
    }
    pieces.last().is_none_or(|last| last.is_empty() || n.ends_with(last))
}

// ---------- Task Scheduler XML ----------

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// `PT3H`, `PT1M`, `P3D`, `PT0S` → ms.
pub fn iso_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim().strip_prefix('P')?;
    let (days, time) = text.split_once('T').unwrap_or((text, ""));
    let mut ms = 0u64;
    let mut take = |part: &str, units: &[(char, u64)]| -> Option<()> {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                number.push(c);
            } else {
                let unit = units.iter().find(|(u, _)| *u == c)?.1;
                ms += number.parse::<u64>().ok()? * unit;
                number.clear();
            }
        }
        number.is_empty().then_some(())
    };
    take(days, &[('D', 86_400_000), ('W', 7 * 86_400_000)])?;
    take(time, &[('H', 3_600_000), ('M', 60_000), ('S', 1_000)])?;
    Some(ms)
}

pub fn iso_duration(ms: u64) -> String {
    if ms == 0 {
        return "PT0S".into();
    }
    let (d, rest) = (ms / 86_400_000, ms % 86_400_000);
    let (h, rest) = (rest / 3_600_000, rest % 3_600_000);
    let (m, s) = (rest / 60_000, (rest % 60_000) / 1000);
    let mut out = String::from("P");
    if d > 0 {
        out.push_str(&format!("{d}D"));
    }
    if h + m + s > 0 {
        out.push('T');
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        if s > 0 {
            out.push_str(&format!("{s}S"));
        }
    }
    out
}

/// Who the generated task runs as (the current user).
#[derive(Clone, Debug, Default)]
pub struct Principal {
    /// SID or DOMAIN\user for the principal.
    pub user_id: String,
    /// DOMAIN\user for logon triggers.
    pub logon_user: String,
}

impl Principal {
    pub fn current() -> Principal {
        let user = format!("{}\\{}", std::env::var("USERDOMAIN").unwrap_or_default(), std::env::var("USERNAME").unwrap_or_default());
        Principal { user_id: user.clone(), logon_user: user }
    }
}

/// What Task Scheduler runs for a manifest: the hidden launcher with the task's
/// JSON, or the command itself when there is no launcher.
pub fn action(m: &Manifest, d: &Defaults) -> (String, String) {
    if d.launcher.is_empty() || d.python.is_empty() {
        (m.command.clone(), m.args.clone())
    } else {
        (d.python.clone(), format!("\"{}\" \"{}\"", d.launcher, d.task_json(&m.name).display()))
    }
}

/// The launcher's per-task file.
pub fn task_json(m: &Manifest, d: &Defaults) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "name": m.name,
        "executable": m.command,
        "arguments": m.args,
        "workingDirectory": m.cwd,
        "logPath": d.log_path(&m.name).display().to_string(),
        "statusPath": d.status_path(&m.name).display().to_string(),
        "manifest": m.path.display().to_string(),
        "reportCommand": m.report,
    }))
    .unwrap_or_default()
}

/// Task Scheduler XML. Task Scheduler treats a start time without an offset
/// as UTC and one with an offset as a fixed instant, so times are written with
/// the current local offset; after a daylight-saving change they show as drift
/// until applied again (a nightly `apply --yes` keeps them on local time).
pub fn task_xml(m: &Manifest, d: &Defaults, who: &Principal, offset_minutes: i32) -> String {
    let local = crate::bus::Local::now();
    let zone = format!("{}{:02}:{:02}", if offset_minutes < 0 { '-' } else { '+' }, offset_minutes.abs() / 60, offset_minutes.abs() % 60);
    let start = |minutes: u32| format!("{}T{}:00{zone}", local.date(), hhmm(minutes));
    let mut triggers = String::new();
    for t in &m.schedule {
        match t {
            Trigger::Daily(min) => triggers.push_str(&format!(
                "    <CalendarTrigger>\n      <StartBoundary>{}</StartBoundary>\n      <ScheduleByDay>\n        <DaysInterval>1</DaysInterval>\n      </ScheduleByDay>\n    </CalendarTrigger>\n",
                start(*min)
            )),
            Trigger::Weekly(days, min) => {
                let days: String = days
                    .iter()
                    .filter_map(|d| DAYS.iter().position(|x| x == d).map(|i| format!("          <{} />\n", XML_DAYS[i])))
                    .collect();
                triggers.push_str(&format!(
                    "    <CalendarTrigger>\n      <StartBoundary>{}</StartBoundary>\n      <ScheduleByWeek>\n        <WeeksInterval>1</WeeksInterval>\n        <DaysOfWeek>\n{days}        </DaysOfWeek>\n      </ScheduleByWeek>\n    </CalendarTrigger>\n",
                    start(*min)
                ));
            }
            Trigger::Every(ms, from) => triggers.push_str(&format!(
                "    <TimeTrigger>\n      <StartBoundary>{}</StartBoundary>\n      <Repetition>\n        <Interval>{}</Interval>\n        <StopAtDurationEnd>false</StopAtDurationEnd>\n      </Repetition>\n    </TimeTrigger>\n",
                start(*from),
                iso_duration(*ms)
            )),
            Trigger::Logon => triggers.push_str(&format!("    <LogonTrigger>\n      <UserId>{}</UserId>\n    </LogonTrigger>\n", xml_escape(&who.logon_user))),
        }
    }
    let policy = match m.overlap.as_str() {
        "queue" => "Queue",
        "parallel" => "Parallel",
        "stop" => "StopExisting",
        _ => "IgnoreNew",
    };
    let restart = if m.restart_count > 0 {
        format!("    <RestartOnFailure>\n      <Count>{}</Count>\n      <Interval>{}</Interval>\n    </RestartOnFailure>\n", m.restart_count, iso_duration(m.restart_every_ms))
    } else {
        String::new()
    };
    let (command, args) = action(m, d);
    let description = if m.description.is_empty() { format!("{} (managed by Copilot Canvas from {})", m.name, m.path.display()) } else { m.description.clone() };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.3\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <RegistrationInfo>\n    <Description>{description}</Description>\n    <Source>Copilot Canvas</Source>\n    <URI>\\{name}</URI>\n  </RegistrationInfo>\n  <Principals>\n    <Principal id=\"Author\">\n      <UserId>{user}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n    </Principal>\n  </Principals>\n  <Settings>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <ExecutionTimeLimit>{limit}</ExecutionTimeLimit>\n    <MultipleInstancesPolicy>{policy}</MultipleInstancesPolicy>\n{restart}    <StartWhenAvailable>{swa}</StartWhenAvailable>\n    <Enabled>{enabled}</Enabled>\n    <IdleSettings>\n      <StopOnIdleEnd>false</StopOnIdleEnd>\n      <RestartOnIdle>false</RestartOnIdle>\n    </IdleSettings>\n    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>\n  </Settings>\n  <Triggers>\n{triggers}  </Triggers>\n  <Actions Context=\"Author\">\n    <Exec>\n      <Command>{command}</Command>\n      <Arguments>{args}</Arguments>\n{cwd}    </Exec>\n  </Actions>\n</Task>\n",
        description = xml_escape(&description),
        name = xml_escape(&m.name),
        user = xml_escape(&who.user_id),
        limit = iso_duration(m.time_limit_ms),
        swa = m.start_when_available,
        enabled = m.enabled,
        command = xml_escape(&command),
        args = xml_escape(&args),
        cwd = if m.cwd.is_empty() { String::new() } else { format!("      <WorkingDirectory>{}</WorkingDirectory>\n", xml_escape(&m.cwd)) },
    )
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(&xml[start..end])
}

fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        // `<LogonTriggerX` is a different element.
        if !after.starts_with(['>', ' ', '/']) {
            rest = after;
            continue;
        }
        if let Some(after_close) = after.strip_prefix("/>") {
            out.push("");
            rest = after_close;
            continue;
        }
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

/// `2026-10-08T13:45:00+00:00` → local minutes after midnight (at the current
/// offset), and whether it carried an offset. Task Scheduler reads a time
/// without an offset as UTC, so this does too.
fn local_minutes(boundary: &str, offset_minutes: i32) -> Option<(u32, bool)> {
    let time = boundary.split_once('T')?.1;
    let (clock, zone) = match time.find(['+', '-', 'Z']) {
        Some(i) => (&time[..i], Some(&time[i..])),
        None => (time, None),
    };
    let minutes = crate::bus::parse_hhmm(&clock[..clock.len().min(5)])? as i32;
    let Some(zone) = zone else { return Some(((minutes + offset_minutes).rem_euclid(1440) as u32, false)) };
    let zone_minutes = if zone == "Z" {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        sign * crate::bus::parse_hhmm(&zone[1..]).unwrap_or(0) as i32
    };
    let local = (minutes - zone_minutes + offset_minutes).rem_euclid(1440);
    Some((local as u32, true))
}

/// The current local UTC offset in minutes.
pub fn local_offset_minutes() -> i32 {
    // SAFETY: plain out-parameter calls.
    let (local, utc) = unsafe {
        let mut l = std::mem::zeroed();
        let mut u = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut l);
        windows_sys::Win32::System::SystemInformation::GetSystemTime(&mut u);
        (l, u)
    };
    let minutes = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| t.wHour as i32 * 60 + t.wMinute as i32;
    let mut diff = minutes(&local) - minutes(&utc);
    if local.wDay != utc.wDay {
        diff += if (local.wYear, local.wMonth, local.wDay) > (utc.wYear, utc.wMonth, utc.wDay) { 1440 } else { -1440 };
    }
    diff
}

/// A registered task read back as a manifest (import and drift checks).
#[derive(Clone, Debug, Default)]
pub struct Imported {
    pub manifest: Manifest,
    /// Times are fixed to a UTC offset (they shift with daylight saving).
    pub utc_anchored: bool,
    /// The launcher in front of the command, when there is one.
    pub defaults: Option<Defaults>,
    pub user_id: String,
    pub logon_user: String,
    /// Triggers this tool cannot express (kept out of the manifest).
    pub unsupported: Vec<String>,
}

pub fn import_xml(name: &str, xml: &str, offset_minutes: i32) -> Imported {
    let mut out = Imported::default();
    let m = &mut out.manifest;
    m.name = name.to_owned();
    m.description = tag(xml, "Description").map(xml_unescape).unwrap_or_default();
    let settings = tag(xml, "Settings").unwrap_or_default();
    m.enabled = tag(settings, "Enabled").is_none_or(|e| e.trim() != "false");
    m.time_limit_ms = tag(settings, "ExecutionTimeLimit").and_then(iso_duration_ms).unwrap_or(72 * 3_600_000);
    m.overlap = match tag(settings, "MultipleInstancesPolicy").unwrap_or("IgnoreNew") {
        "Queue" => "queue",
        "Parallel" => "parallel",
        "StopExisting" => "stop",
        _ => "ignore",
    }
    .into();
    if let Some(restart) = tag(settings, "RestartOnFailure") {
        m.restart_count = tag(restart, "Count").and_then(|c| c.trim().parse().ok()).unwrap_or(0);
        m.restart_every_ms = tag(restart, "Interval").and_then(iso_duration_ms).unwrap_or(60_000);
    }
    m.start_when_available = tag(settings, "StartWhenAvailable").is_some_and(|v| v.trim() == "true");
    if let Some(principal) = tag(xml, "Principals") {
        out.user_id = tag(principal, "UserId").unwrap_or_default().to_owned();
    }
    let triggers = tag(xml, "Triggers").unwrap_or_default();
    for block in blocks(triggers, "LogonTrigger") {
        if out.logon_user.is_empty() {
            out.logon_user = tag(block, "UserId").unwrap_or_default().to_owned();
        }
        m.schedule.push(Trigger::Logon);
    }
    let mut anchored = false;
    for block in blocks(triggers, "CalendarTrigger") {
        let Some((minutes, utc)) = tag(block, "StartBoundary").and_then(|b| local_minutes(b, offset_minutes)) else { continue };
        anchored |= utc;
        if let Some(day) = tag(block, "ScheduleByDay") {
            let daily = tag(day, "DaysInterval").is_none_or(|d| d.trim() == "1");
            match tag(block, "Repetition") {
                None if daily => {
                    m.schedule.push(Trigger::Daily(minutes));
                    continue;
                }
                // Daily, repeating all day: the same as a plain interval.
                Some(rep) if daily => {
                    let interval = tag(rep, "Interval").and_then(iso_duration_ms).unwrap_or(0);
                    let all_day = tag(rep, "Duration").and_then(iso_duration_ms).is_none_or(|d| d >= 86_400_000);
                    if interval > 0 && all_day {
                        m.schedule.push(Trigger::Every(interval, minutes % (interval / 60_000).clamp(1, 1440) as u32));
                        continue;
                    }
                }
                _ => {}
            }
        } else if let Some(week) = tag(block, "ScheduleByWeek")
            && tag(week, "WeeksInterval").is_none_or(|w| w.trim() == "1")
            && tag(block, "Repetition").is_none()
        {
            let days: Vec<String> = XML_DAYS.iter().enumerate().filter(|(_, d)| week.contains(&format!("<{d}"))).map(|(i, _)| DAYS[i].to_owned()).collect();
            if !days.is_empty() {
                m.schedule.push(Trigger::Weekly(days, minutes));
                continue;
            }
        }
        out.unsupported.push(format!("CalendarTrigger {}", block.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    for block in blocks(triggers, "TimeTrigger") {
        let Some((minutes, utc)) = tag(block, "StartBoundary").and_then(|b| local_minutes(b, offset_minutes)) else { continue };
        anchored |= utc;
        match tag(block, "Repetition").and_then(|r| tag(r, "Interval")).and_then(iso_duration_ms) {
            Some(interval) if tag(block, "Repetition").and_then(|r| tag(r, "Duration")).is_none() => {
                m.schedule.push(Trigger::Every(interval, minutes % (interval / 60_000).clamp(1, 1440) as u32));
            }
            _ => out.unsupported.push(format!("TimeTrigger {}", block.split_whitespace().collect::<Vec<_>>().join(" "))),
        }
    }
    for other in ["BootTrigger", "IdleTrigger", "EventTrigger", "RegistrationTrigger", "SessionStateChangeTrigger"] {
        if !blocks(triggers, other).is_empty() {
            out.unsupported.push(other.to_owned());
        }
    }
    out.utc_anchored = anchored;
    let exec = tag(xml, "Exec").unwrap_or_default();
    let command = tag(exec, "Command").map(xml_unescape).unwrap_or_default();
    let args = tag(exec, "Arguments").map(xml_unescape).unwrap_or_default();
    m.cwd = tag(exec, "WorkingDirectory").map(xml_unescape).unwrap_or_default();
    // The hidden launcher: `pythonw "launcher.py" "<state>\tasks\<name>.json"`.
    let quoted: Vec<&str> = args.split('"').map(str::trim).filter(|s| !s.is_empty()).collect();
    let launcher = quoted.iter().find(|s| s.to_ascii_lowercase().ends_with(".py")).map(|s| s.to_string());
    let config = quoted.iter().find(|s| s.to_ascii_lowercase().ends_with(".json")).map(PathBuf::from);
    match (launcher, config.as_ref().and_then(|c| std::fs::read(c).ok().map(|b| (c.clone(), b)))) {
        (Some(launcher), Some((config, bytes))) => {
            let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes).to_vec();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            m.command = json["executable"].as_str().unwrap_or_default().to_owned();
            m.args = json["arguments"].as_str().unwrap_or_default().to_owned();
            if let Some(cwd) = json["workingDirectory"].as_str().filter(|c| !c.is_empty()) {
                m.cwd = cwd.to_owned();
            }
            let state = config.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(default_state);
            out.defaults = Some(Defaults { launcher, python: command, state });
        }
        _ => {
            m.command = command;
            m.args = args;
        }
    }
    out
}

/// What differs between a manifest and its registered task.
pub fn drift(m: &Manifest, d: &Defaults, registered: &Imported) -> Vec<String> {
    let r = &registered.manifest;
    let mut out = Vec::new();
    let mut check = |what: &str, want: String, have: String| {
        if want != have {
            out.push(format!("{what}: {have} → {want}"));
        }
    };
    check("enabled", m.enabled.to_string(), r.enabled.to_string());
    check("command", m.command.clone(), r.command.clone());
    check("arguments", m.args.clone(), r.args.clone());
    check("folder", m.cwd.to_lowercase(), r.cwd.to_lowercase());
    let sorted = |s: &[Trigger]| {
        let mut v: Vec<String> = s.iter().map(Trigger::text).collect();
        v.sort();
        v.join(" · ")
    };
    check("schedule", sorted(&m.schedule), sorted(&r.schedule));
    check("time limit", format_ms(m.time_limit_ms), format_ms(r.time_limit_ms));
    check("overlap", m.overlap.clone(), r.overlap.clone());
    check("restart on failure", format!("{} every {}", m.restart_count, format_ms(m.restart_every_ms)), if r.restart_count == 0 && m.restart_count == 0 {
        format!("{} every {}", m.restart_count, format_ms(m.restart_every_ms))
    } else {
        format!("{} every {}", r.restart_count, format_ms(r.restart_every_ms))
    });
    check("start when available", m.start_when_available.to_string(), r.start_when_available.to_string());
    match &registered.defaults {
        Some(have) if !d.launcher.is_empty() => {
            check("launcher", d.launcher.to_lowercase(), have.launcher.to_lowercase());
            check("python", d.python.to_lowercase(), have.python.to_lowercase());
        }
        None if !d.launcher.is_empty() => out.push("launcher: none → hidden launcher".into()),
        _ => {}
    }
    for u in &registered.unsupported {
        out.push(format!("removes unsupported trigger {u}"));
    }
    out
}

// ---------- runtime view ----------

pub fn live(repo: &Repo, tasks: &BTreeMap<String, crate::taskschd::TaskInfo>) -> Vec<Live> {
    let offset = local_offset_minutes();
    repo.manifests
        .iter()
        .map(|m| {
            let log_path = repo.defaults.log_path(&m.name);
            let task = tasks.get(&m.name.to_lowercase()).cloned();
            let mut runs: Vec<LogRun> = parse_runs(&tail(&log_path, 64 * 1024)).into_iter().take(40).collect();
            // A run without an exit line is only running while the task runs.
            if let Some(open) = runs.first_mut().filter(|r| r.ended.is_empty() && r.error.is_empty())
                && task.as_ref().is_some_and(|t| t.state != "running")
            {
                open.ended = open.started.clone();
                open.error = "no exit recorded (stopped or the machine went down)".into();
            }
            let drift = match &task {
                None => vec!["not registered in Task Scheduler (run apply)".into()],
                Some(t) => drift(m, &repo.defaults, &import_xml(&m.name, &t.xml, offset)),
            };
            Live {
                manifest: m.clone(),
                runs,
                report: read_report(&repo.defaults.status_path(&m.name)),
                status_path: repo.defaults.status_path(&m.name),
                logs: m.logs.iter().filter_map(|p| newest_match(p)).collect(),
                log_path,
                task,
                drift,
            }
        })
        .collect()
}

// ---------- status reports written by automations ----------

/// Writes a report for the running automation (`AUTOMATION_STATUS_FILE`).
pub fn write_report(path: &Path, report: &Report) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(report)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "name: 'Morning'\nfamily: 'Pipeline'\nenabled: true\nai: true\ncommand: 'C:\\pwsh.exe'\nargs: '-File \"C:\\run.ps1\" -Kind morning'\ncwd: 'C:\\notes'\nschedule:\n  - daily 06:45\n  - logon\n  - every 2h from 00:10\n  - weekly fri 16:00\ntime_limit: 3h\noverlap: ignore\nrestart_on_failure: 3 every 1m\nstart_when_available: true\nreport: '\"py\" status.py'\nsuccess_codes: [0, 1]\ndocs:\n  - 'C:\\Policy.md'\nlogs: ['C:\\runs\\*_morning_*\\stderr.log']\noutputs:\n  - title: 'Brief'\n    path: 'C:\\brief.md'\n";

    #[test]
    fn manifest_round_trips_through_yaml() {
        let m = parse_manifest(MANIFEST, Path::new("C:\\a\\Pipeline\\morning.yaml")).unwrap();
        assert_eq!(m.schedule, vec![Trigger::Daily(405), Trigger::Logon, Trigger::Every(7_200_000, 10), Trigger::Weekly(vec!["fri".into()], 960)]);
        assert_eq!((m.time_limit_ms, m.restart_count, m.restart_every_ms, m.ai), (3 * 3_600_000, 3, 60_000, true));
        assert_eq!(m.outputs, vec![("Brief".to_owned(), "C:\\brief.md".to_owned())]);
        let again = parse_manifest(&manifest_yaml(&m), &m.path).unwrap();
        assert_eq!(again, m);
        assert!(Trigger::parse("hourly").is_err());
        assert!(parse_manifest("name: 'a/b'\n", Path::new("x.yaml")).is_err(), "names become task paths");
    }

    #[test]
    fn generated_xml_imports_back_without_drift() {
        let m = parse_manifest(MANIFEST, Path::new("C:\\a\\Pipeline\\morning.yaml")).unwrap();
        let d = Defaults { launcher: "Q:\\launch.py".into(), python: "C:\\py\\pythonw.exe".into(), state: std::env::temp_dir().join(format!("cc-fac-{}", uuid::Uuid::new_v4())) };
        std::fs::create_dir_all(d.state.join("tasks")).unwrap();
        std::fs::write(d.task_json(&m.name), task_json(&m, &d)).unwrap();
        let xml = task_xml(&m, &d, &Principal { user_id: "S-1-5-21-1".into(), logon_user: "DOM\\me".into() }, -420);
        assert!(xml.contains("T06:45:00-07:00</StartBoundary>"), "local time with the current offset");
        let imported = import_xml(&m.name, &xml, -420);
        assert_eq!(drift(&m, &d, &import_xml(&m.name, &xml, -480)).len(), 1, "after a daylight-saving change the time drifts");
        assert_eq!(drift(&m, &d, &imported), Vec::<String>::new());
        assert_eq!(imported.logon_user, "DOM\\me");
        let _ = std::fs::remove_dir_all(&d.state);
    }

    #[test]
    fn utc_anchored_tasks_import_at_local_time_and_report_drift() {
        let xml = "<Task><RegistrationInfo><Description>x &amp; y</Description></RegistrationInfo><Settings><ExecutionTimeLimit>PT3H</ExecutionTimeLimit><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><StartWhenAvailable>true</StartWhenAvailable></Settings><Triggers><CalendarTrigger><StartBoundary>2026-10-08T13:45:00+00:00</StartBoundary><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger><LogonTrigger><UserId>DOM\\me</UserId></LogonTrigger><TimeTrigger><StartBoundary>2026-10-08T17:00:00+00:00</StartBoundary><Repetition><Interval>PT2H</Interval><StopAtDurationEnd>true</StopAtDurationEnd></Repetition></TimeTrigger></Triggers><Actions><Exec><Command>C:\\x.exe</Command><Arguments>-a</Arguments></Exec></Actions></Task>";
        let i = import_xml("T", xml, -420);
        assert!(i.utc_anchored);
        assert_eq!(i.manifest.description, "x & y");
        assert_eq!(i.manifest.schedule, vec![Trigger::Logon, Trigger::Daily(6 * 60 + 45), Trigger::Every(7_200_000, 0)]);
        assert_eq!((i.manifest.command.as_str(), i.manifest.args.as_str()), ("C:\\x.exe", "-a"));
        let d = Defaults::default();
        assert!(drift(&i.manifest, &d, &i).is_empty());
        let every = "<Task><Triggers><CalendarTrigger><StartBoundary>2026-09-25T07:03:00+00:00</StartBoundary><Repetition><Interval>PT5M</Interval><Duration>P1D</Duration></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger></Triggers></Task>";
        assert_eq!(import_xml("W", every, -420).manifest.schedule, vec![Trigger::Every(300_000, 3)], "daily repeating all day is an interval");
    }

    #[test]
    fn launcher_log_becomes_runs_newest_first() {
        let log = "2026-10-26T07:18:36-07:00 Starting task.\n2026-10-26T07:18:36-07:00 Started PID 1.\n2026-10-26T07:30:29-07:00 Exited with code 1.\n2026-10-27T06:45:01-07:00 Starting task.\n2026-10-27T06:45:02-07:00 Launch failed: FileNotFoundError\n2026-10-27T06:45:02-07:00 Exited with code 1.\n2026-10-27T08:00:00-07:00 Starting task.\n2026-10-27T09:00:00-07:00 Starting task.\n";
        let runs = parse_runs(log);
        assert_eq!(runs.len(), 4);
        assert_eq!((runs[0].ended.as_str(), runs[0].exit), ("", None), "still running");
        assert!(runs[1].error.starts_with("no exit"), "superseded without an exit line");
        assert_eq!((runs[2].exit, runs[2].error.as_str()), (Some(1), "FileNotFoundError"));
        assert_eq!(runs[3].exit, Some(1));
    }

    #[test]
    fn durations_and_globs() {
        assert_eq!(iso_duration_ms("PT3H"), Some(10_800_000));
        assert_eq!(iso_duration_ms("P3D"), Some(3 * 86_400_000));
        assert_eq!(iso_duration_ms("PT0S"), Some(0));
        assert_eq!(iso_duration(5_400_000), "PT1H30M");
        assert!(wildcard("*_morning_*", "20261027T1349182_morning_ab"));
        assert!(!wildcard("*_morning_*", "20261027_collector_ab"));
        assert!(wildcard("stderr.log", "STDERR.LOG"));
    }
}
