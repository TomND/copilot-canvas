//! The reports inbox on disk, per canvas home (`<home>/reports/`):
//!
//! - `spool/<id>.json`: reports `copilot-canvas report publish` handed over
//!   (a [`Submission`]); the CLI only ever writes here, atomically, so a report
//!   is never lost while the service is down or restarting.
//! - `items/<id>/meta.json` and `body.md|body.html|body.txt`: stored reports.
//!   Only the service writes them (it checks every submission again first);
//!   a report folder appears by an atomic rename, so readers (the
//!   in-process backend of a local client, `report list`) never see half of one.
//! - `state.json`: read state and reports refused after they were queued.
//! - `service.json`: the running service (the CLI waits for it to store a
//!   report, or says "queued" at once when no service runs).
//!
//! Retention: read reports go after [`KEEP_READ_DAYS`] days or beyond the
//! newest [`KEEP_READ`]; unread ones are never removed automatically (a
//! full inbox refuses new reports instead, loudly).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_model::bus::Routing;
use canvas_model::reports::{
    self, Content, Format, KEEP_READ, KEEP_READ_DAYS, Link, MAX_BODY, MAX_UNREAD, Rejected, Report, SNAPSHOT_ITEMS, SUBMISSION_VERSION, Snapshot, Submission,
};
use serde::{Deserialize, Serialize};

use crate::paths::now_ms;

/// Refused reports remembered for the inbox and `report list`.
const KEEP_REJECTED: usize = 20;
/// A `.rejected` answer nobody picked up goes after a day.
const STALE_ANSWER: Duration = Duration::from_secs(24 * 3600);

pub fn dir(home: &Path) -> PathBuf {
    home.join("reports")
}

pub fn spool_dir(home: &Path) -> PathBuf {
    dir(home).join("spool")
}

pub fn items_dir(home: &Path) -> PathBuf {
    dir(home).join("items")
}

fn state_path(home: &Path) -> PathBuf {
    dir(home).join("state.json")
}

fn alive_path(home: &Path) -> PathBuf {
    dir(home).join("service.json")
}

/// Where the service answers a refused submission (the waiting CLI reads it).
fn answer_path(home: &Path, id: &str) -> PathBuf {
    spool_dir(home).join(format!("{id}.rejected"))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct State {
    /// Report id → when it was read.
    #[serde(default)]
    read: BTreeMap<String, u64>,
    #[serde(default)]
    rejected: Vec<Rejected>,
}

#[derive(Serialize, Deserialize)]
struct Alive {
    pid: u32,
    started_ms: u64,
}

/// The service announces itself (the CLI then waits for it to store reports).
pub fn announce_service(home: &Path) -> Result<()> {
    let pid = std::process::id();
    let alive = Alive { pid, started_ms: canvas_sys::process::process_start_ms(pid).unwrap_or(0) };
    crate::paths::write_atomic(&alive_path(home), &serde_json::to_vec(&alive)?)?;
    Ok(())
}

/// A service of this home runs (its process, not a recycled pid).
pub fn service_alive(home: &Path) -> bool {
    let Some(alive) = std::fs::read(alive_path(home)).ok().and_then(|b| serde_json::from_slice::<Alive>(&b).ok()) else { return false };
    alive.pid != 0
        && canvas_sys::process::alive(alive.pid)
        && canvas_sys::process::process_start_ms(alive.pid).is_some_and(|started| alive.started_ms == 0 || started.abs_diff(alive.started_ms) < 2_000)
}

/// The stored reports (newest first) and the state, read only.
fn load(home: &Path) -> (Vec<Report>, State) {
    let mut reports: Vec<Report> = std::fs::read_dir(items_dir(home))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .filter_map(|e| std::fs::read(e.path().join("meta.json")).ok())
                .filter_map(|bytes| serde_json::from_slice::<Report>(&bytes).ok())
                .filter(|r| reports::valid_id(&r.id))
                .collect()
        })
        .unwrap_or_default();
    sort(&mut reports);
    let state: State = std::fs::read(state_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for report in &mut reports {
        report.read_ms = state.read.get(&report.id).copied().unwrap_or(0);
    }
    (reports, state)
}

fn sort(reports: &mut [Report]) {
    reports.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.id.cmp(&a.id)));
}

fn snapshot_of(version: u64, reports: &[Report], rejected: &[Rejected]) -> Snapshot {
    Snapshot {
        version,
        reports: reports.iter().take(SNAPSHOT_ITEMS).cloned().collect(),
        total: reports.len() as u32,
        unread: reports.iter().filter(|r| r.unread()).count() as u32,
        rejected: rejected.to_vec(),
    }
}

/// The inbox as stored now (`Request::Reports`; read only).
pub fn read_snapshot(home: &Path) -> Snapshot {
    let (reports, state) = load(home);
    snapshot_of(0, &reports, &state.rejected)
}

