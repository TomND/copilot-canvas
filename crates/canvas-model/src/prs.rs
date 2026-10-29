//! Pull requests a canvas tracks for its canvas-wide PR coordinator (WI-014).
//!
//! Working sessions hand a pull request they published to the canvas
//! (`copilot-canvas pr track URL`); the service keeps a [`Registry`] per
//! canvas (`<home>/canvas-agents/<canvas id>/prs/registry.json`, one writer:
//! the service), polls each active pull request read-only with the user's
//! own CLIs (`gh`, `az`), turns what changed since its watermarks into events
//! for the canvas-wide agents (`pr.comment.new`, `pr.checks.failed`, …), and
//! delivers the follow-ups an agent dispatches to the owning session
//! ([`Dispatch`]: intended, queued, delivered, blocked, failed, dismissed).
//! This module holds the data and the pure decisions; the service runs them.

use serde::{Deserialize, Serialize};

/// Pull requests a canvas tracks at most (active ones).
pub const MAX_TRACKED: usize = 200;
/// Watermark ids kept per kind (the newest).
pub const MAX_SEEN: usize = 2_000;
/// Dispatches kept per pull request (the newest).
pub const MAX_DISPATCHES: usize = 40;
/// Dispatches per pull request a client sees.
pub const VIEW_DISPATCHES: usize = 6;
/// Longest dispatch text (characters).
pub const MAX_TEXT: usize = 2_000;
/// Comments an event lists at most (the newest).
pub const EVENT_COMMENTS: usize = 10;
/// Ended pull requests stay listed this long.
pub const KEEP_ENDED_MS: u64 = 30 * 86_400_000;
pub const DEFAULT_POLL_MINUTES: u32 = 5;
/// A monitor that cannot run its CLI tries again after this long.
pub const UNAVAILABLE_RETRY_MS: u64 = 30 * 60_000;
/// Longest back-off after failed polls.
pub const MAX_BACKOFF_MS: u64 = 60 * 60_000;

/// Where a pull request lives.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    /// GitHub or GitHub Enterprise (`gh`).
    #[default]
    GitHub,
    /// Azure DevOps (`az repos`, `az devops invoke`).
    Ado,
}

impl Host {
    pub fn label(self) -> &'static str {
        match self {
            Host::GitHub => "GitHub",
            Host::Ado => "Azure DevOps",
        }
    }

    /// The CLI that reads it.
    pub fn cli(self) -> &'static str {
        match self {
            Host::GitHub => "gh",
            Host::Ado => "az",
        }
    }
}

/// A pull request's identity, parsed from its link.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PrRef {
    pub host: Host,
    /// GitHub: the host name (`github.com`); Azure DevOps: the organization
    /// URL (`https://dev.azure.com/org`).
    pub server: String,
    /// GitHub: the repository owner; Azure DevOps: the organization.
    pub owner: String,
    /// Azure DevOps: the project (empty on GitHub).
    pub project: String,
    pub repo: String,
    pub number: u64,
}

/// `%20` and friends decoded (invalid escapes are kept as they are).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `text` percent-encoded for a URL path segment (spaces and the reserved few).
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl PrRef {
    /// A pull request from its web link (GitHub `…/OWNER/REPO/pull/N`,
    /// Azure DevOps `https://dev.azure.com/ORG/PROJECT/_git/REPO/pullrequest/N`
    /// or `https://ORG.visualstudio.com/…`, the REST link `…/_apis/git/
    /// repositories/REPO/pullRequests/N`) or `OWNER/REPO#N` (github.com).
    pub fn parse(text: &str) -> Result<PrRef, String> {
        let text = text.trim().trim_start_matches(['<', '"', '\'']).trim_end_matches(['.', ',', ';', ')', '>', '"', '\'']);
        let bad = || format!("{text:?} is not a pull request link (GitHub https://github.com/OWNER/REPO/pull/N, Azure DevOps https://dev.azure.com/ORG/PROJECT/_git/REPO/pullrequest/N)");
        if !text.contains("://") {
            // OWNER/REPO#N on github.com.
            let (path, number) = text.split_once('#').ok_or_else(bad)?;
            let (owner, repo) = path.split_once('/').ok_or_else(bad)?;
            let number: u64 = number.parse().map_err(|_| bad())?;
            let name = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !name(owner) || !name(repo) || number == 0 {
                return Err(bad());
            }
            return Ok(PrRef { host: Host::GitHub, server: "github.com".into(), owner: owner.into(), repo: repo.into(), number, project: String::new() });
        }
        let (scheme, rest) = text.split_once("://").ok_or_else(bad)?;
        if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
            return Err(bad());
        }
        let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next().unwrap_or_default().to_ascii_lowercase();
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
            return Err(bad());
        }
        let segments: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(percent_decode).collect();
        let lower: Vec<String> = segments.iter().map(|s| s.to_ascii_lowercase()).collect();
        let number = |s: &str| s.parse::<u64>().ok().filter(|n| *n > 0);
        // Azure DevOps first (its paths never have `pull`).
        let ado = host == "dev.azure.com" || host.ends_with(".visualstudio.com");
        if ado {
            let (org, org_url, path) = if host == "dev.azure.com" {
                let org = segments.first().cloned().ok_or_else(bad)?;
                (org.clone(), format!("https://dev.azure.com/{}", percent_encode(&org)), &segments[1..])
            } else {
                let org = host.trim_end_matches(".visualstudio.com").to_owned();
                let skip = usize::from(lower.first().is_some_and(|s| s == "defaultcollection"));
                (org.clone(), format!("https://dev.azure.com/{}", percent_encode(&org)), &segments[skip..])
            };
            let lowered: Vec<String> = path.iter().map(|s| s.to_ascii_lowercase()).collect();
            // Web: [PROJECT/]_git/REPO/pullrequest/N.
            if let Some(git) = lowered.iter().position(|s| s == "_git")
                && lowered.get(git + 2).is_some_and(|s| s == "pullrequest")
                && let Some(n) = path.get(git + 3).and_then(|s| number(s))
            {
                let repo = path.get(git + 1).cloned().ok_or_else(bad)?;
                let project = if git == 0 { repo.clone() } else { path[git - 1].clone() };
                return Ok(PrRef { host: Host::Ado, server: org_url, owner: org, project, repo, number: n });
            }
            // REST: PROJECT/_apis/git/repositories/REPO/pullRequests/N.
            if let Some(apis) = lowered.iter().position(|s| s == "_apis")
                && lowered.get(apis + 1).is_some_and(|s| s == "git")
                && lowered.get(apis + 2).is_some_and(|s| s == "repositories")
                && lowered.get(apis + 4).is_some_and(|s| s == "pullrequests")
                && let Some(n) = path.get(apis + 5).and_then(|s| number(s))
            {
                let repo = path.get(apis + 3).cloned().ok_or_else(bad)?;
                let project = if apis == 0 { String::new() } else { path[apis - 1].clone() };
                return Ok(PrRef { host: Host::Ado, server: org_url, owner: org, project, repo, number: n });
            }
            return Err(bad());
        }
        // GitHub (and GitHub Enterprise): OWNER/REPO/pull/N[/files…].
        if segments.len() >= 4 && lower[2] == "pull" && let Some(n) = number(&segments[3]) {
            return Ok(PrRef { host: Host::GitHub, server: host, owner: segments[0].clone(), project: String::new(), repo: segments[1].clone(), number: n });
        }
        Err(bad())
    }

    /// One key per pull request: `github.com/owner/repo#12`, and for Azure
    /// DevOps (pull request ids are unique in an organization)
    /// `dev.azure.com/org#12` (lowercase).
    pub fn key(&self) -> String {
        match self.host {
            Host::GitHub => format!("{}/{}/{}#{}", self.server, self.owner, self.repo, self.number).to_lowercase(),
            Host::Ado => format!("dev.azure.com/{}#{}", self.owner, self.number).to_lowercase(),
        }
    }

    /// Its web page.
    pub fn url(&self) -> String {
        match self.host {
            Host::GitHub => format!("https://{}/{}/{}/pull/{}", self.server, self.owner, self.repo, self.number),
            Host::Ado => format!("{}/{}/_git/{}/pullrequest/{}", self.server, percent_encode(&self.project), percent_encode(&self.repo), self.number),
        }
    }

    /// `owner/repo#12`, `repo!12`.
    pub fn short(&self) -> String {
        match self.host {
            Host::GitHub => format!("{}/{}#{}", self.owner, self.repo, self.number),
            Host::Ado => format!("{}!{}", self.repo, self.number),
        }
    }
}

