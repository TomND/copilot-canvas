//! Restore after a restart (README "Restore after a restart"): the service
//! records what its terminals run (`<home>/run/restore-<instance>.json`, the
//! running set) and, when it starts again after a restart, a crash, a killed
//! process or a reboot, resumes them through the normal launch path with no
//! client needed. Agents that were busy get a note typed in once they have
//! settled (through the bus mailbox); idle ones are resumed without one.
//!
//! An entry goes when you stop its session (Stop, *Stop all and quit*, a
//! removal) or its program exits on its own; the service going away for any
//! other reason keeps it. Passive runs are not recorded (the bus queues an
//! interrupted run again, `passive::load_state`), nor are VM boxes (their
//! satellites keep running in the guest and the hub adopts them again).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use canvas_model::bus::{Mail, RestoreSkip, RestoreView};
use serde::{Deserialize, Serialize};

use super::{Shared, lock, log};
use crate::agents::{AgentKind, Provide};
use crate::paths::now_ms;
use crate::protocol::{Activity, AgentLaunch, LaunchKind, SessionSummary};

/// A program that exits on its own stays recorded this long (the machine may
/// be going down: its programs get the signal before the service does).
const EXIT_GRACE_MS: u64 = 10_000;
/// Exits this soon before a shutdown began came with it (a logoff or reboot).
const SHUTDOWN_WINDOW_MS: u64 = 3_000;
/// A resumed agent has settled when it ran this long and has been quiet for `QUIET_MS`.
const SETTLE_MS: u64 = 6_000;
const QUIET_MS: u64 = 2_000;
/// It counts as settled after this long whatever it prints (a clock in its UI).
const SETTLE_MAX_MS: u64 = 45_000;
/// Launch requests not confirmed by a running terminal by then are forgotten.
const REQUEST_TTL_MS: u64 = 60_000;
/// What a session does changes at every turn: such changes are written at
/// most this often (a launch, an exit or a learned id is written at once).
const SAVE_GAP_MS: u64 = 5_000;
/// After a restart, at most this many resumed agents start at once (each
/// loads its whole history: one is a 93 MB file), with at least
/// `RESTORE_GAP_MS` between two launches.
const RESTORE_AT_ONCE: usize = 2;
const RESTORE_GAP_MS: u64 = 2_000;
/// A resumed agent is starting until it printed and was quiet for
/// `STARTING_QUIET_MS`, or for at most `STARTING_MAX_MS`.
const STARTING_QUIET_MS: u64 = 2_000;
const STARTING_MAX_MS: u64 = 20_000;
/// A session still live at start (the previous service's CLI still closing)
/// is checked again every `LIVE_RETRY_MS` until this long after the restore began.
const LIVE_WAIT_MS: u64 = 30_000;
const LIVE_RETRY_MS: u64 = 2_000;

/// What a session was doing when last seen (from its history and terminal).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Doing {
    #[serde(default)]
    pub working: bool,
    /// Waiting on a question or a permission request.
    #[serde(default)]
    pub waiting: bool,
    /// Its shortest recurring schedule (ms; 0: none).
    #[serde(default)]
    pub loop_ms: u64,
    #[serde(default)]
    pub name: String,
}

impl Doing {
    pub fn of(summary: Option<&SessionSummary>, progress_busy: bool) -> Self {
        let Some(s) = summary else { return Self { working: progress_busy, ..Default::default() } };
        Self { working: progress_busy || s.activity == Activity::Working, waiting: s.pending > 0, loop_ms: s.loop_ms, name: s.name.clone() }
    }

    pub fn idle(&self) -> bool {
        !self.working && !self.waiting && self.loop_ms == 0
    }
}

/// One terminal of the running set: how to start it again and what it did.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// The box (terminal) id.
    pub id: String,
    /// The launch that starts it again (an agent's resumes its conversation).
    pub launch: LaunchKind,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub started_ms: u64,
    /// The agent's own session id once learned (OpenCode); empty: the box id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub native: String,
    #[serde(default)]
    pub doing: Doing,
    /// Its program exited on its own this recently before the service went away.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exited_ms: u64,
    /// When it last started or ended a turn (or started): the restore resumes
    /// the most recently active first.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_ms: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// The running set as written to disk.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RunningSet {
    #[serde(default)]
    pub saved_ms: u64,
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

/// `<home>/run/restore-<instance>.json`.
pub fn path(home: &Path, instance: &str) -> PathBuf {
    home.join("run").join(format!("restore-{instance}.json"))
}

pub fn load(path: &Path) -> RunningSet {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save(path: &Path, set: &RunningSet) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(set) {
        let _ = canvas_sys::fs::write_private(path, &bytes);
    }
}

/// The launch that starts a recorded terminal again: an agent resumes its
/// conversation (never its first prompt or fork again); `None`: not
/// recorded (passive runs, VM boxes; plain commands are recorded from the canvas).
pub fn relaunch(kind: &LaunchKind, native: Option<&str>) -> Option<LaunchKind> {
    match kind {
        LaunchKind::Copilot { .. } => Some(LaunchKind::Copilot { new_name: None, prompt: None, resume: true }),
        LaunchKind::CopilotWith { args, env, .. } => Some(LaunchKind::CopilotWith { new_name: None, prompt: None, resume: true, args: args.clone(), env: env.clone() }),
        LaunchKind::Agent(launch) => {
            if launch.headless_prompt.is_some() || canvas_model::vm::parse_vm_agent(&launch.agent).is_some() {
                return None;
            }
            let mut again: AgentLaunch = (**launch).clone();
            again.resume = true;
            again.prompt = None;
            again.fork_from = None;
            if let Some(native) = native.filter(|n| !n.is_empty()) {
                again.native_id = Some(native.to_owned());
            }
            Some(LaunchKind::Agent(Box::new(again)))
        }
        LaunchKind::Command { .. } => Some(kind.clone()),
    }
}

/// The agent of a recorded launch (`None`: a plain command).
fn agent_of(kind: &LaunchKind) -> Option<AgentKind> {
    match kind {
        LaunchKind::Copilot { .. } | LaunchKind::CopilotWith { .. } => Some(AgentKind::Copilot),
        LaunchKind::Agent(launch) => AgentKind::parse(&launch.agent),
        LaunchKind::Command { .. } => None,
    }
}

/// `15m`, `2h`, `45s`, `1d`.
pub fn interval(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 if s.is_multiple_of(3_600) => format!("{}h", s / 3_600),
        3_600..86_400 => format!("{}h{}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d", s / 86_400),
    }
}

/// The note typed into a resumed agent, or `None` when it was idle with no
/// loop (it is resumed without one: nothing to do, no tokens spent).
pub fn note(doing: &Doing, restarted_at: &str) -> Option<String> {
    if doing.idle() {
        return None;
    }
    let mut was = Vec::new();
    if doing.working {
        was.push("working on a task".to_owned());
    }
    if doing.loop_ms > 0 {
        was.push(format!("running a loop every {}", interval(doing.loop_ms)));
    }
    if doing.waiting {
        was.push("waiting for an answer to a question or a permission request".to_owned());
    }
    let was = match was.len() {
        1 => was.remove(0),
        _ => {
            let last = was.pop().unwrap_or_default();
            format!("{} and {last}", was.join(", "))
        }
    };
    let mut text = format!(
        "[Atelier] The machine running this session restarted at {restarted_at} and interrupted you. Before the restart you were {was}. Restore the state you were in: continue the task you were doing, restart any loop or schedule you had, and re-check anything that was in progress."
    );
    if doing.loop_ms > 0 {
        text.push_str(&format!(" Your loop no longer runs: set it up again with the same interval ({}).", interval(doing.loop_ms)));
    }
    if doing.waiting {
        text.push_str(" The question or permission request you were waiting on was lost: ask it again.");
    }
    Some(text)
}