/// One stored report's body (`Request::ReportContent`; read only).
pub fn content(home: &Path, id: &str) -> Result<Content> {
    anyhow::ensure!(reports::valid_id(id), "invalid report id {id:?}");
    let folder = items_dir(home).join(id);
    let meta: Report = serde_json::from_slice(&std::fs::read(folder.join("meta.json")).with_context(|| format!("report {id} is not in the inbox (deleted?)"))?)
        .with_context(|| format!("report {id} is unreadable"))?;
    let body = std::fs::read(folder.join(meta.format.file())).with_context(|| format!("the body of report {id} is missing"))?;
    Ok(Content { id: id.to_owned(), format: meta.format, body: String::from_utf8_lossy(&body).into_owned() })
}

/// What one pass over the spool did (for the service log).
#[derive(Debug, Default, PartialEq)]
pub struct Ingested {
    pub stored: Vec<Report>,
    pub rejected: Vec<Rejected>,
    /// Submissions that could not be written (kept for a later try): (id, why).
    pub failed: Vec<(String, String)>,
}

impl Ingested {
    pub fn changed(&self) -> bool {
        !self.stored.is_empty() || !self.rejected.is_empty()
    }
}

/// The inbox the service owns: it stores spooled reports and keeps the read state.
pub struct Store {
    home: PathBuf,
    /// Newest first, with their read state.
    reports: Vec<Report>,
    state: State,
    /// Bumped on every change (clients see it in the snapshot).
    pub version: u64,
    /// Submissions whose write failed: when to try again.
    retry: BTreeMap<String, Instant>,
}

impl Store {
    /// Loads the inbox (leftovers of interrupted writes are removed and the
    /// retention applied).
    pub fn open(home: &Path, now: u64) -> Self {
        let (reports, mut state) = load(home);
        state.read.retain(|id, _| reports.iter().any(|r| &r.id == id));
        let mut store = Store { home: home.to_owned(), reports, state, version: 1, retry: BTreeMap::new() };
        if let Ok(entries) = std::fs::read_dir(items_dir(home)) {
            for entry in entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with('.')) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        store.retain(now);
        store
    }

    pub fn snapshot(&self) -> Snapshot {
        snapshot_of(self.version, &self.reports, &self.state.rejected)
    }

    pub fn reports(&self) -> &[Report] {
        &self.reports
    }

    fn unread(&self) -> usize {
        self.reports.iter().filter(|r| r.unread()).count()
    }

    fn save_state(&self) -> Result<()> {
        crate::paths::write_atomic(&state_path(&self.home), &serde_json::to_vec_pretty(&self.state)?)?;
        Ok(())
    }

    fn changed(&mut self) {
        self.version += 1;
        let _ = self.save_state();
    }