/// The Reports thread of a pull request (`pr:<key>`, thread-key characters only).
pub fn thread_key(key: &str) -> String {
    let clean: String = format!("pr:{key}").chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '#' | '-') { c } else { '-' }).collect();
    clean.chars().take(120).collect()
}

/// Where a pull request stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    #[default]
    Active,
    Merged,
    /// Closed without merging (GitHub).
    Closed,
    /// Abandoned (Azure DevOps).
    Abandoned,
}

impl PrState {
    /// Monitoring stops for good.
    pub fn ended(self) -> bool {
        self != PrState::Active
    }

    pub fn word(self) -> &'static str {
        match self {
            PrState::Active => "active",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
            PrState::Abandoned => "abandoned",
        }
    }
}

/// Whose pull request it is to the canvas.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A working session published it: review comments and failures go back to it.
    #[default]
    Owned,
    /// You reviewed it (review follow-ups): only its end is watched.
    Reviewed,
}

/// The monitor of one pull request.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MonitorStatus {
    /// Not polled yet.
    #[default]
    Pending,
    /// The last poll worked.
    Ok,
    /// Its CLI is missing or not signed in (the reason says which).
    Unavailable,
    /// Polls fail; it backs off.
    Failing,
    /// It ended (merged, closed, abandoned): never polled again.
    Stopped,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Monitor {
    pub status: MonitorStatus,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub last_ms: u64,
    /// When it is polled next (0: as soon as monitoring runs).
    #[serde(default)]
    pub next_ms: u64,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub polls: u32,
}

/// What a monitor has seen already (only newer things become events).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Seen {
    /// The first poll records what is there without events.
    #[serde(default)]
    pub baseline: bool,
    #[serde(default)]
    pub comments: Vec<String>,
    #[serde(default)]
    pub reviews: Vec<String>,
    /// Failed checks reported (`<head>:<name>`).
    #[serde(default)]
    pub checks: Vec<String>,
}

impl Seen {
    pub fn is_empty(&self) -> bool {
        !self.baseline && self.comments.is_empty() && self.reviews.is_empty() && self.checks.is_empty()
    }

    fn remember(list: &mut Vec<String>, id: String) {
        if !list.contains(&id) {
            list.push(id);
            let excess = list.len().saturating_sub(MAX_SEEN);
            list.drain(..excess);
        }
    }
}

/// Where a follow-up for the owning session stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DispatchState {
    /// Recorded; the service puts it on its way at its next pass.
    #[default]
    Intended,
    /// Waiting for the owning session to be idle (never typed mid-turn).
    Queued,
    /// Typed into the owning session.
    Delivered,
    /// The owning session cannot get it (removed, stopped, open elsewhere,
    /// muted): you resume it, reassign or dismiss it.
    Blocked,
    /// Not delivered (discarded, or not within a day).
    Failed,
    /// You dismissed it.
    Dismissed,
}

impl DispatchState {
    pub fn word(self) -> &'static str {
        match self {
            DispatchState::Intended => "intended",
            DispatchState::Queued => "queued",
            DispatchState::Delivered => "delivered",
            DispatchState::Blocked => "blocked",
            DispatchState::Failed => "failed",
            DispatchState::Dismissed => "dismissed",
        }
    }

    /// Still on its way (it counts against a duplicate).
    pub fn open(self) -> bool {
        matches!(self, DispatchState::Intended | DispatchState::Queued | DispatchState::Blocked)
    }

    /// The Reports kind of a change to this state (`None`: no report).
    pub fn report_kind(self) -> Option<&'static str> {
        match self {
            DispatchState::Queued => Some("queued"),
            DispatchState::Delivered => Some("delivered"),
            DispatchState::Blocked => Some("warn"),
            DispatchState::Failed => Some("failed"),
            DispatchState::Intended | DispatchState::Dismissed => None,
        }
    }
}

/// A follow-up for the owning session (`copilot-canvas pr dispatch`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Dispatch {
    /// `d-<ms>-<4 hex>`; also the mailbox message's id.
    pub id: String,
    /// One dispatch per key per pull request (`comments:<newest id>`).
    pub key: String,
    /// The event it answers (`pr.comment.new`).
    #[serde(default)]
    pub event: String,
    /// What it is about, in a few words ("3 new review comment(s)").
    #[serde(default)]
    pub topic: String,
    /// The session it goes to (the owner when it was made) and its name.
    pub to: String,
    #[serde(default)]
    pub to_name: String,
    pub text: String,
    /// Who dispatched it (an agent's name, or `you`) and its passive key.
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub from_key: String,
    pub state: DispatchState,
    /// Why it is blocked or failed.
    #[serde(default)]
    pub reason: String,
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub delivered_ms: u64,
}

/// One tracked pull request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TrackedPr {
    pub key: String,
    pub pr: PrRef,
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: PrState,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub role: Role,
    /// The owning session's box (a reviewed pull request: the reviewing session).
    pub owner: String,
    #[serde(default)]
    pub owner_name: String,
    /// The owner's group when it was tracked.
    #[serde(default)]
    pub group: String,
    pub tracked_ms: u64,
    /// How it came: `handed off by …`, `detected (gh pr create)`, `you`.
    #[serde(default)]
    pub tracked_by: String,
    #[serde(default)]
    pub monitor: Monitor,
    #[serde(default, skip_serializing_if = "Seen::is_empty")]
    pub seen: Seen,
    /// Review comments seen in all, new ones since the last dispatch.
    #[serde(default)]
    pub comments: u32,
    #[serde(default)]
    pub new_comments: u32,
    /// Checks failing on its head now.
    #[serde(default)]
    pub failing: Vec<String>,
    /// GitHub's review decision, or the Azure DevOps votes in a word.
    #[serde(default)]
    pub decision: String,
    /// What changed last (one line) and when.
    #[serde(default)]
    pub last_change: String,
    #[serde(default)]
    pub last_change_ms: u64,
    #[serde(default)]
    pub dispatches: Vec<Dispatch>,
}

impl TrackedPr {
    pub fn new(pr: PrRef, role: Role, owner: &str, owner_name: &str, group: &str, by: &str, now: u64) -> Self {
        TrackedPr {
            key: pr.key(),
            url: pr.url(),
            pr,
            role,
            owner: owner.to_owned(),
            owner_name: owner_name.to_owned(),
            group: group.to_owned(),
            tracked_ms: now,
            tracked_by: by.to_owned(),
            ..Default::default()
        }
    }

    /// Polled (active, not stopped) and due now.
    pub fn due(&self, now: u64) -> bool {
        !self.state.ended() && self.monitor.status != MonitorStatus::Stopped && self.monitor.next_ms <= now
    }

    /// Something here needs you: a blocked or failed follow-up, a failing monitor.
    pub fn needs_you(&self) -> bool {
        self.dispatches.iter().any(|d| matches!(d.state, DispatchState::Blocked | DispatchState::Failed))
            || (!self.state.ended() && matches!(self.monitor.status, MonitorStatus::Unavailable | MonitorStatus::Failing))
    }
}

/// The pull requests of one canvas.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Registry {
    #[serde(default)]
    pub prs: Vec<TrackedPr>,
}

/// What `track` did.
#[derive(Debug, PartialEq)]
pub enum Tracked {
    New,
    /// Already tracked for this owner (nothing changed).
    Same,
    /// Tracked again after it ended, or moved to a new owner.
    Updated,
}

/// A follow-up an agent (or you) asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DispatchRequest {
    pub key: String,
    pub event: String,
    pub text: String,
    pub from: String,
    pub from_key: String,
}

/// What `dispatch` did.
#[derive(Debug, PartialEq)]
pub enum Dispatched {
    /// A new dispatch (its id), still intended.
    New(String),
    /// One with this key exists (its id and state): nothing new.
    Duplicate(String, DispatchState),
}