/// A resumed agent whose note waits until it has settled.
#[derive(Clone, Debug)]
struct Pending {
    doing: Doing,
    launched_ms: u64,
}

#[derive(Default)]
struct Inner {
    set: RunningSet,
    /// Launches asked for and not yet running: id → (relaunch, cwd, agent, ms).
    requests: HashMap<String, (LaunchKind, String, u64)>,
    /// Terminals you asked to stop (graceful stops tell the backend first).
    stops: HashSet<String>,
    /// Resumed agents waiting to settle.
    pending: HashMap<String, Pending>,
    /// Entries whose launch by the restore is under way, and those of them
    /// cancelled meanwhile (stopped as soon as the launch is done: R3-B-04).
    starting: HashSet<String>,
    cancelled: HashSet<String>,
    view: Option<RestoreView>,
    /// Notes for the bus loop to queue (front of the mailbox).
    notes: Vec<Mail>,
    /// The set changed: written at the next sync.
    dirty: bool,
    /// Only what sessions do changed: written within `SAVE_GAP_MS`.
    doing_dirty: bool,
    /// Numbers each copy of the set taken for writing (a later one wins).
    version: u64,
    /// When the set was last taken for writing.
    saved_at: u64,
    /// Off (`atelier config restore off`): nothing is recorded or resumed.
    off: bool,
    /// When the service began shutting down (0: running).
    shutdown_ms: u64,
}

impl Inner {
    /// A copy of the set to write once the lock is released, when it changed
    /// (`force`: whatever only sessions' doings changed meanwhile).
    fn take_save(&mut self, now: u64, force: bool) -> Option<(u64, RunningSet)> {
        let due = self.dirty || (self.doing_dirty && (force || now.saturating_sub(self.saved_at) >= SAVE_GAP_MS));
        if !due {
            return None;
        }
        (self.dirty, self.doing_dirty) = (false, false);
        self.version += 1;
        self.saved_at = now;
        self.set.saved_ms = now;
        Some((self.version, self.set.clone()))
    }
}

/// Writes the running set (the file, or a test's stand-in).
type Saver = Box<dyn Fn(&Path, &RunningSet) + Send + Sync>;

/// The restore state of a service.
pub struct Restore {
    path: PathBuf,
    inner: std::sync::Mutex<Inner>,
    /// The version last written; held while writing, never with `inner`: a
    /// slow disk (a flush can take seconds) never holds up launches, client
    /// connects, the bus or exits, which all take `inner`.
    written: std::sync::Mutex<u64>,
    saver: Saver,
}

impl Restore {
    pub fn new(home: &Path, instance: &str) -> Self {
        Self::with_saver(home, instance, Box::new(save))
    }

    fn with_saver(home: &Path, instance: &str, saver: Saver) -> Self {
        let path = path(home, instance);
        let off = !canvas_common::settings::Settings::load(home).restore();
        Self { inner: std::sync::Mutex::new(Inner { set: load(&path), off, ..Default::default() }), path, written: std::sync::Mutex::new(0), saver }
    }

    /// Writes a copy taken by [`Inner::take_save`] (after `inner` was
    /// released), unless a newer copy was written meanwhile.
    fn write(&self, job: Option<(u64, RunningSet)>) {
        let Some((version, set)) = job else { return };
        let mut written = lock(&self.written);
        if *written >= version {
            return;
        }
        (self.saver)(&self.path, &set);
        *written = version;
    }

    /// A client (or the restore, or a passive run) asked to launch `id`.
    pub fn requested(&self, id: &str, kind: &LaunchKind, cwd: &str) {
        let mut inner = lock(&self.inner);
        if inner.off {
            return;
        }
        match relaunch(kind, None) {
            Some(again) => {
                inner.requests.insert(id.to_owned(), (again, cwd.to_owned(), now_ms()));
            }
            None => {
                inner.requests.remove(id);
            }
        }
    }

    /// You asked terminal `id` to stop, or (`shutting_down`) the service is going away.
    pub fn stopping(&self, id: &str, shutting_down: bool) {
        if shutting_down {
            self.shutdown(now_ms());
        } else {
            lock(&self.inner).stops.insert(id.to_owned());
        }
    }

    /// Terminal `id`'s program exited: `user`: you stopped it (a graceful
    /// stop asked first, or a forced one outside a shutdown).
    pub fn exited(&self, id: &str, forced: bool, shutting_down: bool, now: u64) -> &'static str {
        let mut inner = lock(&self.inner);
        let asked = inner.stops.remove(id);
        // Resumed after a restart and gone before it settled: not resumed after all.
        if inner.pending.remove(id).is_some()
            && let Some(view) = inner.view.as_mut()
        {
            view.resuming.retain(|r| r != id);
            view.interrupted.push(RestoreSkip { id: id.to_owned(), why: "it exited right after it was resumed".into() });
        }
        if !inner.set.entries.contains_key(id) {
            return "not recorded";
        }
        if asked || (forced && !shutting_down) {
            inner.set.entries.remove(id);
            inner.dirty = true;
            return "removed (stopped by you)";
        }
        if shutting_down {
            drop(inner);
            self.shutdown(now);
            return "kept (the service is going away)";
        }
        if let Some(entry) = inner.set.entries.get_mut(id) {
            entry.exited_ms = now;
        }
        inner.dirty = true;
        "exited on its own (removed shortly unless the service goes away)"
    }

    /// The service is going away: exits from before the shutdown began are
    /// final (beyond the window in which a logoff or reboot ends programs first).
    pub fn shutdown(&self, now: u64) {
        let job = {
            let mut inner = lock(&self.inner);
            if inner.shutdown_ms > 0 {
                return;
            }
            inner.shutdown_ms = now;
            let before = inner.set.entries.len();
            inner.set.entries.retain(|_, e| e.exited_ms == 0 || now.saturating_sub(e.exited_ms) < SHUTDOWN_WINDOW_MS);
            if inner.set.entries.len() != before {
                inner.dirty = true;
            }
            inner.take_save(now, true)
        };
        self.write(job);
    }

    /// Terminal `id` exited after it was launched again (its exit came late):
    /// a stop you asked of the earlier run is spent.
    pub fn relaunched(&self, id: &str) {
        lock(&self.inner).stops.remove(id);
    }

    /// The notes to queue now (the bus loop owns the mailbox).
    pub fn take_notes(&self) -> Vec<Mail> {
        std::mem::take(&mut lock(&self.inner).notes)
    }

    /// Writes the running set as it is (the service is exiting).
    pub fn save_now(&self) {
        let job = {
            let mut inner = lock(&self.inner);
            if inner.off {
                return;
            }
            inner.take_save(now_ms(), true)
        };
        self.write(job);
    }

    /// Whether the restore holds messages for `id` back (it is being resumed
    /// and has not settled: its note goes first).
    pub fn holds(&self, id: &str) -> bool {
        lock(&self.inner).pending.contains_key(id)
    }

    pub fn view(&self) -> Option<RestoreView> {
        lock(&self.inner).view.clone()
    }

    /// The recorded set (tests and `doctor`).
    pub fn entries(&self) -> Vec<Entry> {
        lock(&self.inner).set.entries.values().cloned().collect()
    }

    /// A box still waiting for its turn in the restore was removed from the
    /// canvas or stopped: it is not resumed when its turn comes, and leaves
    /// the running set (R2-C-01: it was resumed anyway, unseen, with the
    /// note to continue its task, and kept across every restart). One
    /// already resumed is left alone (it runs: Stop ends it as usual).
    /// Answers whether it was waiting.
    pub fn cancel(&self, id: &str, why: &str) -> bool {
        let mut inner = lock(&self.inner);
        let waiting = !inner.pending.contains_key(id) && inner.view.as_ref().is_some_and(|v| v.resuming.iter().any(|r| r == id));
        if !waiting {
            return false;
        }
        inner.set.entries.remove(id);
        inner.dirty = true;
        // Its launch by the restore is under way: what it starts is stopped (R3-B-04).
        if inner.starting.contains(id) {
            inner.cancelled.insert(id.to_owned());
        }
        if let Some(view) = inner.view.as_mut() {
            view.resuming.retain(|r| r != id);
            view.interrupted.push(RestoreSkip { id: id.to_owned(), why: why.to_owned() });
        }
        true
    }

    /// The restore begins launching `id`.
    fn launching(&self, id: &str) {
        let mut inner = lock(&self.inner);
        inner.starting.insert(id.to_owned());
        inner.cancelled.remove(id);
    }

    /// The restore's launch of `id` is done; answers whether it was
    /// cancelled meanwhile (then what it started is stopped, gets no note,
    /// and is not recorded again).
    fn launched(&self, id: &str) -> bool {
        let mut inner = lock(&self.inner);
        inner.starting.remove(id);
        let cancelled = inner.cancelled.remove(id);
        if cancelled {
            inner.requests.remove(id);
            if inner.set.entries.remove(id).is_some() {
                inner.dirty = true;
            }
        }
        cancelled
    }
}