    /// Stores the reports waiting in the spool (oldest first).
    pub fn ingest(&mut self, routing: &Routing, now: u64) -> Ingested {
        let mut done = Ingested::default();
        let spool = spool_dir(&self.home);
        let Ok(entries) = std::fs::read_dir(&spool) else { return done };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = || entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_ANSWER);
            match path.extension().and_then(|e| e.to_str()) {
                Some("json") => files.push(path),
                // Answers nobody picked up, and abandoned temporary files.
                Some("rejected") | Some("tmp") if stale() => {
                    let _ = std::fs::remove_file(&path);
                }
                _ => {}
            }
        }
        files.sort();
        for file in files {
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if self.retry.get(&stem).is_some_and(|at| Instant::now() < *at) {
                continue;
            }
            let submission = std::fs::read(&file).map_err(anyhow::Error::from).and_then(|bytes| serde_json::from_slice::<Submission>(&bytes).context("unreadable report file"));
            let checked = submission.and_then(|s| {
                s.validate()?;
                anyhow::ensure!(s.id == stem, "the report file's name does not match its id");
                Ok(s)
            });
            let submission = match checked {
                Ok(s) => s,
                Err(error) => {
                    let title = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| v["title"].as_str().map(reports::clean_title)).unwrap_or_default();
                    done.rejected.push(self.reject(&stem, &title, &format!("{error:#}"), now));
                    let _ = std::fs::remove_file(&file);
                    continue;
                }
            };
            if self.reports.iter().any(|r| r.id == submission.id) {
                // Stored before (the spool file outlived a crash): nothing to do.
                let _ = std::fs::remove_file(&file);
                continue;
            }
            let unread = self.unread();
            if unread >= MAX_UNREAD {
                let why = format!("the inbox holds {unread} unread reports (at most {MAX_UNREAD}): read or delete some in Copilot Canvas › Reports first");
                done.rejected.push(self.reject(&submission.id, &reports::clean_title(&submission.title), &why, now));
                let _ = std::fs::remove_file(&file);
                continue;
            }
            let report = stored_report(&submission, routing, now);
            match self.write(&report, &submission.body) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&file);
                    self.retry.remove(&report.id);
                    self.reports.push(report.clone());
                    done.stored.push(report);
                }
                Err(error) => {
                    self.retry.insert(stem.clone(), Instant::now() + Duration::from_secs(30));
                    done.failed.push((stem, format!("{error:#}")));
                }
            }
        }
        if done.changed() {
            sort(&mut self.reports);
            self.retain(now);
            self.changed();
        }
        done
    }

    /// Records a refused submission (for the waiting CLI and the inbox).
    fn reject(&mut self, id: &str, title: &str, reason: &str, now: u64) -> Rejected {
        let rejected = Rejected { id: id.to_owned(), title: title.to_owned(), reason: reason.to_owned(), ms: now };
        if reports::valid_id(id) {
            let _ = crate::paths::write_atomic(&answer_path(&self.home, id), &serde_json::to_vec(&rejected).unwrap_or_default());
        }
        self.state.rejected.insert(0, rejected.clone());
        self.state.rejected.truncate(KEEP_REJECTED);
        rejected
    }

    /// Writes a report folder under a temporary name, then renames it into place.
    fn write(&self, report: &Report, body: &str) -> Result<()> {
        let items = items_dir(&self.home);
        std::fs::create_dir_all(&items).with_context(|| format!("cannot create {}", items.display()))?;
        let temp = items.join(format!(".{}.tmp", report.id));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp)?;
        let written = (|| -> Result<()> {
            std::fs::write(temp.join(report.format.file()), body.as_bytes())?;
            std::fs::write(temp.join("meta.json"), serde_json::to_vec_pretty(report)?)?;
            std::fs::rename(&temp, items.join(&report.id))?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        written.with_context(|| format!("cannot store report {} in {}", report.id, items.display()))
    }

    /// Marks reports read (`read`) or unread; true when anything changed.
    pub fn mark(&mut self, ids: &[String], read: bool, now: u64) -> bool {
        let mut changed = false;
        for report in self.reports.iter_mut().filter(|r| ids.contains(&r.id)) {
            if read && report.read_ms == 0 {
                report.read_ms = now.max(1);
                self.state.read.insert(report.id.clone(), report.read_ms);
                changed = true;
            } else if !read && report.read_ms != 0 {
                report.read_ms = 0;
                self.state.read.remove(&report.id);
                changed = true;
            }
        }
        if changed {
            self.changed();
        }
        changed
    }

    /// Marks every report read.
    pub fn mark_all_read(&mut self, now: u64) -> bool {
        let ids: Vec<String> = self.reports.iter().filter(|r| r.unread()).map(|r| r.id.clone()).collect();
        self.mark(&ids, true, now)
    }

    /// Deletes reports (their folders); answers the ones that went.
    pub fn delete(&mut self, ids: &[String]) -> Vec<String> {
        let mut gone = Vec::new();
        for id in ids.iter().filter(|id| reports::valid_id(id)) {
            let folder = items_dir(&self.home).join(id);
            if !folder.exists() || std::fs::remove_dir_all(&folder).is_ok() {
                gone.push(id.clone());
            }
        }
        self.reports.retain(|r| !gone.contains(&r.id));
        self.state.read.retain(|id, _| !gone.contains(id));
        // Refused ones can be dismissed the same way.
        let before = self.state.rejected.len();
        self.state.rejected.retain(|r| !ids.contains(&r.id));
        if !gone.is_empty() || before != self.state.rejected.len() {
            self.changed();
        }
        gone
    }

    /// Read reports older than [`KEEP_READ_DAYS`] or beyond the newest
    /// [`KEEP_READ`] go; unread ones always stay. Answers the removed ids.
    pub fn retain(&mut self, now: u64) -> Vec<String> {
        let cutoff = now.saturating_sub(KEEP_READ_DAYS * 86_400_000);
        let mut kept_read = 0;
        let mut drop = Vec::new();
        for report in &self.reports {
            if report.unread() {
                continue;
            }
            kept_read += 1;
            if report.created_ms < cutoff || kept_read > KEEP_READ {
                drop.push(report.id.clone());
            }
        }
        if drop.is_empty() {
            return drop;
        }
        self.delete(&drop)
    }
}