impl Registry {
    pub fn get(&self, key: &str) -> Option<&TrackedPr> {
        self.prs.iter().find(|p| p.key == key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut TrackedPr> {
        self.prs.iter_mut().find(|p| p.key == key)
    }

    /// Tracks a pull request for `owner`. A session cannot take another
    /// session's active pull request (`reassign`: you and canvas-wide agents can).
    pub fn track(&mut self, pr: TrackedPr, reassign: bool) -> Result<Tracked, String> {
        if let Some(existing) = self.get_mut(&pr.key) {
            if existing.owner == pr.owner && !existing.state.ended() && existing.role == pr.role {
                if !pr.title.is_empty() && existing.title.is_empty() {
                    existing.title = pr.title;
                }
                return Ok(Tracked::Same);
            }
            if existing.owner != pr.owner && !existing.state.ended() && !reassign && existing.role == Role::Owned {
                return Err(format!("{} is already tracked for “{}”; only you or a canvas-wide agent can give it to another session", existing.pr.short(), existing.owner_name));
            }
            existing.owner = pr.owner;
            existing.owner_name = pr.owner_name;
            existing.group = pr.group;
            existing.tracked_by = pr.tracked_by;
            if existing.role == Role::Reviewed {
                existing.role = pr.role;
            }
            if existing.state.ended() {
                // Tracked again: monitored from scratch (its old comments are not new).
                existing.state = PrState::Active;
                existing.monitor = Monitor::default();
                existing.tracked_ms = pr.tracked_ms;
            }
            if !pr.title.is_empty() {
                existing.title = pr.title;
            }
            return Ok(Tracked::Updated);
        }
        let active = self.prs.iter().filter(|p| !p.state.ended()).count();
        if active >= MAX_TRACKED {
            return Err(format!("the canvas tracks {active} active pull requests (at most {MAX_TRACKED}): untrack some first"));
        }
        self.prs.push(pr);
        Ok(Tracked::New)
    }

    pub fn untrack(&mut self, key: &str) -> Option<TrackedPr> {
        let at = self.prs.iter().position(|p| p.key == key)?;
        Some(self.prs.remove(at))
    }

    /// Keys of the pull requests to poll now (the longest-waiting first).
    pub fn due(&self, now: u64) -> Vec<String> {
        let mut due: Vec<&TrackedPr> = self.prs.iter().filter(|p| p.due(now)).collect();
        due.sort_by_key(|p| p.monitor.next_ms);
        due.into_iter().map(|p| p.key.clone()).collect()
    }

    /// Records a follow-up for a pull request's owner; one per key.
    pub fn dispatch(&mut self, key: &str, request: DispatchRequest, now: u64) -> Result<Dispatched, String> {
        let pr = self.get_mut(key).ok_or_else(|| format!("{key} is not tracked on this canvas (copilot-canvas pr list)"))?;
        if pr.role != Role::Owned {
            return Err(format!("{} is a pull request you reviewed: follow-ups go through review follow-ups", pr.pr.short()));
        }
        let text: String = request.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TEXT).collect();
        if text.is_empty() {
            return Err("the follow-up has no text".into());
        }
        let dedupe = if request.key.trim().is_empty() { format!("text:{:016x}", fnv(&text)) } else { request.key.trim().chars().take(160).collect() };
        if let Some(existing) = pr.dispatches.iter().find(|d| d.key == dedupe && d.state != DispatchState::Dismissed && d.state != DispatchState::Failed) {
            return Ok(Dispatched::Duplicate(existing.id.clone(), existing.state));
        }
        let id = format!("d-{now}-{:04x}", fnv(&format!("{key}{dedupe}{now}")) & 0xffff);
        let topic = topic(&request.event, pr);
        pr.dispatches.push(Dispatch {
            id: id.clone(),
            key: dedupe,
            event: request.event,
            topic,
            to: pr.owner.clone(),
            to_name: pr.owner_name.clone(),
            text,
            from: request.from,
            from_key: request.from_key,
            state: DispatchState::Intended,
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        });
        if request.key.starts_with("comments:") {
            pr.new_comments = 0;
        }
        let excess = pr.dispatches.len().saturating_sub(MAX_DISPATCHES);
        pr.dispatches.drain(..excess);
        Ok(Dispatched::New(id))
    }

    /// The pull request and dispatch with this dispatch id.
    pub fn find_dispatch(&mut self, id: &str) -> Option<(&mut TrackedPr, usize)> {
        self.prs.iter_mut().find_map(|p| p.dispatches.iter().position(|d| d.id == id).map(move |i| (p, i)))
    }

    /// Ended pull requests go after [`KEEP_ENDED_MS`].
    pub fn prune(&mut self, now: u64) {
        self.prs.retain(|p| !p.state.ended() || now.saturating_sub(p.monitor.last_ms.max(p.last_change_ms).max(p.tracked_ms)) < KEEP_ENDED_MS);
    }
}

/// What a follow-up for `event` is about, from what the pull request shows now.
pub fn topic(event: &str, pr: &TrackedPr) -> String {
    match event {
        "pr.comment.new" if pr.new_comments > 0 => format!("{} new review comment(s)", pr.new_comments),
        "pr.comment.new" => "new review comments".into(),
        "pr.checks.failed" if !pr.failing.is_empty() => format!("failing checks ({})", pr.failing.join(", ")),
        "pr.checks.failed" => "failing checks".into(),
        "pr.changes_requested" => "a request for changes".into(),
        "pr.approved" => "an approval".into(),
        "" => "a follow-up".into(),
        other => other.to_owned(),
    }
}

/// A small, stable hash (dedupe keys and ids).
pub fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// One line of untrusted text: control characters gone, at most `max` characters.
pub fn excerpt(text: &str, max: usize) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().filter(|c| !c.is_control()).collect();
    if line.chars().count() > max { format!("{}…", line.chars().take(max.saturating_sub(1)).collect::<String>()) } else { line }
}

// ---------- what a poll finds ----------

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub url: String,
    /// The start of its text (untrusted).
    pub text: String,
    /// The file of an inline comment.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ReviewItem {
    pub id: String,
    pub author: String,
    /// `approved`, `changes_requested`.
    pub state: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    #[default]
    Pending,
    Pass,
    Fail,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    #[serde(default)]
    pub url: String,
}

/// A pull request as one poll saw it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Poll {
    pub title: String,
    pub state: PrState,
    pub draft: bool,
    /// The head commit (failed checks are reported once per head).
    pub head: String,
    /// The pull request's author (their own comments are not new comments).
    pub author: String,
    pub decision: String,
    pub comments: Vec<Comment>,
    pub reviews: Vec<ReviewItem>,
    pub checks: Vec<Check>,
    /// Azure DevOps: the project and repository by name (a pull request
    /// tracked from its REST link names them by id).
    pub project: String,
    pub repo: String,
}

/// The JSON values in CLI output (one document, or pages printed one after another).
fn values(text: &str) -> Vec<serde_json::Value> {
    let start = text.find(['{', '[']).unwrap_or(text.len());
    serde_json::Deserializer::from_str(&text[start..]).into_iter::<serde_json::Value>().map_while(Result::ok).collect()
}

fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