/// A client's restore request (`{"action": "restore", "op": "cancel", "id"}`:
/// a box waiting for its turn that you stopped); false when `value` is none.
/// Older services ignore it.
pub(super) fn action(shared: &Shared, value: &serde_json::Value) -> bool {
    if value["action"].as_str() != Some("restore") {
        return false;
    }
    if value["op"].as_str() == Some("cancel")
        && let Some(id) = value["id"].as_str()
    {
        if shared.restore.cancel(id, "stopped before its turn") {
            log(&shared.instance, &format!("restore: {id} stopped before its turn; not resumed"));
        } else if shared.restore.holds(id) {
            // Resumed just before the window's Stop came (it still showed it waiting): stopped (R3-B-04).
            log(&shared.instance, &format!("restore: {id} was stopped right after it was resumed"));
            let _ = shared.host.stop_terminal(id, false);
        }
    }
    true
}

/// Terminal facts the running set is kept from.
pub struct TermFacts {
    pub id: String,
    pub alive: bool,
    pub agent: bool,
    pub remote: bool,
    pub started_ms: u64,
    pub cols: u16,
    pub rows: u16,
    pub progress_busy: bool,
}

impl Restore {
    /// Records launches whose terminal now runs, a plain command from its
    /// canvas box (`command`), learned agent session ids, what each session
    /// does (`doing`), and drops exits past their grace. Writes the file when
    /// it changed (never while the service is going away), after the lock is
    /// released: nothing that takes it waits for the disk.
    pub fn sync(&self, terms: &[TermFacts], natives: &HashMap<String, String>, command: &dyn Fn(&str) -> Option<(LaunchKind, String)>, doing: &dyn Fn(&str, bool) -> Doing, now: u64) {
        let job = {
            let mut inner = lock(&self.inner);
            if inner.off || inner.shutdown_ms > 0 {
                return;
            }
            Self::record(&mut inner, terms, natives, command, doing, now);
            inner.take_save(now, false)
        };
        self.write(job);
    }

    fn record(inner: &mut Inner, terms: &[TermFacts], natives: &HashMap<String, String>, command: &dyn Fn(&str) -> Option<(LaunchKind, String)>, doing: &dyn Fn(&str, bool) -> Doing, now: u64) {
        let Inner { set, requests, dirty, doing_dirty, pending, .. } = inner;
        for t in terms.iter().filter(|t| t.alive && !t.remote) {
            let request = requests.get(&t.id).filter(|(_, _, at)| t.started_ms + 1_000 >= *at).cloned();
            let recorded = set.entries.get(&t.id).is_some_and(|e| e.started_ms == t.started_ms);
            if let Some((launch, cwd, _)) = request.filter(|_| !recorded) {
                requests.remove(&t.id);
                // A resume after a restart keeps what it did before until its note is
                // queued; any other launch starts afresh (its session id comes from `natives`).
                let doing = set.entries.get(&t.id).filter(|_| pending.contains_key(&t.id)).map(|e| e.doing.clone()).unwrap_or_default();
                set.entries.insert(t.id.clone(), Entry { id: t.id.clone(), launch, cwd, cols: t.cols, rows: t.rows, started_ms: t.started_ms, native: String::new(), doing, exited_ms: 0, active_ms: t.started_ms });
                *dirty = true;
            } else if !recorded
                && !t.agent
                && !set.entries.contains_key(&t.id)
                && let Some((launch, cwd)) = command(&t.id)
            {
                set.entries.insert(t.id.clone(), Entry { id: t.id.clone(), launch, cwd, cols: t.cols, rows: t.rows, started_ms: t.started_ms, native: String::new(), doing: Doing::default(), exited_ms: 0, active_ms: t.started_ms });
                *dirty = true;
            }
        }
        requests.retain(|_, (_, _, at)| now.saturating_sub(*at) < REQUEST_TTL_MS);
        let alive: HashSet<&str> = terms.iter().filter(|t| t.alive).map(|t| t.id.as_str()).collect();
        for entry in set.entries.values_mut() {
            if !alive.contains(entry.id.as_str()) {
                continue;
            }
            if let Some(native) = natives.get(&entry.id).filter(|n| **n != entry.native) {
                entry.native = native.clone();
                if let LaunchKind::Agent(launch) = &mut entry.launch {
                    launch.native_id = Some(native.clone());
                }
                *dirty = true;
            }
            // A resumed agent keeps what it did before the restart until its note is queued.
            if agent_of(&entry.launch).is_some() && !pending.contains_key(&entry.id) {
                let progress = terms.iter().any(|t| t.id == entry.id && t.progress_busy);
                let now_doing = doing(if entry.native.is_empty() { &entry.id } else { &entry.native }, progress);
                if now_doing != entry.doing {
                    entry.doing = now_doing;
                    entry.active_ms = now;
                    *doing_dirty = true;
                }
            }
        }
        let before = set.entries.len();
        set.entries.retain(|id, e| e.exited_ms == 0 || alive.contains(id.as_str()) || now.saturating_sub(e.exited_ms) < EXIT_GRACE_MS);
        // A relaunch after an exit runs again.
        for entry in set.entries.values_mut().filter(|e| e.exited_ms > 0 && alive.contains(e.id.as_str())) {
            entry.exited_ms = 0;
            *dirty = true;
        }
        if set.entries.len() != before {
            *dirty = true;
        }
    }
}