/// The stored form of a checked submission: its source resolved through the
/// canvas's routing (the box's name and group when it is on the canvas).
fn stored_report(s: &Submission, routing: &Routing, now: u64) -> Report {
    let fallback = |default: &str| -> String {
        let named = reports::clean_line(&s.source_name, 80);
        if named.is_empty() { default.to_owned() } else { named }
    };
    let (source_name, source_kind, group) = if let Some(spec) = (!s.passive.is_empty()).then(|| routing.passive.iter().find(|p| p.key == s.passive)).flatten() {
        (spec.name.clone(), "passive", spec.group.clone())
    } else if let Some(spec) = (!s.source.is_empty()).then(|| routing.passive_by_session(&s.source)).flatten() {
        (spec.name.clone(), "passive", spec.group.clone())
    } else if !s.source.is_empty() {
        let name = routing.names.get(&s.source).cloned().unwrap_or_else(|| fallback(&format!("session {}", s.source.chars().take(8).collect::<String>())));
        (name, if s.passive.is_empty() { "session" } else { "passive" }, routing.members.get(&s.source).cloned().unwrap_or_default())
    } else {
        (fallback("outside the canvas"), "external", String::new())
    };
    Report {
        id: s.id.clone(),
        title: reports::clean_title(&s.title),
        format: s.format,
        kind: s.kind.clone(),
        created_ms: if s.created_ms == 0 || s.created_ms > now + 60_000 { now } else { s.created_ms },
        stored_ms: now,
        source: s.source.clone(),
        source_name: reports::clean_line(&source_name, 80),
        source_kind: source_kind.to_owned(),
        group_name: routing.group(&group).map(|g| g.name.clone()).unwrap_or_default(),
        group,
        session: reports::clean_line(&s.session, 200),
        pr: s.pr.clone(),
        links: s.links.clone(),
        thread: s.thread.clone(),
        preview: reports::preview(s.format, &s.body),
        size: s.body.len() as u64,
        read_ms: 0,
    }
}

// ---------- the CLI: copilot-canvas report publish|list ----------

/// `copilot-canvas report publish` options.
#[derive(Clone, Debug, Default)]
pub struct PublishArgs {
    pub title: Option<String>,
    /// A Markdown file, or `-` for standard input.
    pub markdown: Option<String>,
    /// An HTML file, or `-`.
    pub html: Option<String>,
    pub message: Option<String>,
    pub kind: Option<String>,
    /// `TITLE=URL` each.
    pub links: Vec<String>,
    pub pr: Option<String>,
    pub session: Option<String>,
    pub thread: Option<String>,
    pub source: Option<String>,
}

/// What became of a published report.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// In the inbox now.
    Stored { id: String, title: String },
    /// Kept in the spool until a service stores it (why it was not stored yet).
    Queued { id: String, title: String, why: String, file: PathBuf },
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Stored { id, title } => format!("Report stored in the Copilot Canvas inbox: “{title}” ({id}). It shows as unread in Reports."),
            Outcome::Queued { id, title, why, file } => format!(
                "Report queued, not stored yet: “{title}” ({id}). {why}; it is kept in {} and appears in Reports when the service takes it (check with `copilot-canvas report list`).",
                file.display()
            ),
        }
    }
}