/// `gh pr view URL --json number,url,title,state,isDraft,headRefOid,reviewDecision,author,comments,reviews,statusCheckRollup`
/// and `gh api repos/O/R/pulls/N/comments --paginate` (inline review comments).
pub fn parse_github(view: &str, inline: &str) -> Result<Poll, String> {
    let view = values(view).into_iter().next().filter(|v| v.is_object()).ok_or("gh pr view gave no JSON")?;
    let state = match str_of(&view, "state").to_ascii_uppercase().as_str() {
        "OPEN" => PrState::Active,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        other => return Err(format!("gh pr view: unknown state {other:?}")),
    };
    let url = str_of(&view, "url").to_owned();
    let author = view["author"]["login"].as_str().unwrap_or_default().to_owned();
    let mut poll = Poll {
        title: excerpt(str_of(&view, "title"), 200),
        state,
        draft: view["isDraft"].as_bool().unwrap_or(false),
        head: str_of(&view, "headRefOid").to_owned(),
        author: author.clone(),
        decision: str_of(&view, "reviewDecision").to_ascii_lowercase(),
        ..Default::default()
    };
    for c in view["comments"].as_array().into_iter().flatten() {
        let id = str_of(c, "id");
        if id.is_empty() || c["isMinimized"].as_bool() == Some(true) {
            continue;
        }
        poll.comments.push(Comment { id: format!("c:{id}"), author: c["author"]["login"].as_str().unwrap_or_default().to_owned(), url: str_of(c, "url").to_owned(), text: excerpt(str_of(c, "body"), 160), path: String::new() });
    }
    for r in view["reviews"].as_array().into_iter().flatten() {
        let id = str_of(r, "id");
        let who = r["author"]["login"].as_str().unwrap_or_default().to_owned();
        let body = excerpt(str_of(r, "body"), 160);
        match str_of(r, "state").to_ascii_uppercase().as_str() {
            "APPROVED" => poll.reviews.push(ReviewItem { id: format!("r:{id}"), author: who, state: "approved".into(), text: body }),
            "CHANGES_REQUESTED" => poll.reviews.push(ReviewItem { id: format!("r:{id}"), author: who, state: "changes_requested".into(), text: body }),
            // A review's own text counts as a comment (its inline comments come separately).
            "COMMENTED" if !body.is_empty() => poll.comments.push(Comment { id: format!("r:{id}"), author: who, url: url.clone(), text: body, path: String::new() }),
            _ => {}
        }
    }
    for page in values(inline) {
        for c in page.as_array().into_iter().flatten() {
            let Some(id) = c["id"].as_u64().map(|n| n.to_string()).or_else(|| c["id"].as_str().map(str::to_owned)) else { continue };
            poll.comments.push(Comment { id: format!("i:{id}"), author: c["user"]["login"].as_str().unwrap_or_default().to_owned(), url: str_of(c, "html_url").to_owned(), text: excerpt(str_of(c, "body"), 160), path: str_of(c, "path").to_owned() });
        }
    }
    for check in view["statusCheckRollup"].as_array().into_iter().flatten() {
        let (name, state, link) = if check["__typename"] == "StatusContext" || check.get("context").is_some() {
            let state = match str_of(check, "state").to_ascii_uppercase().as_str() {
                "SUCCESS" => CheckState::Pass,
                "FAILURE" | "ERROR" => CheckState::Fail,
                _ => CheckState::Pending,
            };
            (str_of(check, "context"), state, str_of(check, "targetUrl"))
        } else {
            let state = match (str_of(check, "status").to_ascii_uppercase().as_str(), str_of(check, "conclusion").to_ascii_uppercase().as_str()) {
                ("COMPLETED", "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => CheckState::Fail,
                ("COMPLETED", _) => CheckState::Pass,
                _ => CheckState::Pending,
            };
            (str_of(check, "name"), state, str_of(check, "detailsUrl"))
        };
        if !name.is_empty() {
            poll.checks.push(Check { name: excerpt(name, 80), state, url: link.to_owned() });
        }
    }
    Ok(poll)
}

/// `az repos pr show`, the threads (`az devops invoke --area git --resource
/// pullRequestThreads`) and `az repos pr policy list` of an Azure DevOps pull request.
pub fn parse_ado(pr: &PrRef, show: &str, threads: &str, policies: &str) -> Result<Poll, String> {
    let show = values(show).into_iter().next().filter(|v| v.is_object()).ok_or("az repos pr show gave no JSON")?;
    let state = match str_of(&show, "status").to_ascii_lowercase().as_str() {
        "active" | "notset" | "" => PrState::Active,
        "completed" => PrState::Merged,
        "abandoned" => PrState::Abandoned,
        other => return Err(format!("az repos pr show: unknown status {other:?}")),
    };
    let author = show["createdBy"]["uniqueName"].as_str().unwrap_or_default().to_owned();
    let web = pr.url();
    let mut poll = Poll {
        title: excerpt(str_of(&show, "title"), 200),
        state,
        draft: show["isDraft"].as_bool().unwrap_or(false),
        head: show["lastMergeSourceCommit"]["commitId"].as_str().unwrap_or_default().to_owned(),
        author,
        project: show["repository"]["project"]["name"].as_str().unwrap_or_default().to_owned(),
        repo: show["repository"]["name"].as_str().unwrap_or_default().to_owned(),
        ..Default::default()
    };
    let mut approved = 0;
    let mut waiting = 0;
    for reviewer in show["reviewers"].as_array().into_iter().flatten() {
        let vote = reviewer["vote"].as_i64().unwrap_or(0);
        let who = reviewer["uniqueName"].as_str().or(reviewer["displayName"].as_str()).unwrap_or_default().to_owned();
        let state = match vote {
            10 | 5 => "approved",
            -5 | -10 => "changes_requested",
            _ => continue,
        };
        if state == "approved" { approved += 1 } else { waiting += 1 }
        poll.reviews.push(ReviewItem { id: format!("v:{who}:{vote}"), author: reviewer["displayName"].as_str().unwrap_or(&who).to_owned(), state: state.into(), text: String::new() });
    }
    poll.decision = if waiting > 0 { "changes_requested".into() } else if approved > 0 { "approved".into() } else { String::new() };
    let threads = values(threads).into_iter().next().unwrap_or_default();
    let list = threads["value"].as_array().or(threads.as_array()).cloned().unwrap_or_default();
    for thread in &list {
        if thread["isDeleted"].as_bool() == Some(true) {
            continue;
        }
        let thread_id = thread["id"].as_u64().unwrap_or(0);
        let path = thread["threadContext"]["filePath"].as_str().unwrap_or_default().trim_start_matches('/').to_owned();
        for c in thread["comments"].as_array().into_iter().flatten() {
            if c["isDeleted"].as_bool() == Some(true) || str_of(c, "commentType").eq_ignore_ascii_case("system") {
                continue;
            }
            let Some(id) = c["id"].as_u64() else { continue };
            poll.comments.push(Comment {
                id: format!("t:{thread_id}/{id}"),
                author: c["author"]["uniqueName"].as_str().or(c["author"]["displayName"].as_str()).unwrap_or_default().to_owned(),
                url: format!("{web}?_a=overview&discussionId={thread_id}"),
                text: excerpt(str_of(c, "content"), 160),
                path: path.clone(),
            });
        }
    }
    for policy in values(policies).into_iter().flat_map(|v| v.as_array().cloned().unwrap_or_default()) {
        let kind = policy["configuration"]["type"]["displayName"].as_str().unwrap_or_default();
        if !kind.eq_ignore_ascii_case("build") {
            continue;
        }
        let settings = &policy["configuration"]["settings"];
        let name = settings["displayName"].as_str().filter(|n| !n.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("Build {}", settings["buildDefinitionId"].as_u64().unwrap_or(0)));
        let state = match str_of(&policy, "status").to_ascii_lowercase().as_str() {
            "approved" => CheckState::Pass,
            "rejected" | "broken" => CheckState::Fail,
            _ => CheckState::Pending,
        };
        let url = policy["context"]["buildId"].as_u64().map(|id| format!("{}/{}/_build/results?buildId={id}", pr.server, percent_encode(&pr.project))).unwrap_or_default();
        poll.checks.push(Check { name: excerpt(&name, 80), state, url });
    }
    Ok(poll)
}

/// What a poll found that is new.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// New review comments by others than the pull request's author.
    Comments(Vec<Comment>),
    /// Checks that fail on the head and were not reported for it.
    ChecksFailed { head: String, checks: Vec<Check> },
    /// A new approval or request for changes.
    Reviewed(ReviewItem),
    /// It merged, closed or was abandoned: monitoring stops.
    Ended(PrState),
}