/// What happened to one recorded entry at start.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Launched again; its note (if any) follows once it has settled.
    Resumed,
    /// A client started it first.
    Running,
    /// Its session is open in another program (who: "already running in …"):
    /// tried again for a while (the previous service's CLI may still be closing).
    Live(String),
    /// Stopped or launched again by you before its turn came: nothing to do.
    Gone,
    Skipped(String),
}

/// A recorded entry waiting for its turn.
#[derive(Clone, Debug)]
struct Waiting {
    entry: Entry,
    /// Not tried again before then (its session was live elsewhere).
    not_before: u64,
}

/// The entries resumed after a restart, a few at a time and the most
/// recently active first: every agent loads its whole history as it starts,
/// and all of them at once starved each other (and your typing).
#[derive(Default)]
struct Queue {
    waiting: Vec<Waiting>,
    /// Launched and still starting: (id, when).
    starting: Vec<(String, u64)>,
    last_launch: u64,
    /// When the restore began.
    began: u64,
}

/// How recently an entry was active: a busy one was busy until the service
/// went away (`previous_ms`); an idle one when it last started or ended a turn.
fn recency(entry: &Entry, previous_ms: u64) -> (u64, u64) {
    let active = entry.active_ms.max(entry.started_ms);
    let busy = agent_of(&entry.launch).is_some() && !entry.doing.idle();
    (if busy { previous_ms.max(active) } else { active }, active)
}

impl Queue {
    fn new(mut entries: Vec<Entry>, previous_ms: u64, now: u64) -> Self {
        entries.sort_by_key(|e| std::cmp::Reverse(recency(e, previous_ms)));
        Self { waiting: entries.into_iter().map(|entry| Waiting { entry, not_before: 0 }).collect(), began: now, ..Default::default() }
    }

    fn is_done(&self) -> bool {
        self.waiting.is_empty() && self.starting.is_empty()
    }

    /// Starts what may start now: at most `RESTORE_AT_ONCE` starting at
    /// once (`starting` tells whether a launch still is), `RESTORE_GAP_MS`
    /// apart. Answers the entries whose turn came, with what happened.
    /// One a client shows goes first (`shown`: it starts anyway; nothing new is started by it).
    fn step(&mut self, now: u64, start: &mut dyn FnMut(&Entry) -> Outcome, starting: &dyn Fn(&str, u64) -> bool, shown: &HashSet<String>) -> Vec<(String, Outcome)> {
        self.starting.retain(|(id, at)| starting(id, *at));
        let mut done = Vec::new();
        while self.starting.len() < RESTORE_AT_ONCE && (self.last_launch == 0 || now.saturating_sub(self.last_launch) >= RESTORE_GAP_MS) {
            let due = |w: &Waiting| w.not_before <= now;
            let Some(index) = self.waiting.iter().position(|w| due(w) && shown.contains(&w.entry.id)).or_else(|| self.waiting.iter().position(due)) else { break };
            let waiting = self.waiting.remove(index);
            let id = waiting.entry.id.clone();
            match start(&waiting.entry) {
                Outcome::Resumed => {
                    self.starting.push((id.clone(), now));
                    self.last_launch = now;
                    done.push((id, Outcome::Resumed));
                }
                Outcome::Live(_) if now.saturating_sub(self.began) < LIVE_WAIT_MS => {
                    self.waiting.insert(index, Waiting { not_before: now + LIVE_RETRY_MS, ..waiting });
                }
                Outcome::Live(holder) => done.push((id, Outcome::Skipped(format!("its session is {holder}")))),
                other => done.push((id, other)),
            }
        }
        done
    }
}

/// Starts again what the previous service ran (no client needed), a few
/// at a time, then keeps the running set and delivers the notes. Runs on its own thread.
pub(super) fn run(shared: Arc<Shared>) {
    let started = now_ms();
    let (entries, previous_ms, off) = {
        let inner = lock(&shared.restore.inner);
        (inner.set.entries.values().cloned().collect::<Vec<_>>(), inner.set.saved_ms, inner.off)
    };
    let mut queue = Queue::default();
    if off {
        if !entries.is_empty() {
            log(&shared.instance, &format!("restore is off: {} session(s) that ran before the restart stay stopped", entries.len()));
            let job = {
                let mut inner = lock(&shared.restore.inner);
                inner.view = Some(RestoreView {
                    started_ms: started,
                    previous_ms,
                    interrupted: entries.iter().map(|e| RestoreSkip { id: e.id.clone(), why: "restore is off (atelier config restore on)".into() }).collect(),
                    ..Default::default()
                });
                inner.set.entries.clear();
                inner.dirty = true;
                inner.take_save(now_ms(), true)
            };
            shared.restore.write(job);
        }
    } else if !entries.is_empty() {
        // The VM thread adopts VM boxes and the index starts meanwhile.
        std::thread::sleep(Duration::from_millis(500));
        queue = Queue::new(entries, previous_ms, now_ms());
        let order: Vec<&str> = queue.waiting.iter().map(|w| w.entry.id.as_str()).collect();
        log(&shared.instance, &format!("restore: resuming {} session(s), {RESTORE_AT_ONCE} at a time, the most recently active first: {}", order.len(), order.join(", ")));
        let mut inner = lock(&shared.restore.inner);
        // Every box shows "resuming after restart" until its turn comes.
        inner.view = Some(RestoreView { started_ms: started, previous_ms, resuming: order.iter().map(|id| (*id).to_owned()).collect(), ..Default::default() });
        // Exits kept from just before the restart (a logoff ends programs first) wait their turn too.
        for entry in inner.set.entries.values_mut() {
            entry.exited_ms = 0;
        }
    }
    let mut kept_at = 0;
    let (mut resumed, mut not_resumed) = (0, 0);
    loop {
        std::thread::sleep(Duration::from_millis(if queue.is_done() { 1_000 } else { 250 }));
        if shared.shutting_down.load(Ordering::Relaxed) {
            shared.restore.shutdown(now_ms());
            shared.restore.save_now();
            return;
        }
        if !queue.is_done() {
            let now = now_ms();
            let (began, shown) = (queue.began, shared.visible());
            for (id, outcome) in queue.step(now, &mut |entry| restore_one(&shared, entry, began), &|id, launched| still_starting(&shared, id, launched, now), &shown) {
                match outcome {
                    Outcome::Resumed => resumed += 1,
                    Outcome::Skipped(_) => not_resumed += 1,
                    _ => {}
                }
                shared.restore.turn_came(&id, outcome);
            }
            if queue.is_done() {
                log(&shared.instance, &format!("restore: {resumed} resumed, {not_resumed} not resumed"));
            }
        }
        if now_ms().saturating_sub(kept_at) >= 1_000 {
            kept_at = now_ms();
            keep(&shared);
            settle(&shared);
        }
    }
}