/// Reads a body: a file, or `-` for standard input (at most [`MAX_BODY`] bytes, UTF-8).
fn read_body(source: &str, what: &str) -> Result<String> {
    let mut bytes = Vec::new();
    if source == "-" {
        std::io::stdin().take(MAX_BODY as u64 + 1).read_to_end(&mut bytes).context("cannot read the report from standard input")?;
    } else {
        let size = std::fs::metadata(source).with_context(|| format!("cannot read {what} file {source}"))?.len();
        if size > MAX_BODY as u64 {
            bail!("{source} is {:.1} MiB; a report is at most {} MiB (images count: inline them small, or link to large files)", size as f64 / (1 << 20) as f64, MAX_BODY >> 20);
        }
        std::fs::File::open(source).and_then(|f| f.take(MAX_BODY as u64 + 1).read_to_end(&mut bytes)).with_context(|| format!("cannot read {what} file {source}"))?;
    }
    if bytes.len() > MAX_BODY {
        bail!("the report is larger than {} MiB", MAX_BODY >> 20);
    }
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", if source == "-" { "standard input" } else { source }))?;
    Ok(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

/// A title from the body when none was given: a message's first line, a
/// Markdown document's first heading, an HTML document's `<title>` or `<h1>`.
fn title_from(format: Format, body: &str) -> String {
    let found = match format {
        Format::Message => body.lines().find(|l| !l.trim().is_empty()).map(str::to_owned),
        Format::Markdown => body.lines().map(str::trim).find(|l| l.starts_with('#')).map(|l| l.trim_start_matches('#').trim().to_owned()),
        Format::Html => {
            let lower = body.to_ascii_lowercase();
            ["<title", "<h1"].iter().find_map(|tag| {
                let start = lower.find(tag)?;
                let open = start + lower[start..].find('>')? + 1;
                let close = open + lower[open..].find('<')?;
                Some(reports::decode_entities(&body[open..close]))
            })
        }
    };
    reports::clean_title(&found.unwrap_or_default())
}

/// Who publishes: the canvas session the CLI runs in (its environment).
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// `COPILOT_CANVAS_SESSION`: the box id.
    pub session: String,
    /// `COPILOT_CANVAS_PASSIVE`: a passive run's key.
    pub passive: String,
    /// `AUTOMATION_NAME`: a scheduled automation's name (a fallback source name).
    pub automation: String,
}

impl Identity {
    pub fn from_env() -> Self {
        let env = |name: &str| std::env::var(name).map(|v| v.trim().to_owned()).unwrap_or_default();
        Identity { session: env("COPILOT_CANVAS_SESSION"), passive: env("COPILOT_CANVAS_PASSIVE"), automation: env("AUTOMATION_NAME") }
    }
}

/// Checks a report, spools it for the service and, when the service runs,
/// waits (up to `wait`) until it stored or refused it. Every problem is an
/// error (the CLI exits non-zero with it on stderr).
pub fn publish(home: &Path, args: PublishArgs, who: &Identity, wait: Duration) -> Result<Outcome> {
    let bodies = [args.markdown.is_some(), args.html.is_some(), args.message.is_some()].iter().filter(|b| **b).count();
    if bodies != 1 {
        bail!("give exactly one of --markdown FILE, --html FILE (FILE may be - for standard input) or --message TEXT");
    }
    let (format, body) = match (&args.markdown, &args.html, &args.message) {
        (Some(file), _, _) => (Format::Markdown, read_body(file, "Markdown")?),
        (_, Some(file), _) => (Format::Html, read_body(file, "HTML")?),
        (_, _, Some(text)) => (Format::Message, text.clone()),
        _ => unreachable!("one body was given"),
    };
    let title = match args.title.as_deref().map(reports::clean_title) {
        Some(title) if !title.is_empty() => title,
        Some(_) => bail!("--title is empty"),
        None => title_from(format, &body),
    };
    if title.is_empty() {
        bail!("give the report a --title (no heading was found to take it from)");
    }
    let source = who.session.clone();
    let automation = (!who.automation.is_empty()).then(|| who.automation.clone());
    let source_name = args.source.clone().or(automation).unwrap_or_default();
    if source.is_empty() && reports::clean_line(&source_name, 80).is_empty() {
        bail!("this is not a Copilot Canvas session: name who publishes with --source NAME (for example --source \"Daily triage\")");
    }
    let kind = args.kind.as_deref().map(|k| k.trim().to_ascii_lowercase()).unwrap_or_else(|| "info".into());
    reports::check_kind(&kind)?;
    let links: Vec<Link> = args.links.iter().map(|l| reports::parse_link(l)).collect::<Result<_>>()?;
    let now = now_ms();
    let submission = Submission {
        version: SUBMISSION_VERSION,
        id: reports::new_id(now),
        title: title.clone(),
        format,
        kind,
        body,
        created_ms: now,
        source,
        source_name,
        passive: who.passive.clone(),
        session: args.session.clone().unwrap_or_default(),
        pr: args.pr.clone().map(|p| p.trim().to_owned()).unwrap_or_default(),
        links,
        thread: args.thread.clone().map(|t| t.trim().to_owned()).unwrap_or_default(),
    };
    submission.validate()?;
    let file = spool(home, &submission)?;
    let id = submission.id.clone();
    if !service_alive(home) {
        return Ok(Outcome::Queued { id, title, why: "The Copilot Canvas service is not running".into(), file });
    }
    let stored = items_dir(home).join(&id).join("meta.json");
    let answer = answer_path(home, &id);
    let deadline = Instant::now() + wait;
    loop {
        if stored.is_file() {
            return Ok(Outcome::Stored { id, title });
        }
        if let Ok(bytes) = std::fs::read(&answer) {
            let _ = std::fs::remove_file(&answer);
            let reason = serde_json::from_slice::<Rejected>(&bytes).map(|r| r.reason).unwrap_or_else(|_| "no reason given".into());
            bail!("Copilot Canvas refused the report: {reason}");
        }
        if Instant::now() >= deadline {
            let why = format!("The service did not take it within {} s (it may be busy, or an older build without reports)", wait.as_secs());
            return Ok(Outcome::Queued { id, title, why, file });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Writes a checked submission into the spool (a temporary file renamed),
/// where the service's inbox thread stores it; answers the spool file.
pub fn spool(home: &Path, submission: &Submission) -> Result<PathBuf> {
    let spool = spool_dir(home);
    std::fs::create_dir_all(&spool).with_context(|| format!("cannot create {}", spool.display()))?;
    let file = spool.join(format!("{}.json", submission.id));
    let temp = spool.join(format!("{}.tmp", submission.id));
    std::fs::write(&temp, serde_json::to_vec(submission)?).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, &file)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(file)
}

/// A short message from the canvas itself (the PR monitor, review
/// follow-ups) into the inbox: `source` is the canvas-wide agent it is on
/// behalf of (its box and passive key), else it shows as `source_name`.
#[derive(Clone, Debug, Default)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub kind: String,
    pub source: String,
    pub passive: String,
    pub source_name: String,
    pub session: String,
    pub pr: String,
    pub links: Vec<Link>,
    pub thread: String,
}

/// Spools a [`Notice`] (cleaned to fit the inbox's rules).
pub fn notice(home: &Path, notice: Notice, now: u64) -> Result<String> {
    let links: Vec<Link> = notice.links.into_iter().filter(|l| reports::parse_link(&format!("{}={}", l.title, l.url)).is_ok()).take(reports::MAX_LINKS).collect();
    let body: String = notice.body.chars().take(reports::MAX_MESSAGE).collect();
    let submission = Submission {
        version: SUBMISSION_VERSION,
        id: reports::new_id(now),
        title: reports::clean_title(&notice.title),
        format: Format::Message,
        kind: notice.kind,
        body: if body.trim().is_empty() { notice.title.clone() } else { body },
        created_ms: now,
        source: notice.source,
        source_name: notice.source_name,
        passive: notice.passive,
        session: notice.session,
        pr: if reports::parse_link(&format!("PR={}", notice.pr)).is_ok() { notice.pr } else { String::new() },
        links,
        thread: notice.thread.chars().take(120).collect(),
    };
    submission.validate()?;
    spool(home, &submission)?;
    Ok(submission.id)
}

/// `copilot-canvas report list`: stored reports (newest first), reports
/// still queued, and refused ones.
pub fn list(home: &Path, unread_only: bool, limit: usize) -> Result<String> {
    let (reports, state) = load(home);
    let now = now_ms();
    let queued: Vec<Submission> = std::fs::read_dir(spool_dir(home))
        .map(|entries| {
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
            files.sort();
            files.iter().filter_map(|f| std::fs::read(f).ok()).filter_map(|b| serde_json::from_slice(&b).ok()).collect()
        })
        .unwrap_or_default();
    let unread = reports.iter().filter(|r| r.unread()).count();
    let mut out = vec![format!(
        "Copilot Canvas reports in {}: {} stored ({unread} unread), {} queued, {} refused{}",
        dir(home).display(),
        reports.len(),
        queued.len(),
        state.rejected.len(),
        if service_alive(home) { "" } else { " · the service is not running" }
    )];
    for s in &queued {
        out.push(format!("  ◌ queued   {:<8} {}  ({})", "", reports::clean_title(&s.title), s.id));
    }
    for r in &state.rejected {
        out.push(format!("  ✗ refused  {:>7} ago  {}: {}  ({})", crate::paths::format_age(r.ms, now), if r.title.is_empty() { "(no title)" } else { &r.title }, r.reason, r.id));
    }
    for r in reports.iter().filter(|r| !unread_only || r.unread()).take(limit) {
        let from = if r.source_name.is_empty() { String::new() } else { format!(" — from {}", r.source_name) };
        out.push(format!(
            "  {} {:<8} {:>7} ago  {} {}{from}  ({})",
            if r.unread() { "●" } else { " " },
            r.kind,
            crate::paths::format_age(r.created_ms, now),
            reports::kind_glyph(&r.kind),
            r.title,
            r.id
        ));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::bus::{PassiveSpec, RouteGroup};

    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-reports-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn routing() -> Routing {
        Routing {
            groups: vec![RouteGroup { id: "g1".into(), name: "Payments".into(), folder: None }],
            members: [("box-1".to_owned(), "g1".to_owned())].into(),
            names: [("box-1".to_owned(), "Fix flaky test".to_owned())].into(),
            passive: vec![PassiveSpec { key: "k1".into(), session_id: "pass-1".into(), name: "Rollout tracker".into(), group: "g1".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    fn args(message: &str) -> PublishArgs {
        PublishArgs { message: Some(message.into()), source: Some("Daily triage".into()), ..Default::default() }
    }

    #[test]
    fn publishing_checks_its_input_and_fails_loudly() {
        let home = home("cli");
        let nobody = Identity::default();
        let fail = |a: PublishArgs| publish(&home, a, &nobody, Duration::ZERO).unwrap_err().to_string();
        assert!(fail(PublishArgs { message: Some("hi".into()), ..Default::default() }).contains("not a Copilot Canvas session: name who publishes with --source"));
        assert!(fail(PublishArgs { source: Some("x".into()), ..Default::default() }).contains("exactly one of --markdown FILE"));
        assert!(fail(PublishArgs { message: Some("m".into()), html: Some("x.html".into()), source: Some("x".into()), ..Default::default() }).contains("exactly one"));
        assert!(fail(PublishArgs { markdown: Some(home.join("missing.md").display().to_string()), source: Some("x".into()), ..Default::default() }).contains("cannot read Markdown file"));
        let big = home.join("big.md");
        std::fs::write(&big, vec![b'x'; MAX_BODY + 10]).unwrap();
        assert!(fail(PublishArgs { markdown: Some(big.display().to_string()), title: Some("t".into()), source: Some("x".into()), ..Default::default() }).contains("at most 2 MiB"));
        let binary = home.join("bin.md");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        assert!(fail(PublishArgs { markdown: Some(binary.display().to_string()), title: Some("t".into()), source: Some("x".into()), ..Default::default() }).contains("not UTF-8"));
        assert!(fail(PublishArgs { kind: Some("loud".into()), ..args("hi") }).contains("--kind is one of"));
        assert!(fail(PublishArgs { links: vec!["Docs=file:///C:/x".into()], ..args("hi") }).contains("http(s)://"));
        assert!(fail(PublishArgs { pr: Some("ftp://x".into()), ..args("hi") }).contains("--pr"));
        assert!(fail(PublishArgs { title: Some("  ".into()), ..args("hi") }).contains("--title is empty"));
        let untitled = home.join("untitled.md");
        std::fs::write(&untitled, "no heading here").unwrap();
        assert!(fail(PublishArgs { markdown: Some(untitled.display().to_string()), source: Some("x".into()), ..Default::default() }).contains("--title"));
        // Nothing failed half-way: the spool holds no report.
        assert!(std::fs::read_dir(spool_dir(&home)).map(|d| d.count()).unwrap_or(0) == 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_report_published_while_the_service_is_down_is_queued_then_stored() {
        let home = home("spool");
        let md = home.join("brief.md");
        std::fs::write(&md, "\u{feff}# Morning brief\n\n- 3 PRs need you\n- build green\n").unwrap();
        let outcome = publish(&home, PublishArgs { markdown: Some(md.display().to_string()), links: vec!["Board=https://example.com/board".into()], source: Some("Daily triage".into()), ..Default::default() }, &Identity::default(), Duration::from_secs(5)).unwrap();
        let Outcome::Queued { id, title, why, file } = &outcome else { panic!("{outcome:?}") };
        assert_eq!(title, "Morning brief", "the first heading names it");
        assert!(why.contains("not running") && file.is_file() && outcome.describe().contains("queued, not stored yet"));
        let automation = Identity { automation: "Daily triage".into(), ..Default::default() };
        let message = publish(&home, PublishArgs { kind: Some("Queued".into()), pr: Some("https://github.com/o/r/pull/12".into()), thread: Some("pr-12".into()), source: None, ..args("PR 12 received 12 new comments; dispatched the owning session") }, &automation, Duration::ZERO).unwrap();
        assert!(list(&home, false, 10).unwrap().contains("2 queued"));
        // The service starts later and takes both.
        let mut store = Store::open(&home, now_ms());
        let done = store.ingest(&routing(), now_ms());
        assert_eq!(done.stored.len(), 2, "{done:?}");
        assert!(std::fs::read_dir(spool_dir(&home)).unwrap().count() == 0, "the spool is empty again");
        let snapshot = store.snapshot();
        assert_eq!((snapshot.total, snapshot.unread), (2, 2));
        let brief = snapshot.reports.iter().find(|r| &r.id == id).unwrap();
        assert_eq!((brief.format, brief.source_name.as_str(), brief.source_kind.as_str(), brief.links.len()), (Format::Markdown, "Daily triage", "external", 1));
        assert_eq!(brief.preview, "Morning brief 3 PRs need you build green");
        let Outcome::Queued { id: message_id, .. } = message else { panic!() };
        let update = snapshot.reports.iter().find(|r| r.id == message_id).unwrap();
        assert_eq!((update.kind.as_str(), update.thread.as_str(), update.title.as_str()), ("queued", "pr-12", "PR 12 received 12 new comments; dispatched the owning session"));
        assert_eq!(content(&home, id).unwrap().body, "# Morning brief\n\n- 3 PRs need you\n- build green\n", "the BOM goes, the text stays");
        assert!(content(&home, "../x").is_err() && content(&home, "0000000000001-deadbeef").is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_cli_waits_for_a_running_service_and_reports_stored_or_refused() {
        let home = home("wait");
        announce_service(&home).unwrap();
        assert!(service_alive(&home), "this process counts as the service");
        let ingest_home = home.clone();
        let service = std::thread::spawn(move || {
            let mut store = Store::open(&ingest_home, now_ms());
            for _ in 0..250 {
                store.ingest(&routing(), now_ms());
                let snapshot = store.snapshot();
                if snapshot.total >= 1 && !snapshot.rejected.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            store.snapshot()
        });
        let in_box = Identity { session: "box-1".into(), ..Default::default() };
        let stored = publish(&home, PublishArgs { message: Some("Build is green".into()), ..Default::default() }, &in_box, Duration::from_secs(5));
        let Outcome::Stored { title, .. } = stored.unwrap() else { panic!("stored") };
        assert_eq!(title, "Build is green");
        // A submission the CLI's checks let through but the service refuses: the CLI says why.
        let sneaky = Submission { version: SUBMISSION_VERSION, id: reports::new_id(now_ms()), title: "x".into(), format: Format::Html, kind: "info".into(), body: " ".into(), ..Default::default() };
        std::fs::write(spool_dir(&home).join(format!("{}.json", sneaky.id)), serde_json::to_vec(&sneaky).unwrap()).unwrap();
        let snapshot = service.join().unwrap();
        assert_eq!(snapshot.reports[0].source_name, "Fix flaky test", "named after its box");
        assert_eq!((snapshot.reports[0].group_name.as_str(), snapshot.reports[0].source_kind.as_str()), ("Payments", "session"));
        assert_eq!(snapshot.rejected.len(), 1);
        assert!(snapshot.rejected[0].reason.contains("empty") && answer_path(&home, &sneaky.id).is_file(), "the refusal is answered for a waiting CLI");
        assert!(list(&home, false, 5).unwrap().contains("✗ refused"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn read_state_persists_and_retention_never_drops_unread_reports() {
        let home = home("state");
        let day = 86_400_000;
        let now = 100 * day;
        let mut store = Store::open(&home, now);
        let submit = |id_ms: u64, title: &str| {
            let s = Submission { version: SUBMISSION_VERSION, id: reports::new_id(id_ms), title: title.into(), format: Format::Message, kind: "info".into(), body: title.into(), created_ms: id_ms, passive: "k1".into(), ..Default::default() };
            std::fs::create_dir_all(spool_dir(&home)).unwrap();
            std::fs::write(spool_dir(&home).join(format!("{}.json", s.id)), serde_json::to_vec(&s).unwrap()).unwrap();
            s.id
        };
        let old = submit(now - 40 * day, "old");
        let older_unread = submit(now - 90 * day, "old but unread");
        let fresh = submit(now - day, "fresh");
        assert_eq!(store.ingest(&routing(), now).stored.len(), 3);
        assert_eq!(store.reports()[0].source_kind, "passive");
        assert_eq!(store.reports()[0].source_name, "Rollout tracker");
        assert!(store.mark(&[old.clone(), fresh.clone()], true, now));
        assert!(!store.mark(std::slice::from_ref(&old), true, now), "already read");
        // Reopened (a service restart): read state kept; the old read report goes, the old unread one stays.
        let mut store = Store::open(&home, now);
        let ids: Vec<&str> = store.reports().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [fresh.as_str(), older_unread.as_str()]);
        assert!(store.reports()[0].read_ms > 0 && store.reports()[1].unread());
        assert!(!items_dir(&home).join(&old).exists(), "its folder is gone");
        assert!(store.mark(std::slice::from_ref(&fresh), false, now) && store.snapshot().unread == 2);
        assert!(store.mark_all_read(now) && store.snapshot().unread == 0);
        let version = store.version;
        assert_eq!(store.delete(&[fresh.clone(), "not-there-1".into()]), [fresh.clone(), "not-there-1".to_owned()]);
        assert!(store.version > version && store.reports().len() == 1);
        assert_eq!(read_snapshot(&home).total, 1, "the in-process backend reads the same inbox");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_full_inbox_refuses_new_reports_instead_of_dropping_unread_ones() {
        let home = home("full");
        let mut store = Store::open(&home, 1_000);
        store.reports = (0..MAX_UNREAD).map(|i| Report { id: format!("{i:013}-aaaaaaaa"), ..Default::default() }).collect();
        let s = Submission { version: SUBMISSION_VERSION, id: reports::new_id(5), title: "one more".into(), format: Format::Message, kind: "info".into(), body: "x".into(), ..Default::default() };
        std::fs::create_dir_all(spool_dir(&home)).unwrap();
        std::fs::write(spool_dir(&home).join(format!("{}.json", s.id)), serde_json::to_vec(&s).unwrap()).unwrap();
        let done = store.ingest(&Routing::default(), 2_000);
        assert!(done.stored.is_empty() && done.rejected[0].reason.contains("unread reports (at most 1000)"), "{done:?}");
        assert_eq!(store.reports().len(), MAX_UNREAD, "nothing was dropped");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn titles_come_from_the_body_when_none_is_given() {
        assert_eq!(title_from(Format::Markdown, "intro\n## Daily *triage*\n"), "Daily *triage*");
        assert_eq!(title_from(Format::Html, "<html><head><TITLE>Brief &amp; plan</TITLE></head>"), "Brief & plan");
        assert_eq!(title_from(Format::Html, "<body><h1 class=x>Status</h1>"), "Status");
        assert_eq!(title_from(Format::Message, "\n  first line\nsecond"), "first line");
    }
}