impl TrackedPr {
    /// Applies a poll: updates what is shown and the watermarks, and answers
    /// what is new. The first poll only records (nothing before tracking is
    /// new); a reviewed pull request only reports its end.
    pub fn apply(&mut self, poll: &Poll, now: u64) -> Vec<Change> {
        let mut changes = Vec::new();
        if !poll.title.is_empty() {
            self.title = poll.title.clone();
        }
        if self.pr.host == Host::Ado && !poll.repo.is_empty() && !poll.project.is_empty() && (self.pr.repo != poll.repo || self.pr.project != poll.project) {
            // Named now (it was tracked from a REST link with ids): its web link.
            self.pr.project = poll.project.clone();
            self.pr.repo = poll.repo.clone();
            self.url = self.pr.url();
        }
        self.draft = poll.draft;
        self.decision = poll.decision.clone();
        let failing: Vec<&Check> = poll.checks.iter().filter(|c| c.state == CheckState::Fail).collect();
        self.failing = failing.iter().map(|c| c.name.clone()).collect();
        let others: Vec<&Comment> = poll.comments.iter().filter(|c| poll.author.is_empty() || !c.author.eq_ignore_ascii_case(&poll.author)).collect();
        self.comments = others.len() as u32;
        let baseline = !self.seen.baseline;
        let owned = self.role == Role::Owned;
        if owned {
            let new: Vec<Comment> = others.iter().filter(|c| !self.seen.comments.contains(&c.id)).map(|c| (*c).clone()).collect();
            for c in &poll.comments {
                Seen::remember(&mut self.seen.comments, c.id.clone());
            }
            if !baseline && !new.is_empty() {
                self.new_comments += new.len() as u32;
                changes.push(Change::Comments(new));
            }
            let fresh: Vec<Check> = failing.iter().filter(|c| !self.seen.checks.contains(&format!("{}:{}", poll.head, c.name))).map(|c| (*c).clone()).collect();
            for c in &failing {
                Seen::remember(&mut self.seen.checks, format!("{}:{}", poll.head, c.name));
            }
            // A failure already there when tracking started still needs its owner.
            if !fresh.is_empty() && !poll.state.ended() {
                changes.push(Change::ChecksFailed { head: poll.head.clone(), checks: fresh });
            }
            for review in &poll.reviews {
                if !self.seen.reviews.contains(&review.id) {
                    Seen::remember(&mut self.seen.reviews, review.id.clone());
                    if !baseline {
                        changes.push(Change::Reviewed(review.clone()));
                    }
                }
            }
        }
        self.seen.baseline = true;
        if poll.state.ended() && !self.state.ended() {
            changes.push(Change::Ended(poll.state));
        }
        self.state = poll.state;
        self.monitor.status = if poll.state.ended() { MonitorStatus::Stopped } else { MonitorStatus::Ok };
        self.monitor.reason = if poll.state.ended() { format!("{}: monitoring stopped", poll.state.word()) } else { String::new() };
        self.monitor.last_ms = now;
        self.monitor.failures = 0;
        self.monitor.polls += 1;
        if let Some(change) = changes.last() {
            self.last_change = change.summary();
            self.last_change_ms = now;
        }
        changes
    }

    /// A poll that could not run: unavailable (the CLI is missing or not
    /// signed in: try again in 30 minutes) or failing (back off).
    pub fn poll_failed(&mut self, unavailable: bool, reason: &str, interval_ms: u64, now: u64) {
        self.monitor.status = if unavailable { MonitorStatus::Unavailable } else { MonitorStatus::Failing };
        self.monitor.reason = excerpt(reason, 300);
        self.monitor.last_ms = now;
        self.monitor.failures += 1;
        self.monitor.next_ms = now + if unavailable { UNAVAILABLE_RETRY_MS } else { backoff(interval_ms, self.monitor.failures) };
    }
}

/// The wait after `failures` failed polls: the interval doubled each time, at most an hour.
pub fn backoff(interval_ms: u64, failures: u32) -> u64 {
    interval_ms.saturating_mul(1u64 << failures.min(6)).min(MAX_BACKOFF_MS).max(interval_ms)
}

impl Change {
    pub fn summary(&self) -> String {
        match self {
            Change::Comments(list) => format!("{} new comment(s)", list.len()),
            Change::ChecksFailed { checks, .. } => format!("{} failing: {}", if checks.len() == 1 { "a check is" } else { "checks are" }, checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")),
            Change::Reviewed(r) if r.state == "approved" => format!("approved by {}", r.author),
            Change::Reviewed(r) => format!("changes requested by {}", r.author),
            Change::Ended(state) => state.word().to_owned(),
        }
    }

    /// The bus event (type, data) for the canvas-wide agents. Its data names
    /// the pull request, its owner and a dispatch key for follow-ups.
    pub fn event(&self, pr: &TrackedPr) -> (String, serde_json::Value) {
        let mut data = serde_json::json!({
            "pr": pr.key,
            "url": pr.url,
            "short": pr.pr.short(),
            "title": pr.title,
            "owner": pr.owner,
            "owner_name": pr.owner_name,
            "monitor": "copilot-canvas",
        });
        let kind = match self {
            Change::Comments(list) => {
                let newest = list.last().map(|c| c.id.clone()).unwrap_or_default();
                let shown: Vec<serde_json::Value> = list.iter().rev().take(EVENT_COMMENTS).rev().map(|c| serde_json::json!({ "author": c.author, "url": c.url, "text": c.text, "path": c.path })).collect();
                data["count"] = list.len().into();
                data["comments"] = shown.into();
                data["dispatch_key"] = format!("comments:{newest}").into();
                "pr.comment.new"
            }
            Change::ChecksFailed { head, checks } => {
                data["head"] = head.clone().into();
                data["failed"] = checks.iter().map(|c| serde_json::json!({ "name": c.name, "url": c.url })).collect::<Vec<_>>().into();
                data["dispatch_key"] = format!("checks:{}:{}", head.chars().take(12).collect::<String>(), checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",")).chars().take(160).collect::<String>().into();
                "pr.checks.failed"
            }
            Change::Reviewed(r) => {
                data["reviewer"] = r.author.clone().into();
                data["comment"] = r.text.clone().into();
                data["dispatch_key"] = format!("review:{}", r.id).into();
                if r.state == "approved" { "pr.approved" } else { "pr.changes_requested" }
            }
            Change::Ended(state) => {
                data["state"] = state.word().into();
                if *state == PrState::Merged { "pr.merged" } else { "pr.closed" }
            }
        };
        (kind.to_owned(), data)
    }
}

// ---------- the coordinator's settings ----------

/// What a pull request follow-up may ask of the owning session beyond
/// fixing it locally (none by default: tracking a pull request is no permission).
pub const PERMISSIONS: [(&str, &str); 5] = [
    ("push", "push fixes to the pull request's branch"),
    ("reply", "reply to review comments"),
    ("resolve", "resolve review threads"),
    ("vote", "approve or reject (vote)"),
    ("merge", "complete or merge it"),
];

/// PR coordination of a canvas-wide agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PrSettings {
    /// Minutes between polls of each pull request.
    #[serde(default = "default_poll")]
    pub poll_minutes: u32,
    /// Track pull requests sessions create (`gh pr create` and the like seen
    /// in their history) without a hand-off.
    #[serde(default = "yes")]
    pub auto_track: bool,
    /// [`PERMISSIONS`] follow-ups may ask for (empty: read-only).
    #[serde(default)]
    pub allow: Vec<String>,
}

fn default_poll() -> u32 {
    DEFAULT_POLL_MINUTES
}

fn yes() -> bool {
    true
}

impl Default for PrSettings {
    fn default() -> Self {
        PrSettings { poll_minutes: DEFAULT_POLL_MINUTES, auto_track: true, allow: Vec::new() }
    }
}

impl PrSettings {
    /// `on every 5m auto allow push` (the settings prompt's text).
    pub fn text(&self) -> String {
        let allow = if self.allow.is_empty() { "read-only".to_owned() } else { format!("allow {}", self.allow.join(",")) };
        format!("on every {}m {} {allow}", self.poll_minutes, if self.auto_track { "auto" } else { "manual" })
    }