/// Whether a resumed terminal is still starting: it runs, and has not yet
/// printed and then been quiet for `STARTING_QUIET_MS`; a Copilot session
/// also until its history says it has loaded (a `session.resume` or
/// `session.start` after the launch: it prints its banner, then loads a
/// large history for seconds without a word, R2-C-11). At most `STARTING_MAX_MS`.
fn still_starting(shared: &Shared, id: &str, launched: u64, now: u64) -> bool {
    if now.saturating_sub(launched) >= STARTING_MAX_MS {
        return false;
    }
    let Some(term) = lock(&shared.terms).get(id).cloned() else { return false };
    if !term.alive.load(Ordering::Acquire) {
        return false;
    }
    let output = term.last_output_ms.load(Ordering::Relaxed);
    let copilot = lock(&shared.agent_terms).get(id) == Some(&AgentKind::Copilot);
    let history = || {
        let native = shared.native_of(id);
        lock(&shared.sessions).get(&native).map(|s| s.last_event_ms)
    };
    starting_by(output, now, copilot.then(history).flatten(), term.started_ms)
}

/// [`still_starting`]'s rule: no output yet or output within the quiet
/// time, or (a Copilot session's history, `last_event`) nothing written to
/// its history since it started (`started`).
fn starting_by(output: u64, now: u64, last_event: Option<u64>, started: u64) -> bool {
    output == 0 || now.saturating_sub(output) < STARTING_QUIET_MS || last_event.is_some_and(|at| at < started)
}

impl Restore {
    /// An entry's turn came in the restore: the view says what happened (a
    /// resumed one stays "resuming" until it settles), and one not resumed
    /// leaves the running set.
    fn turn_came(&self, id: &str, outcome: Outcome) {
        let mut inner = lock(&self.inner);
        if let Outcome::Skipped(_) = &outcome {
            inner.set.entries.remove(id);
            inner.dirty = true;
        }
        let Some(view) = inner.view.as_mut() else { return };
        if matches!(outcome, Outcome::Resumed) {
            return;
        }
        view.resuming.retain(|r| r != id);
        match outcome {
            Outcome::Running => view.resumed.push(id.to_owned()),
            Outcome::Skipped(why) => view.interrupted.push(RestoreSkip { id: id.to_owned(), why }),
            _ => {}
        }
    }

    /// Whether `entry` is still recorded as the restore found it (not
    /// stopped, nor launched again by you, meanwhile).
    fn still_recorded(&self, entry: &Entry) -> bool {
        lock(&self.inner).set.entries.get(&entry.id).is_some_and(|e| e.started_ms == entry.started_ms)
    }
}

/// Starts one recorded terminal again through the normal launch path
/// (`began`: when the restore began).
fn restore_one(shared: &Shared, entry: &Entry, began: u64) -> Outcome {
    let id = &entry.id;
    // You started it first (or it is being started now).
    if shared.running_or_launching(id) {
        // Started by a client since the restore began ("Update service"
        // resumes every terminal it had at once): a busy one still gets its
        // note once it has settled (R2-C-06).
        let started = lock(&shared.terms).get(id).filter(|t| t.alive.load(Ordering::Acquire)).map(|t| t.started_ms);
        let agent = agent_of(&entry.launch).is_some();
        if agent && !entry.doing.idle() && started.is_none_or(|at| at >= began) {
            log(&shared.instance, &format!("restore: {id} was started by a client meanwhile (it was {}); a note follows once it has settled", describe(&entry.doing)));
            lock(&shared.restore.inner).pending.insert(id.clone(), Pending { doing: entry.doing.clone(), launched_ms: started.unwrap_or_else(now_ms) });
            return Outcome::Resumed;
        }
        log(&shared.instance, &format!("restore: {id} already runs"));
        return Outcome::Running;
    }
    if !shared.restore.still_recorded(entry) {
        log(&shared.instance, &format!("restore: {id} was stopped or started again meanwhile; not resumed"));
        return Outcome::Gone;
    }
    let agent = agent_of(&entry.launch);
    if let Some(kind) = agent {
        let native = if entry.native.is_empty() { id.as_str() } else { entry.native.as_str() };
        if kind.provider().is_live(native) {
            let holder = shared.session_holder(kind, native, Some(id));
            log(&shared.instance, &format!("restore: {id} not resumed yet: its session {native} is {holder}"));
            return Outcome::Live(holder);
        }
    }
    if shared.docs.placement_of(id).is_some_and(|p| p.is_vm()) {
        return Outcome::Skipped("a VM box (its VM keeps it)".into());
    }
    let Some(launch) = relaunch(&entry.launch, Some(&entry.native)) else { return Outcome::Skipped("not restorable".into()) };
    shared.restore.launching(id);
    let launched = shared.launch(id, launch, &entry.cwd, entry.cols.clamp(20, 1000), entry.rows.clamp(5, 500));
    // Stopped (or removed) while it was being launched: what started is stopped, with no note (R3-B-04).
    if shared.restore.launched(id) {
        log(&shared.instance, &format!("restore: {id} was stopped while it was being resumed; stopping what started"));
        if launched.is_ok() {
            let _ = shared.host.stop_terminal(id, false);
        }
        return Outcome::Gone;
    }
    match launched {
        Ok(()) => {
            let note = agent.is_some() && !entry.doing.idle();
            let what = match agent {
                Some(kind) if note => format!("resumed {} (it was {}); a note follows once it has settled", kind.id(), describe(&entry.doing)),
                Some(kind) => format!("resumed {} without a note (it was idle)", kind.id()),
                None => "started its command again".to_owned(),
            };
            log(&shared.instance, &format!("restore: {id} {what}"));
            lock(&shared.restore.inner).pending.insert(id.clone(), Pending { doing: if agent.is_some() { entry.doing.clone() } else { Doing::default() }, launched_ms: now_ms() });
            Outcome::Resumed
        }
        Err(error) => {
            log(&shared.instance, &format!("restore: {id} not resumed: {error:#}"));
            Outcome::Skipped(format!("{error:#}"))
        }
    }
}

fn describe(doing: &Doing) -> String {
    let mut parts = Vec::new();
    if doing.working {
        parts.push("working".to_owned());
    }
    if doing.waiting {
        parts.push("waiting on you".to_owned());
    }
    if doing.loop_ms > 0 {
        parts.push(format!("looping every {}", interval(doing.loop_ms)));
    }
    if parts.is_empty() { "idle".into() } else { parts.join(", ") }
}

/// Keeps the running set from the terminals the service runs (only their
/// sessions' summaries are copied, not the whole index).
fn keep(shared: &Shared) {
    let terms: Vec<TermFacts> = lock(&shared.terms)
        .values()
        .map(|t| {
            let (cols, rows) = t.size();
            let info = t.info();
            TermFacts {
                id: t.id.clone(),
                alive: t.alive.load(Ordering::Acquire),
                agent: t.copilot,
                remote: t.is_remote(),
                started_ms: t.started_ms,
                cols,
                rows,
                progress_busy: info.progress_seen && matches!(info.progress, 1 | 3),
            }
        })
        .collect();
    let natives = lock(&shared.agent_sessions).clone();
    let sessions: HashMap<String, SessionSummary> = {
        let all = lock(&shared.sessions);
        terms.iter().filter(|t| t.alive && t.agent).filter_map(|t| {
            let native = natives.get(&t.id).cloned().unwrap_or_else(|| t.id.clone());
            all.get(&native).map(|s| (native, s.clone()))
        }).collect()
    };
    let command = |id: &str| shared.docs.command_of(id).map(|(command, cwd)| (LaunchKind::Command { program: command[0].clone(), args: command[1..].to_vec() }, cwd));
    let doing = |native: &str, progress: bool| Doing::of(sessions.get(native), progress);
    shared.restore.sync(&terms, &natives, &command, &doing, now_ms());
}