    /// Parses the settings prompt: `off`, or `on [every 5m] [auto|manual]
    /// [read-only|allow push,reply]`.
    pub fn parse(text: &str) -> Result<Option<PrSettings>, String> {
        let words: Vec<String> = text.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).map(|w| w.to_ascii_lowercase()).collect();
        if words.first().is_some_and(|w| w == "off") {
            return Ok(None);
        }
        let mut settings = PrSettings::default();
        let mut i = 0;
        let mut allowing = false;
        while i < words.len() {
            let word = words[i].as_str();
            match word {
                "on" => allowing = false,
                "auto" => (settings.auto_track, allowing) = (true, false),
                "manual" => (settings.auto_track, allowing) = (false, false),
                "read-only" | "readonly" | "none" => (settings.allow, allowing) = (Vec::new(), false),
                "allow" => allowing = true,
                "every" => {
                    let next = words.get(i + 1).ok_or("every takes a duration such as 5m")?;
                    let ms = crate::bus::parse_duration(next).ok_or_else(|| format!("not a duration: {next} (for example 5m)"))?;
                    settings.poll_minutes = ((ms / 60_000).max(1)) as u32;
                    i += 1;
                    allowing = false;
                }
                _ if allowing && PERMISSIONS.iter().any(|(p, _)| *p == word) => {
                    if !settings.allow.iter().any(|a| a == word) {
                        settings.allow.push(word.to_owned());
                    }
                }
                _ => return Err(format!("not understood: {word} (on every 5m auto read-only · allow {} · off)", PERMISSIONS.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(","))),
            }
            i += 1;
        }
        Ok(Some(settings))
    }
}

/// What a follow-up permits beyond fixing locally, for prompts and messages.
pub fn permissions_text(allow: &[String]) -> String {
    let denied: Vec<&str> = PERMISSIONS.iter().map(|(p, _)| *p).filter(|p| !allow.iter().any(|a| a == p)).collect();
    if allow.is_empty() {
        return format!("tracking grants no permission to {}: fix and test locally; ask the user before any of these", denied.join(", "));
    }
    let allowed: Vec<&str> = PERMISSIONS.iter().filter(|(p, _)| allow.iter().any(|a| a == p)).map(|(_, about)| *about).collect();
    if denied.is_empty() { format!("allowed: {}", allowed.join("; ")) } else { format!("allowed: {}; not allowed without the user: {}", allowed.join("; "), denied.join(", ")) }
}

/// The text typed into the owning session for a dispatch: the agent's
/// words, the pull request's link and what the follow-up permits.
pub fn dispatch_text(pr: &TrackedPr, dispatch: &Dispatch, allow: &[String]) -> String {
    format!("PR {} ({}): {} [Canvas: {}. Follow-up {}.]", pr.pr.short(), pr.url, dispatch.text, permissions_text(allow), dispatch.id)
}

// ---------- what clients see ----------

/// The pull requests of the routed canvas for clients (`bus::Snapshot::prs`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PrsView {
    /// The canvas these belong to.
    pub canvas: String,
    /// The canvas-wide agents with PR coordination (enabled and not paused).
    #[serde(default)]
    pub coordinators: Vec<String>,
    /// Why nothing is polled now (empty: monitoring runs).
    #[serde(default)]
    pub paused: String,
    /// What follow-ups may ask for (the coordinators' permissions).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Newest first; without watermarks, the last few dispatches each.
    #[serde(default)]
    pub prs: Vec<TrackedPr>,
}

impl PrsView {
    pub fn of(canvas: &str, registry: &Registry, coordinators: Vec<String>, paused: String, allow: Vec<String>) -> Self {
        let mut prs: Vec<TrackedPr> = registry
            .prs
            .iter()
            .map(|p| {
                let mut p = p.clone();
                p.seen = Seen::default();
                let skip = p.dispatches.len().saturating_sub(VIEW_DISPATCHES);
                p.dispatches.drain(..skip);
                p
            })
            .collect();
        // Needs you first, then active, then the newest.
        prs.sort_by_key(|p| (!p.needs_you(), p.state.ended(), std::cmp::Reverse(p.last_change_ms.max(p.tracked_ms))));
        PrsView { canvas: canvas.to_owned(), coordinators, paused, allow, prs }
    }

    pub fn active(&self) -> usize {
        self.prs.iter().filter(|p| !p.state.ended()).count()
    }

    pub fn needing_you(&self) -> usize {
        self.prs.iter().filter(|p| p.needs_you()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_and_azure_devops_links_parse_to_one_key_each() {
        let gh = PrRef::parse("https://github.com/Octo/App/pull/42/files?w=1#diff").unwrap();
        assert_eq!((gh.host, gh.owner.as_str(), gh.repo.as_str(), gh.number), (Host::GitHub, "Octo", "App", 42));
        assert_eq!(gh.key(), "github.com/octo/app#42");
        assert_eq!(gh.url(), "https://github.com/Octo/App/pull/42");
        assert_eq!(PrRef::parse("octo/app#42").unwrap().key(), gh.key(), "the short form is the same pull request");
        assert_eq!(PrRef::parse("<https://github.com/octo/app/pull/42>.").unwrap().key(), gh.key());
        let ghe = PrRef::parse("https://git.corp.example/team/svc/pull/7").unwrap();
        assert_eq!((ghe.server.as_str(), ghe.key().as_str()), ("git.corp.example", "git.corp.example/team/svc#7"));

        let ado = PrRef::parse("https://dev.azure.com/contoso/My%20Project/_git/web-app/pullrequest/1234?_a=files").unwrap();
        assert_eq!((ado.host, ado.owner.as_str(), ado.project.as_str(), ado.repo.as_str(), ado.number), (Host::Ado, "contoso", "My Project", "web-app", 1234));
        assert_eq!(ado.server, "https://dev.azure.com/contoso");
        assert_eq!(ado.url(), "https://dev.azure.com/contoso/My%20Project/_git/web-app/pullrequest/1234");
        assert_eq!(ado.key(), "dev.azure.com/contoso#1234");
        assert_eq!(ado.short(), "web-app!1234");
        let old = PrRef::parse("https://contoso.visualstudio.com/DefaultCollection/My Project/_git/web-app/pullrequest/1234").unwrap();
        assert_eq!(old.key(), ado.key(), "visualstudio.com is the same organization");
        assert_eq!(old.server, "https://dev.azure.com/contoso");
        let rest = PrRef::parse("https://dev.azure.com/contoso/3f2a/_apis/git/repositories/9b1c/pullRequests/1234").unwrap();
        assert_eq!((rest.key(), rest.project.as_str(), rest.repo.as_str()), (ado.key(), "3f2a", "9b1c"), "the REST link of az repos pr create");
        let unnamed = PrRef::parse("https://dev.azure.com/contoso/_git/tools/pullrequest/5").unwrap();
        assert_eq!((unnamed.project.as_str(), unnamed.repo.as_str()), ("tools", "tools"), "a repository named like its project");

        for bad in ["", "https://github.com/octo/app/issues/42", "https://github.com/octo/app/pull/0", "ftp://github.com/o/r/pull/1", "javascript:alert(1)", "octo/app", "o/r#x", "https://dev.azure.com/contoso/p/_git/r/commits/5", "https://evil host/o/r/pull/1"] {
            assert!(PrRef::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(thread_key("dev.azure.com/my org#12"), "pr:dev.azure.com/my-org#12");
    }

    fn pr(owner: &str) -> TrackedPr {
        TrackedPr::new(PrRef::parse("https://github.com/octo/app/pull/42").unwrap(), Role::Owned, owner, "Fix flaky test", "g1", "handed off", 1_000)
    }

    #[test]
    fn the_registry_tracks_once_per_pull_request_and_survives_a_round_trip() {
        let mut registry = Registry::default();
        assert_eq!(registry.track(pr("s1"), false), Ok(Tracked::New));
        assert_eq!(registry.track(pr("s1"), false), Ok(Tracked::Same), "a second hand-off changes nothing");
        assert!(registry.track(pr("s2"), false).unwrap_err().contains("already tracked for “Fix flaky test”"), "a session cannot take another's");
        assert_eq!(registry.track(pr("s2"), true), Ok(Tracked::Updated), "you and canvas-wide agents can");
        assert_eq!(registry.get("github.com/octo/app#42").unwrap().owner, "s2");
        // It survives a restart (the service writes and reads the same JSON).
        let text = serde_json::to_string_pretty(&registry).unwrap();
        let again: Registry = serde_json::from_str(&text).unwrap();
        assert_eq!(again, registry);
        // Ended pull requests can be tracked again; untrack removes.
        registry.get_mut("github.com/octo/app#42").unwrap().state = PrState::Merged;
        assert_eq!(registry.track(pr("s1"), false), Ok(Tracked::Updated));
        assert_eq!(registry.get("github.com/octo/app#42").unwrap().state, PrState::Active);
        assert!(registry.untrack("github.com/octo/app#42").is_some() && registry.prs.is_empty());
        // A limit, and ended ones are pruned after 30 days.
        for n in 0..MAX_TRACKED {
            let p = TrackedPr::new(PrRef::parse(&format!("o/r#{}", n + 1)).unwrap(), Role::Owned, "s1", "x", "", "", 0);
            registry.track(p, false).unwrap();
        }
        assert!(registry.track(pr("s1"), false).unwrap_err().contains("at most"));
        registry.prs[0].state = PrState::Closed;
        registry.prune(KEEP_ENDED_MS + 1);
        assert_eq!(registry.prs.len(), MAX_TRACKED - 1);
    }

    #[test]
    fn dispatches_are_deduplicated_per_key() {
        let mut registry = Registry::default();
        registry.track(pr("s1"), false).unwrap();
        let key = "github.com/octo/app#42";
        let request = |k: &str, text: &str| DispatchRequest { key: k.into(), event: "pr.comment.new".into(), text: text.into(), from: "PR coordinator".into(), from_key: "kw".into() };
        registry.get_mut(key).unwrap().new_comments = 3;
        let Dispatched::New(id) = registry.dispatch(key, request("comments:c:9", "Address the 3 new comments"), 5).unwrap() else { panic!() };
        assert_eq!((registry.get(key).unwrap().dispatches[0].topic.as_str(), registry.get(key).unwrap().new_comments), ("3 new review comment(s)", 0), "what it is about is kept; the new comments are taken care of");
        assert_eq!(registry.dispatch(key, request("comments:c:9", "again, worded differently"), 6).unwrap(), Dispatched::Duplicate(id.clone(), DispatchState::Intended), "one dispatch per event");
        assert!(matches!(registry.dispatch(key, request("checks:abc:build", "Investigate the build"), 7).unwrap(), Dispatched::New(_)));
        // Without a key the text is the key.
        assert!(matches!(registry.dispatch(key, request("", "Rebase please"), 8).unwrap(), Dispatched::New(_)));
        assert!(matches!(registry.dispatch(key, request(" ", "Rebase  please"), 9).unwrap(), Dispatched::Duplicate(..)));
        // A dismissed or failed one can be dispatched again.
        let (pr, at) = registry.find_dispatch(&id).unwrap();
        pr.dispatches[at].state = DispatchState::Dismissed;
        assert!(matches!(registry.dispatch(key, request("comments:c:9", "Address them"), 10).unwrap(), Dispatched::New(_)));
        assert!(registry.dispatch(key, request("x", "  "), 11).unwrap_err().contains("no text"));
        assert!(registry.dispatch("github.com/o/other#1", request("x", "y"), 12).unwrap_err().contains("not tracked"));
        let d = &registry.get(key).unwrap().dispatches[0];
        assert_eq!((d.to.as_str(), d.to_name.as_str(), d.from.as_str()), ("s1", "Fix flaky test", "PR coordinator"), "it goes to the owner");
    }

    const GH_VIEW: &str = r#"{"number":42,"url":"https://github.com/octo/app/pull/42","title":"Fix flaky upload test","state":"OPEN","isDraft":false,
        "headRefOid":"abc123def456789","reviewDecision":"CHANGES_REQUESTED","author":{"login":"octo-dev"},
        "comments":[{"id":"IC_1","author":{"login":"ana"},"body":"Please add a test\nfor the retry","url":"https://github.com/octo/app/pull/42#issuecomment-1","isMinimized":false},
                    {"id":"IC_2","author":{"login":"octo-dev"},"body":"Done","url":"https://github.com/octo/app/pull/42#issuecomment-2"}],
        "reviews":[{"id":"PRR_1","author":{"login":"ana"},"state":"COMMENTED","body":""},
                   {"id":"PRR_2","author":{"login":"ben"},"state":"CHANGES_REQUESTED","body":"Retry logic is wrong"}],
        "statusCheckRollup":[{"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/octo/app/actions/runs/1"},
                             {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":"","detailsUrl":""}]}"#;
    const GH_INLINE: &str = r#"[{"id":101,"user":{"login":"ana"},"body":"nit: rename","html_url":"https://github.com/octo/app/pull/42#discussion_r101","path":"src/upload.rs"}][]"#;

    #[test]
    fn github_output_becomes_a_poll() {
        let poll = parse_github(GH_VIEW, GH_INLINE).unwrap();
        assert_eq!((poll.state, poll.author.as_str(), poll.head.as_str(), poll.decision.as_str()), (PrState::Active, "octo-dev", "abc123def456789", "changes_requested"));
        let ids: Vec<&str> = poll.comments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c:IC_1", "c:IC_2", "i:101"], "issue and inline comments; an empty COMMENTED review is no comment");
        assert_eq!(poll.comments[0].text, "Please add a test for the retry", "one line");
        assert_eq!(poll.comments[2].path, "src/upload.rs");
        assert_eq!(poll.reviews, [ReviewItem { id: "r:PRR_2".into(), author: "ben".into(), state: "changes_requested".into(), text: "Retry logic is wrong".into() }]);
        assert_eq!(poll.checks.iter().map(|c| (c.name.as_str(), c.state)).collect::<Vec<_>>(), [("build", CheckState::Pass), ("lint", CheckState::Pending)]);
        let merged = parse_github(&GH_VIEW.replace("\"OPEN\"", "\"MERGED\""), "[]").unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert!(parse_github("gh: not logged in", "").is_err());
    }

    #[test]
    fn polls_diff_against_watermarks_and_end_states_stop_monitoring() {
        let mut tracked = pr("s1");
        let mut poll = parse_github(GH_VIEW, GH_INLINE).unwrap();
        assert!(tracked.apply(&poll, 10).is_empty(), "the first poll is the baseline");
        assert_eq!((tracked.comments, tracked.monitor.status, tracked.monitor.polls), (2, MonitorStatus::Ok, 1), "the author's own comment does not count");
        // Three new comments by reviewers, one by the author, and a failing check.
        for (i, who) in ["ana", "ben", "ana", "octo-dev"].iter().enumerate() {
            poll.comments.push(Comment { id: format!("i:{}", 200 + i), author: who.to_string(), url: format!("https://github.com/octo/app/pull/42#discussion_r{}", 200 + i), text: format!("comment {i}"), path: String::new() });
        }
        poll.checks[0].state = CheckState::Fail;
        let changes = tracked.apply(&poll, 20);
        let Change::Comments(new) = &changes[0] else { panic!("{changes:?}") };
        assert_eq!(new.len(), 3);
        assert!(matches!(&changes[1], Change::ChecksFailed { checks, .. } if checks[0].name == "build"));
        assert_eq!(tracked.new_comments, 3);
        let (kind, data) = changes[0].event(&tracked);
        assert_eq!((kind.as_str(), data["count"].as_u64(), data["dispatch_key"].as_str()), ("pr.comment.new", Some(3), Some("comments:i:202")));
        assert_eq!((data["owner"].as_str(), data["url"].as_str()), (Some("s1"), Some("https://github.com/octo/app/pull/42")));
        assert_eq!(data["comments"][0]["url"], "https://github.com/octo/app/pull/42#discussion_r200");
        let (kind, data) = changes[1].event(&tracked);
        assert_eq!((kind.as_str(), data["dispatch_key"].as_str()), ("pr.checks.failed", Some("checks:abc123def456:build")));
        // The same poll again: nothing new (no duplicate events).
        assert!(tracked.apply(&poll, 30).is_empty());
        // A new head with the same failure is reported again; an approval is a review event.
        poll.head = "fff000".into();
        poll.reviews.push(ReviewItem { id: "r:PRR_3".into(), author: "ana".into(), state: "approved".into(), text: String::new() });
        let changes = tracked.apply(&poll, 40);
        assert!(matches!(&changes[0], Change::ChecksFailed { head, .. } if head == "fff000"));
        assert_eq!(changes[1].event(&tracked).0, "pr.approved");
        // Merged: one event, monitoring stops for good.
        poll.state = PrState::Merged;
        let changes = tracked.apply(&poll, 50);
        assert_eq!(changes.iter().map(|c| c.event(&tracked).0).collect::<Vec<_>>(), ["pr.merged"]);
        assert_eq!(tracked.monitor.status, MonitorStatus::Stopped);
        assert!(!tracked.due(u64::MAX), "never polled again");
        let mut registry = Registry { prs: vec![tracked.clone()] };
        assert!(registry.due(u64::MAX).is_empty());
        registry.prs[0].state = PrState::Active;
        registry.prs[0].monitor.status = MonitorStatus::Ok;
        assert_eq!(registry.due(u64::MAX).len(), 1);
        // A failure that was already there when tracking started still needs its owner.
        let mut fresh = pr("s1");
        assert!(matches!(fresh.apply(&poll_with_failure(), 1).as_slice(), [Change::ChecksFailed { .. }]));
    }

    fn poll_with_failure() -> Poll {
        Poll { head: "h1".into(), checks: vec![Check { name: "build".into(), state: CheckState::Fail, url: String::new() }], ..Default::default() }
    }

    #[test]
    fn reviewed_pull_requests_only_report_their_end_and_failed_polls_back_off() {
        let mut reviewed = TrackedPr::new(PrRef::parse("o/r#5").unwrap(), Role::Reviewed, "rev", "Reviewer", "", "review", 0);
        let mut poll = poll_with_failure();
        poll.comments.push(Comment { id: "c:1".into(), author: "x".into(), ..Default::default() });
        assert!(reviewed.apply(&poll, 1).is_empty());
        poll.comments.push(Comment { id: "c:2".into(), author: "y".into(), ..Default::default() });
        assert!(reviewed.apply(&poll, 2).is_empty(), "comments on a pull request you reviewed are not dispatched");
        poll.state = PrState::Closed;
        assert_eq!(reviewed.apply(&poll, 3), [Change::Ended(PrState::Closed)]);
        let mut tracked = pr("s1");
        tracked.poll_failed(true, "gh is not installed", 300_000, 100);
        assert_eq!((tracked.monitor.status, tracked.monitor.next_ms), (MonitorStatus::Unavailable, 100 + UNAVAILABLE_RETRY_MS));
        assert!(tracked.needs_you());
        tracked.poll_failed(false, "HTTP 502", 300_000, 200);
        assert_eq!(tracked.monitor.next_ms, 200 + 1_200_000, "doubled per failure");
        assert_eq!(backoff(300_000, 30), MAX_BACKOFF_MS);
    }

    #[test]
    fn azure_devops_output_becomes_a_poll() {
        let pr = PrRef::parse("https://dev.azure.com/contoso/Web/_git/app/pullrequest/77").unwrap();
        let show = r#"{"pullRequestId":77,"status":"active","isDraft":true,"title":"Retry uploads","createdBy":{"displayName":"Dev","uniqueName":"dev@contoso.com"},
            "lastMergeSourceCommit":{"commitId":"c0ffee"},"reviewers":[{"displayName":"Ana","uniqueName":"ana@contoso.com","vote":-5},{"displayName":"Ben","uniqueName":"ben@contoso.com","vote":0}]}"#;
        let threads = r#"{"count":3,"value":[
            {"id":5,"isDeleted":false,"threadContext":{"filePath":"/src/up.cs"},"comments":[{"id":1,"author":{"displayName":"Ana","uniqueName":"ana@contoso.com"},"content":"Handle 429","commentType":"text"},{"id":2,"author":{"displayName":"Dev","uniqueName":"dev@contoso.com"},"content":"ok","commentType":"text"}]},
            {"id":6,"isDeleted":false,"comments":[{"id":1,"author":{"displayName":"System"},"content":"Policy updated","commentType":"system"}]},
            {"id":7,"isDeleted":true,"comments":[{"id":1,"author":{"uniqueName":"x"},"content":"gone"}]}]}"#;
        let policies = r#"[{"status":"rejected","configuration":{"type":{"displayName":"Build"},"settings":{"displayName":"CI","buildDefinitionId":3}},"context":{"buildId":991}},
            {"status":"approved","configuration":{"type":{"displayName":"Minimum number of reviewers"},"settings":{}}}]"#;
        let poll = parse_ado(&pr, show, threads, policies).unwrap();
        assert_eq!((poll.state, poll.draft, poll.head.as_str(), poll.author.as_str(), poll.decision.as_str()), (PrState::Active, true, "c0ffee", "dev@contoso.com", "changes_requested"));
        assert_eq!(poll.comments.iter().map(|c| (c.id.as_str(), c.author.as_str())).collect::<Vec<_>>(), [("t:5/1", "ana@contoso.com"), ("t:5/2", "dev@contoso.com")], "system and deleted ones are not comments");
        assert_eq!(poll.comments[0].url, "https://dev.azure.com/contoso/Web/_git/app/pullrequest/77?_a=overview&discussionId=5");
        assert_eq!(poll.reviews[0].id, "v:ana@contoso.com:-5");
        assert_eq!(poll.checks, [Check { name: "CI".into(), state: CheckState::Fail, url: "https://dev.azure.com/contoso/Web/_build/results?buildId=991".into() }]);
        assert_eq!(parse_ado(&pr, &show.replace("\"active\"", "\"abandoned\""), "{}", "[]").unwrap().state, PrState::Abandoned);
        assert_eq!(parse_ado(&pr, &show.replace("\"active\"", "\"completed\""), "{}", "[]").unwrap().state, PrState::Merged);
    }

    #[test]
    fn settings_parse_and_follow_ups_say_what_they_permit() {
        assert_eq!(PrSettings::parse("off"), Ok(None));
        assert_eq!(PrSettings::parse("on").unwrap(), Some(PrSettings::default()));
        let s = PrSettings::parse("on every 10m manual allow push, reply").unwrap().unwrap();
        assert_eq!((s.poll_minutes, s.auto_track, s.allow.clone()), (10, false, vec!["push".to_owned(), "reply".to_owned()]));
        assert_eq!(PrSettings::parse(&s.text()).unwrap().unwrap(), s, "the prompt shows what it parses");
        assert!(PrSettings::parse("on allow deploy").unwrap_err().contains("deploy"));
        assert!(PrSettings::default().allow.is_empty(), "read-only by default");
        let none = permissions_text(&[]);
        assert!(none.contains("no permission to push, reply, resolve, vote, merge"), "{none}");
        let some = permissions_text(&["push".into()]);
        assert!(some.starts_with("allowed: push fixes") && some.contains("reply, resolve, vote, merge"), "{some}");
        let mut tracked = pr("s1");
        tracked.dispatches.push(Dispatch { id: "d-1-0001".into(), text: "Address the comments".into(), ..Default::default() });
        let text = dispatch_text(&tracked, &tracked.dispatches[0], &[]);
        assert!(text.starts_with("PR octo/app#42 (https://github.com/octo/app/pull/42): Address the comments [Canvas: tracking grants no permission"), "{text}");
    }

    #[test]
    fn the_view_drops_watermarks_and_puts_what_needs_you_first() {
        let mut registry = Registry::default();
        let mut a = pr("s1");
        a.seen.comments = vec!["c:1".into()];
        a.last_change_ms = 5;
        let mut b = TrackedPr::new(PrRef::parse("o/r#2").unwrap(), Role::Owned, "s2", "B", "", "", 1);
        b.dispatches = (0..10).map(|i| Dispatch { id: format!("d{i}"), state: if i == 9 { DispatchState::Blocked } else { DispatchState::Delivered }, ..Default::default() }).collect();
        registry.prs = vec![a, b];
        let view = PrsView::of("c1", &registry, vec!["PR coordinator".into()], String::new(), Vec::new());
        assert_eq!(view.prs[0].key, "github.com/o/r#2", "a blocked follow-up first");
        assert_eq!(view.prs[0].dispatches.len(), VIEW_DISPATCHES);
        assert!(view.prs.iter().all(|p| p.seen.is_empty()));
        assert_eq!((view.active(), view.needing_you()), (2, 1));
        assert!(!serde_json::to_string(&view).unwrap().contains("\"seen\""));
    }
}