/// Notes for resumed agents that have settled go to the front of the
/// mailbox (the bus types them in when the session is idle).
fn settle(shared: &Shared) {
    let pending: Vec<(String, Pending)> = lock(&shared.restore.inner).pending.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if pending.is_empty() {
        return;
    }
    let now = now_ms();
    let restarted_at = {
        let local = canvas_model::bus::Local::now();
        format!("{:02}:{:02} ({})", local.hour, local.minute, local.date())
    };
    for (id, p) in pending {
        let term = lock(&shared.terms).get(&id).cloned();
        let (alive, started, output) = match &term {
            Some(t) => (t.alive.load(Ordering::Acquire), t.started_ms, t.last_output_ms.load(Ordering::Relaxed)),
            None => (false, p.launched_ms, 0),
        };
        if !alive {
            if now.saturating_sub(p.launched_ms) > 3_000 {
                log(&shared.instance, &format!("restore: {id} exited right after it was resumed"));
                finish(shared, &id, Some("it exited right after it was resumed"));
            }
            continue;
        }
        let ran = now.saturating_sub(started);
        if !(ran >= SETTLE_MAX_MS || (ran >= SETTLE_MS && now.saturating_sub(output) >= QUIET_MS)) {
            continue;
        }
        if let Some(text) = note(&p.doing, &restarted_at) {
            let mail = Mail { id: uuid::Uuid::new_v4().to_string(), to: id.clone(), from: String::new(), text, ms: now };
            lock(&shared.restore.inner).notes.push(mail);
            log(&shared.instance, &format!("restore: {id} has settled; its note is queued"));
        }
        finish(shared, &id, None);
    }
}

/// `id` is no longer being resumed: resumed, or (`failed`) not after all.
fn finish(shared: &Shared, id: &str, failed: Option<&str>) {
    let mut inner = lock(&shared.restore.inner);
    inner.pending.remove(id);
    if let Some(view) = inner.view.as_mut() {
        view.resuming.retain(|r| r != id);
        match failed {
            Some(why) => view.interrupted.push(RestoreSkip { id: id.to_owned(), why: why.to_owned() }),
            None => view.resumed.push(id.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, prompt: Option<&str>) -> LaunchKind {
        LaunchKind::Agent(Box::new(AgentLaunch { agent: "copilot".into(), native_id: None, resume: false, prompt: prompt.map(str::to_owned), name: Some(id.into()), ..Default::default() }))
    }

    fn term(id: &str, alive: bool, started_ms: u64) -> TermFacts {
        TermFacts { id: id.into(), alive, agent: true, remote: false, started_ms, cols: 120, rows: 40, progress_busy: false }
    }

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sync(r: &Restore, terms: &[TermFacts], now: u64) {
        let doing = |_: &str, progress: bool| Doing { working: progress, ..Default::default() };
        r.sync(terms, &HashMap::new(), &|_| None, &doing, now);
    }

    #[test]
    fn a_launch_is_recorded_as_a_resume_once_its_terminal_runs() {
        let home = home();
        let r = Restore::new(&home, "default");
        r.requested("a", &agent("a", Some("first prompt")), "/w");
        assert!(r.entries().is_empty(), "nothing runs yet");
        sync(&r, &[term("a", true, now_ms())], now_ms());
        let entries = r.entries();
        assert_eq!(entries.len(), 1);
        let LaunchKind::Agent(launch) = &entries[0].launch else { panic!("an agent launch") };
        assert!(launch.resume && launch.prompt.is_none(), "resumes, never sends its first prompt again");
        assert_eq!((entries[0].cwd.as_str(), entries[0].cols, entries[0].rows), ("/w", 120, 40));
        // Written to disk, and read back by the next service.
        let again = load(&path(&home, "default"));
        assert_eq!(again.entries.len(), 1);
        // Passive runs and VM launches are never recorded.
        let headless = LaunchKind::Agent(Box::new(AgentLaunch { agent: "copilot".into(), headless_prompt: Some("run".into()), ..Default::default() }));
        r.requested("p", &headless, "/w");
        let vm = LaunchKind::Agent(Box::new(AgentLaunch { agent: "vm:copilot".into(), ..Default::default() }));
        r.requested("v", &vm, "/w");
        sync(&r, &[term("a", true, 1), term("p", true, now_ms()), term("v", true, now_ms())], now_ms());
        assert_eq!(r.entries().len(), 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn entries_stay_when_the_service_goes_away_and_go_when_you_stop_them() {
        let home = home();
        let r = Restore::new(&home, "default");
        let now = now_ms();
        for id in ["a", "b", "c", "d"] {
            r.requested(id, &agent(id, None), "/w");
        }
        let terms: Vec<TermFacts> = ["a", "b", "c", "d"].iter().map(|id| term(id, true, now)).collect();
        sync(&r, &terms, now);
        assert_eq!(r.entries().len(), 4);
        // You stop a: a graceful stop asks first.
        r.stopping("a", false);
        assert_eq!(r.exited("a", false, false, now), "removed (stopped by you)");
        // A forced stop (the quit dialog's, a removal) outside a shutdown.
        assert_eq!(r.exited("b", true, false, now), "removed (stopped by you)");
        // c's program quits on its own: gone after the grace.
        r.exited("c", false, false, now);
        let dead = |id: &str| term(id, false, now);
        sync(&r, &[dead("c"), term("d", true, now)], now + 1_000);
        assert!(r.entries().iter().any(|e| e.id == "c"), "kept for a moment (the machine may be going down)");
        sync(&r, &[dead("c"), term("d", true, now)], now + EXIT_GRACE_MS + 1);
        assert_eq!(r.entries().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["d"]);
        // The service stops: d is killed with it and kept.
        r.stopping("d", true);
        assert_eq!(r.exited("d", true, true, now), "kept (the service is going away)");
        r.shutdown(now + EXIT_GRACE_MS + 2);
        let next = load(&path(&home, "default"));
        assert_eq!(next.entries.keys().collect::<Vec<_>>(), vec!["d"], "the next service resumes d only");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn exits_just_before_a_shutdown_are_kept_older_ones_are_final() {
        let home = home();
        let r = Restore::new(&home, "default");
        let now = now_ms();
        for id in ["early", "late"] {
            r.requested(id, &agent(id, None), "/w");
        }
        sync(&r, &[term("early", true, now), term("late", true, now)], now);
        r.exited("early", false, false, now);
        r.exited("late", false, false, now + 5_000);
        r.shutdown(now + 5_500);
        let next = load(&path(&home, "default"));
        assert_eq!(next.entries.keys().collect::<Vec<_>>(), vec!["late"], "a logoff ends programs first: that exit came with it");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_note_says_what_the_session_was_doing_and_idle_ones_get_none() {
        assert_eq!(note(&Doing::default(), "10:52"), None, "idle, no loop: resumed silently");
        let working = note(&Doing { working: true, ..Default::default() }, "10:52 (2026-11-12)").unwrap();
        assert!(working.starts_with("[Atelier] The machine running this session restarted at 10:52 (2026-11-12) and interrupted you. Before the restart you were working on a task."), "{working}");
        assert!(working.contains("Restore the state you were in"));
        let looping = note(&Doing { loop_ms: 900_000, ..Default::default() }, "t").unwrap();
        assert!(looping.contains("you were running a loop every 15m") && looping.contains("same interval (15m)"), "{looping}");
        let waiting = note(&Doing { working: true, waiting: true, loop_ms: 3_600_000, ..Default::default() }, "t").unwrap();
        assert!(waiting.contains("working on a task, running a loop every 1h and waiting for an answer"), "{waiting}");
        assert!(waiting.contains("ask it again"));
        assert_eq!((interval(45_000), interval(5_400_000), interval(172_800_000)), ("45s".into(), "1h30m".into(), "2d".into()));
        // A message from Atelier itself is typed as written.
        let mail = Mail { from: String::new(), text: working.clone(), ..Default::default() };
        assert_eq!(canvas_model::bus::mail_text(&mail), working);
    }

    #[test]
    fn a_learned_session_id_is_resumed_and_a_resume_that_dies_is_reported() {
        let home = home();
        let r = Restore::new(&home, "default");
        let now = now_ms();
        let oc = LaunchKind::Agent(Box::new(AgentLaunch { agent: "opencode".into(), ..Default::default() }));
        r.requested("box", &oc, "/w");
        let natives = HashMap::from([("box".to_owned(), "ses_abc".to_owned())]);
        let doing = |native: &str, _: bool| Doing { working: native == "ses_abc", ..Default::default() };
        r.sync(&[term("box", true, now)], &natives, &|_| None, &doing, now);
        let entry = &r.entries()[0];
        assert_eq!(entry.native, "ses_abc");
        assert!(entry.doing.working, "what its own session does");
        let LaunchKind::Agent(again) = relaunch(&entry.launch, Some(&entry.native)).unwrap() else { panic!() };
        assert_eq!((again.native_id.as_deref(), again.resume), (Some("ses_abc"), true), "OpenCode resumes the session it learned");
        // A resume after a restart that exits before it settles leaves "resuming".
        {
            let mut inner = lock(&r.inner);
            inner.pending.insert("box".into(), Pending { doing: Doing::default(), launched_ms: now });
            inner.view = Some(RestoreView { resuming: vec!["box".into()], ..Default::default() });
        }
        r.exited("box", false, false, now);
        let view = r.view().unwrap();
        assert!(view.resuming.is_empty() && view.interrupted[0].id == "box" && !r.holds("box"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn restore_is_off_when_the_setting_says_so() {
        let home = home();
        canvas_common::settings::Settings { restore: Some(false), ..Default::default() }.save(&home).unwrap();
        let r = Restore::new(&home, "default");
        r.requested("a", &agent("a", None), "/w");
        sync(&r, &[term("a", true, now_ms())], now_ms());
        assert!(r.entries().is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_slow_disk_never_holds_up_launches_exits_or_the_bus() {
        let home = home();
        // A write that takes 3 s (a flush on a busy disk took up to 59 s on the owner's host).
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let saver: Saver = Box::new(move |_: &Path, _: &RunningSet| {
            let _ = entered_tx.send(());
            std::thread::sleep(Duration::from_secs(3));
        });
        let r = Arc::new(Restore::with_saver(&home, "default", saver));
        r.requested("a", &agent("a", None), "/w");
        let writer = std::thread::spawn({
            let r = r.clone();
            move || sync(&r, &[term("a", true, now_ms())], now_ms())
        });
        entered.recv_timeout(Duration::from_secs(5)).expect("the write began");
        // Meanwhile: a launch, an exit, the bus's questions all answer at once.
        let started = std::time::Instant::now();
        r.requested("b", &agent("b", None), "/w");
        r.exited("c", false, false, now_ms());
        let _ = (r.view(), r.holds("a"), r.take_notes(), r.entries());
        assert!(started.elapsed() < Duration::from_millis(500), "waited {:?} for the disk", started.elapsed());
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn what_sessions_do_is_written_at_most_every_few_seconds_launches_at_once() {
        let home = home();
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let saver: Saver = Box::new({
            let writes = writes.clone();
            move |_: &Path, _: &RunningSet| {
                writes.fetch_add(1, Ordering::SeqCst);
            }
        });
        let r = Restore::with_saver(&home, "default", saver);
        let now = now_ms();
        r.requested("a", &agent("a", None), "/w");
        let terms = [term("a", true, now)];
        sync(&r, &terms, now);
        assert_eq!(writes.load(Ordering::SeqCst), 1, "a launch is written at once");
        // Its turn starts and ends every second: written once the gap has passed.
        let doing = |working: bool| move |_: &str, _: bool| Doing { working, ..Default::default() };
        for (i, working) in [true, false, true, false].into_iter().enumerate() {
            r.sync(&terms, &HashMap::new(), &|_| None, &doing(working), now + 1_000 * (i as u64 + 1));
        }
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        r.sync(&terms, &HashMap::new(), &|_| None, &doing(true), now + SAVE_GAP_MS + 1);
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        assert!(r.entries()[0].doing.working && r.entries()[0].active_ms == now + SAVE_GAP_MS + 1);
        // The service going away writes what is pending at once.
        r.sync(&terms, &HashMap::new(), &|_| None, &doing(false), now + SAVE_GAP_MS + 2);
        r.save_now();
        assert_eq!(writes.load(Ordering::SeqCst), 3);
        let _ = std::fs::remove_dir_all(home);
    }

    fn entry(id: &str, working: bool, active_ms: u64) -> Entry {
        Entry { id: id.into(), launch: agent(id, None), cwd: "/w".into(), cols: 100, rows: 30, started_ms: 1_000, native: String::new(), doing: Doing { working, ..Default::default() }, exited_ms: 0, active_ms }
    }

    #[test]
    fn the_restore_starts_a_few_at_a_time_the_most_recently_active_first() {
        let previous = 900_000;
        let entries = vec![entry("old", false, 100_000), entry("busy", true, 50_000), entry("recent", false, 800_000), entry("mid", false, 400_000), entry("older", false, 200_000), entry("busy2", true, 10_000)];
        let mut queue = Queue::new(entries, previous, 1_000_000);
        let order: Vec<&str> = queue.waiting.iter().map(|w| w.entry.id.as_str()).collect();
        assert_eq!(order, ["busy", "busy2", "recent", "mid", "older", "old"], "busy ones were busy until the restart");
        let mut launched: Vec<String> = Vec::new();
        let starting = std::cell::RefCell::new(HashSet::<String>::new());
        let mut step = |queue: &mut Queue, now: u64| {
            let mut start = |e: &Entry| {
                launched.push(e.id.clone());
                starting.borrow_mut().insert(e.id.clone());
                Outcome::Resumed
            };
            queue.step(now, &mut start, &|id, _| starting.borrow().contains(id), &HashSet::new()).len()
        };
        let t = 1_000_000;
        assert_eq!(step(&mut queue, t), 1, "one at once");
        assert_eq!(step(&mut queue, t + 1_000), 0, "a short gap between two");
        assert_eq!(step(&mut queue, t + RESTORE_GAP_MS), 1);
        assert_eq!(step(&mut queue, t + 3 * RESTORE_GAP_MS), 0, "at most {RESTORE_AT_ONCE} starting");
        // The first one printed and went quiet: the next one starts.
        starting.borrow_mut().remove("busy");
        assert_eq!(step(&mut queue, t + 4 * RESTORE_GAP_MS), 1);
        starting.borrow_mut().clear();
        for i in 5..12 {
            step(&mut queue, t + i * RESTORE_GAP_MS);
            starting.borrow_mut().clear();
        }
        assert_eq!(launched, ["busy", "busy2", "recent", "mid", "older", "old"]);
        assert!(queue.is_done());
    }

    #[test]
    fn a_box_a_client_shows_goes_first_and_one_removed_before_its_turn_is_not_resumed() {
        let t = 1_000_000;
        let mut queue = Queue::new(vec![entry("a", true, 3), entry("b", false, 2), entry("c", false, 1)], t, t);
        let mut launched = Vec::new();
        let mut start = |e: &Entry| {
            launched.push(e.id.clone());
            Outcome::Resumed
        };
        let shown: HashSet<String> = ["c".to_owned()].into();
        queue.step(t, &mut start, &|_, _| false, &shown);
        assert_eq!(launched, ["c"], "shown in a window: it goes first (it would start anyway)");
        // A box removed (or stopped) while it waits: cancelled, its turn finds it gone (R2-C-01).
        let home = home();
        let r = Restore::new(&home, "default");
        lock(&r.inner).view = Some(RestoreView { resuming: vec!["a".into(), "b".into()], ..Default::default() });
        for id in ["a", "b"] {
            lock(&r.inner).set.entries.insert(id.into(), entry(id, false, 1));
        }
        lock(&r.inner).pending.insert("a".into(), Pending { doing: Doing::default(), launched_ms: t });
        assert!(!r.cancel("a", "removed from the canvas before its turn"), "resumed already (settling): it runs, Stop ends it");
        assert!(r.cancel("b", "removed from the canvas before its turn"));
        assert!(!r.still_recorded(&entry("b", false, 1)), "its turn: gone, never launched");
        assert_eq!(r.entries().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a"], "not kept for the next restart");
        let view = r.view().unwrap();
        assert_eq!((view.resuming, view.interrupted.iter().map(|s| (s.id.as_str(), s.why.as_str())).collect::<Vec<_>>()), (vec!["a".to_owned()], vec![("b", "removed from the canvas before its turn")]));
        assert!(!r.cancel("b", "again"), "once");
        let _ = std::fs::remove_dir_all(home);
    }

    /// R3-B-04: a box stopped while the restore itself launches it is
    /// stopped once the launch is done (no note, not recorded again).
    #[test]
    fn a_box_stopped_while_the_restore_launches_it_is_stopped_after_the_launch() {
        let home = home();
        let r = Restore::new(&home, "default");
        lock(&r.inner).view = Some(RestoreView { resuming: vec!["a".into(), "b".into()], ..Default::default() });
        for id in ["a", "b"] {
            lock(&r.inner).set.entries.insert(id.into(), entry(id, true, 1));
        }
        // Its launch is under way (resolving, spawning) when the Stop comes.
        r.launching("a");
        r.requested("a", &LaunchKind::Copilot { new_name: None, prompt: None, resume: true }, "/w");
        assert!(r.cancel("a", "stopped before its turn"));
        assert!(r.launched("a"), "the launch finds it cancelled: what started is stopped");
        assert!(!lock(&r.inner).requests.contains_key("a"), "not recorded again from its launch request");
        assert!(!r.holds("a"), "no note for it");
        assert!(r.entries().iter().all(|e| e.id != "a"));
        // A launch nobody cancelled is kept.
        r.launching("b");
        assert!(!r.launched("b"));
        assert!(r.entries().iter().any(|e| e.id == "b"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_resumed_copilot_counts_as_starting_until_its_history_says_it_loaded() {
        let (started, now) = (10_000, 20_000);
        // Printed its banner, quiet for 5 s: done starting by output alone.
        assert!(!starting_by(15_000, now, None, started));
        // Copilot's history has nothing since the launch (it is loading a large session): still starting (R2-C-11).
        assert!(starting_by(15_000, now, Some(9_000), started));
        // `session.resume` written: loaded.
        assert!(!starting_by(15_000, now, Some(12_000), started));
        assert!(starting_by(0, now, Some(12_000), started), "nothing printed yet");
        assert!(starting_by(19_000, now, Some(12_000), started), "printing");
    }

    #[test]
    fn a_session_still_live_at_start_is_tried_again_for_a_while_then_named() {
        let t = 1_000_000;
        let mut queue = Queue::new(vec![entry("a", true, 1), entry("b", false, 1)], t, t);
        // The previous service's CLI of `a` is still closing for a few seconds.
        let mut tries = 0;
        let mut start = |e: &Entry| {
            if e.id == "a" {
                tries += 1;
                if tries < 3 {
                    return Outcome::Live("already running in another terminal (copilot pid 7, not started by Atelier): use that terminal".into());
                }
            }
            Outcome::Resumed
        };
        let none = |_: &str, _: u64| false;
        let mut done = Vec::new();
        for i in 0..20 {
            done.extend(queue.step(t + i * 1_000, &mut start, &none, &HashSet::new()));
        }
        assert_eq!(done, [("b".to_owned(), Outcome::Resumed), ("a".to_owned(), Outcome::Resumed)], "b goes meanwhile, a once its old CLI is gone");
        // One that stays live: given up after a while, with who has it.
        let mut queue = Queue::new(vec![entry("c", true, 1)], t, t);
        let mut live = |_: &Entry| Outcome::Live("already running in VS Code (Code.exe pid 9): close it there first".into());
        let mut done = Vec::new();
        for i in 0..40 {
            done.extend(queue.step(t + i * 1_000, &mut live, &none, &HashSet::new()));
        }
        assert_eq!(done, [("c".to_owned(), Outcome::Skipped("its session is already running in VS Code (Code.exe pid 9): close it there first".into()))]);
        assert!(queue.is_done());
    }

    #[test]
    fn the_view_follows_each_entrys_turn() {
        let home = home();
        let r = Restore::new(&home, "default");
        lock(&r.inner).view = Some(RestoreView { resuming: vec!["a".into(), "b".into(), "c".into()], ..Default::default() });
        lock(&r.inner).set.entries.insert("c".into(), entry("c", false, 1));
        r.turn_came("a", Outcome::Resumed);
        r.turn_came("b", Outcome::Running);
        r.turn_came("c", Outcome::Skipped("its session is already running in VS Code".into()));
        let view = r.view().unwrap();
        assert_eq!(view.resuming, ["a"], "resuming until it settles");
        assert_eq!(view.resumed, ["b"]);
        assert_eq!(view.skipped("c"), Some("its session is already running in VS Code"));
        assert!(r.entries().is_empty(), "not resumed: no longer recorded");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_stop_of_a_run_that_was_relaunched_is_spent() {
        let home = home();
        let r = Restore::new(&home, "default");
        let now = now_ms();
        r.requested("a", &agent("a", None), "/w");
        sync(&r, &[term("a", true, now)], now);
        // You stop it and start it again before its exit is handled.
        r.stopping("a", false);
        r.relaunched("a");
        assert_eq!(r.exited("a", false, false, now), "exited on its own (removed shortly unless the service goes away)", "the next exit is its own");
        let _ = std::fs::remove_dir_all(home);
    }
}
