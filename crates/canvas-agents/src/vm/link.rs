//! The hub's side of the satellites' links (plan VM-24, VM-25): one live
//! link per sandbox, and each VM box's terminal on the hub ([`Remote`],
//! a `canvas_host::term::RemotePty`) whose program runs in the guest.
//!
//! - A satellite is served only after its credential (`sat-…`) proved it
//!   is the one minted for a sandbox; its first message must be `Hello`
//!   naming that sandbox, then only `Sat` messages and heartbeats are
//!   accepted (anything else closes the link), and a terminal it names must
//!   be one of its own sandbox's (else the link is closed). A newer link of
//!   the same sandbox replaces the older one.
//! - Output is taken in order by offset: a repeat is trimmed, a gap asks
//!   for a `Resync` from what the hub has, a screen (`Snapshot`) replaces
//!   the terminal's engine (logged; nothing of the hub's is drawn into a
//!   program's screen once it runs, and its first output starts on a blank one).
//! - Input is numbered per generation and kept until acknowledged; it is
//!   sent again after a reconnect (the satellite applies it once). Typing
//!   while the link is down is refused ("VM offline").
//! - A box's terminal is made at its launch, before its VM exists: it shows
//!   the launcher's steps as note lines, and its program is started
//!   (`Spawn`) once the launch runs and the sandbox's satellite is linked.
//!   Its generation is recorded in the intent, so a restarted service
//!   adopts the terminal again ([`Links::adopt`]) and the satellite replays it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::protocol::{ClientMsg, ServiceMsg};
use canvas_common::sat::{ExitInfo, HelloInfo, ProcReport, SAT_IO_CHUNK, SAT_PROTOCOL, SAT_PROTOCOL_MIN, SatDown, SatUp, StatsReport, TermHave, WelcomeInfo};
use serde::{Deserialize, Serialize};
use canvas_common::wire::{Closer, FrameReader, FrameWriter};
use canvas_host::server::SatelliteLink;
use canvas_host::term::{RemoteFeed, RemotePty};
use canvas_model::vm::{OpAnswer, VmState};
use crossbeam_channel::{Receiver, Sender, bounded, select_biased};

use super::launch;
use super::satellite::SILENCE;
use crate::paths::now_ms;

/// Input kept until the satellite acknowledges it.
const UNACKED: usize = 1 << 20;
/// Clears the screen and its scrollback and homes the cursor.
const CLEAR: &[u8] = b"\x1b[0m\x1b[H\x1b[2J\x1b[3J";
/// Typing while a VM resumes is kept (at most this much, this long) and sent
/// once its satellite links again (D9); otherwise typing offline is refused.
const RESUME_BUFFER: usize = 4 << 10;
const RESUME_WAIT_MS: u64 = 30_000;
const HEARTBEAT: Duration = Duration::from_secs(2);
/// A link whose writer cannot take a message within this long is closed.
const SEND: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the service tells about VM links (its log).
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

pub struct Links {
    home: PathBuf,
    log: Log,
    links: Mutex<HashMap<String, Arc<Link>>>,
    remotes: Mutex<HashMap<String, Arc<Remote>>>,
    epochs: AtomicU64,
    /// Controls sent and waiting for their answer: id → (sandbox, where it goes).
    controls: Mutex<HashMap<u64, (String, Sender<String>)>>,
    control_ids: AtomicU64,
    /// Stops asked for while a sandbox's link was down: (sandbox, terminal,
    /// generation), sent when it links again (whatever was launched meanwhile).
    stops: Mutex<Vec<(String, String, u64)>>,
    /// The MCP gateway: guest agents' sessions with the hub's MCP servers.
    mcp: Arc<super::mcp::Gateway>,
    /// Told of each session found inside a box's terminal in a guest:
    /// (the box, the session, its name, its folder).
    subsession: Mutex<Option<SubsessionHook>>,
    /// The sessions told of already.
    reported: Mutex<std::collections::HashSet<String>>,
    /// Per sandbox, the Copilot sessions alive in its guest (its last process report).
    live: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    /// Relayed commands running: (sandbox, operation id); and per sandbox how many run now.
    ops: Mutex<RunningOps>,
    ops_turn: std::sync::Condvar,
    /// What runs a relayed command (this program).
    exe: Mutex<PathBuf>,
    me: Weak<Links>,
    /// Per sandbox, its writable roots (from the last sync): where the guest's edits go.
    writes: Mutex<HashMap<String, HashMap<String, super::sync::Writes>>>,
    /// Per sandbox, the link whose roots were synced in full once (its epoch).
    synced: Mutex<HashMap<String, u64>>,
    /// File revisions by (path, size, modified).
    revs: Mutex<HashMap<PathBuf, (u64, std::time::SystemTime, String)>>,
}

/// What a terminal's command line names the guest's roots folder by (the hub
/// puts the satellite's in its place when it starts the program).
pub const ROOTS: &str = "%ROOTS%";
/// File bytes one sync pass sends at most (the rest comes with the next).
const SYNC_BUDGET: u64 = 32 << 20;

/// Relayed commands taken: (sandbox, operation id); and per sandbox how many run now.
type RunningOps = (std::collections::HashSet<(String, String)>, HashMap<String, usize>);

/// Relayed commands a sandbox may have running at once, and waiting.
const OPS_RUNNING: usize = 4;
const OPS_WAITING: usize = 64;

pub type SubsessionHook = Arc<dyn Fn(&str, &str, &str, &str) + Send + Sync>;

/// The gateway's answers go out on the sandbox's link.
struct McpDown(std::sync::Weak<Links>);

impl super::mcp::Down for McpDown {
    fn send(&self, sandbox: &str, stream: u64, kind: u8, bytes: Vec<u8>) -> bool {
        let Some(link) = self.0.upgrade().and_then(|links| links.link(sandbox)) else { return false };
        if kind == canvas_common::sat::MCP_CLOSE {
            lock(&link.streams).close(stream);
        }
        link.send(false, SatDown::Mcp { stream, kind, bytes })
    }
}

/// A satellite's live link.
struct Link {
    sandbox: String,
    /// Its credential (`sat-…`): revoked, the link is closed.
    satellite: String,
    /// The guest's roots folder (`HelloInfo::roots`).
    roots: String,
    /// The guest user's home and the guest's MCP proxy port (`HelloInfo`).
    guest_home: String,
    mcp_port: u16,
    epoch: u64,
    io: Sender<ServiceMsg>,
    other: Sender<ServiceMsg>,
    closer: Closer,
    /// Its open MCP streams (at most `flood::MCP_STREAMS`).
    streams: Mutex<super::flood::Streams>,
}

/// What the hub last knew of a sandbox's satellite (`<home>/vm/links/<sandbox>.json`):
/// whether it is linked and since when, and its last stats.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LinkRecord {
    pub linked: bool,
    pub since_ms: u64,
    pub stats_ms: u64,
    pub stats: StatsReport,
}

fn record_path(home: &Path, sandbox: &str) -> Option<PathBuf> {
    let plain = !sandbox.is_empty() && sandbox.len() <= 128 && sandbox.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    plain.then(|| home.join("vm").join("links").join(format!("{sandbox}.json")))
}

/// A sandbox's link record (`None`: its satellite never linked to this hub).
pub fn load_record(home: &Path, sandbox: &str) -> Option<LinkRecord> {
    std::fs::read(record_path(home, sandbox)?).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn update_record(home: &Path, sandbox: &str, change: impl FnOnce(&mut LinkRecord)) {
    let Some(path) = record_path(home, sandbox) else { return };
    let mut record = load_record(home, sandbox).unwrap_or_default();
    change(&mut record);
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = crate::paths::write_atomic(&path, &bytes);
    }
}

/// How often satellites send their stats (`ATELIER_VM_STATS_SECS`, else 30 s).
fn stats_secs() -> u64 {
    canvas_common::env::var("ATELIER_VM_STATS_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(30, |s| s.clamp(2, 3600))
}

impl Link {
    /// `io`: terminal I/O and what orders it (`Welcome`, `Spawn`, `Resync`,
    /// input, sizes, stops) on the lane that goes first, in order; controls on the other.
    fn send(&self, io: bool, message: SatDown) -> bool {
        let lane = if io { &self.io } else { &self.other };
        if lane.send_timeout(ServiceMsg::Sat(Box::new(message)), SEND).is_err() {
            self.closer.close();
            return false;
        }
        true
    }
}

/// A VM box's terminal on the hub.
pub struct Remote {
    pub term: String,
    links: Weak<Links>,
    state: Mutex<RemoteState>,
}

#[derive(Default)]
struct RemoteState {
    feed: Option<RemoteFeed>,
    /// Empty until the launch runs in a sandbox.
    sandbox: String,
    generation: u64,
    /// The program was started in the guest (or adopted: it was before).
    spawned: bool,
    /// Output offset taken so far.
    have: u64,
    /// A `Resync` was asked for: later output waits for it.
    resyncing: bool,
    /// The program was just started: its first output begins on a blank
    /// screen (a guest's ConPTY draws by absolute positions over the
    /// launch's step lines otherwise).
    fresh: bool,
    next_seq: u64,
    unacked: VecDeque<(u64, Vec<u8>)>,
    unacked_bytes: usize,
    cols: u16,
    rows: u16,
    /// The last launch step shown as a note line.
    shown: String,
    exited: bool,
    /// Until when typing is kept for a VM that resumes (ms; 0: not resuming).
    resuming_until: u64,
    /// The first input `seq` typed while the VM resumed, and when (dropped after [`RESUME_WAIT_MS`]).
    buffered: Option<(u64, u64)>,
    buffered_bytes: usize,
}

impl RemoteState {
    fn exit(&mut self, code: u32, error: Option<String>) {
        if !self.exited {
            self.exited = true;
            if let Some(feed) = &self.feed {
                feed.exited(code, error);
            }
        }
    }

    fn note(&mut self, text: &str) {
        if self.shown != text {
            self.shown = text.to_owned();
            if let Some(feed) = &self.feed {
                feed.note(text);
            }
        }
    }
}

impl Links {
    pub fn new(home: &Path, log: Log) -> Arc<Self> {
        Arc::new_cyclic(|links| Links {
            home: home.to_path_buf(),
            log,
            links: Mutex::default(),
            remotes: Mutex::default(),
            epochs: AtomicU64::new(0),
            controls: Mutex::default(),
            control_ids: AtomicU64::new(0),
            stops: Mutex::default(),
            // The hub's MCP servers as this machine's Copilot configuration has them (read at each session's start).
            mcp: super::mcp::Gateway::new(home, Arc::new(super::mcp::CopilotServers::of_this_machine()), Arc::new(McpDown(links.clone()))),
            subsession: Mutex::default(),
            reported: Mutex::default(),
            live: Mutex::default(),
            ops: Mutex::default(),
            ops_turn: std::sync::Condvar::new(),
            exe: Mutex::new(std::env::current_exe().unwrap_or_default()),
            me: links.clone(),
            writes: Mutex::default(),
            synced: Mutex::default(),
            revs: Mutex::default(),
        })
    }

    /// Closes the links whose credential was revoked (their VM was deleted):
    /// their satellites then hear the refusal, proven, and end.
    pub fn close_revoked(&self) {
        let links: Vec<Arc<Link>> = lock(&self.links).values().cloned().collect();
        if links.is_empty() {
            return;
        }
        let store = super::store::Satellites::new(&self.home);
        for link in links {
            if store.sandbox_of(&link.satellite).as_deref() != Some(link.sandbox.as_str()) {
                (self.log)(format!("VM {}: its satellite's credential was revoked; the link is closed", link.sandbox));
                link.closer.close();
            }
        }
    }

    /// The guest's roots folder of `sandbox` (`None`: not linked, or a satellite without roots).
    pub fn guest_roots(&self, sandbox: &str) -> Option<String> {
        self.link(sandbox).map(|l| l.roots.clone()).filter(|r| !r.is_empty())
    }

    /// What the hub knows of `sandbox`'s guest from its satellite (`None`: not linked, or no roots).
    pub fn guest(&self, sandbox: &str) -> Option<super::sync::Guest> {
        let link = self.link(sandbox).filter(|l| !l.roots.is_empty())?;
        Some(super::sync::Guest { roots: link.roots.clone(), home: link.guest_home.clone(), mcp_port: link.mcp_port })
    }

    /// The hub's MCP server processes running for guests now.
    pub fn mcp_running(&self) -> Vec<super::mcp::Running> {
        self.mcp.running()
    }

    /// Whether `sandbox`'s roots were synced in full since its satellite linked.
    pub fn synced(&self, sandbox: &str) -> bool {
        let Some(link) = self.link(sandbox) else { return false };
        lock(&self.synced).get(sandbox) == Some(&link.epoch)
    }

    fn rev(&self, source: &super::sync::Source) -> Option<String> {
        match source {
            super::sync::Source::Bytes(bytes) => Some(canvas_common::sat::rev_of(bytes)),
            super::sync::Source::File(path) => {
                let meta = std::fs::metadata(path).ok()?;
                let modified = meta.modified().ok()?;
                if let Some((len, at, rev)) = lock(&self.revs).get(path)
                    && *len == meta.len()
                    && *at == modified
                {
                    return Some(rev.clone());
                }
                let rev = canvas_common::sat::rev_of(&std::fs::read(path).ok()?);
                lock(&self.revs).insert(path.clone(), (meta.len(), modified, rev.clone()));
                Some(rev)
            }
        }
    }

    /// Brings `sandbox`'s guest to `roots`: asks what it holds, sends each
    /// root's manifest and the files it lacks (at most [`SYNC_BUDGET`] a
    /// pass), ends the roots it no longer has. Its writable roots take the
    /// guest's edits from then on.
    pub fn sync(&self, sandbox: &str, roots: &[super::sync::Root]) -> Result<()> {
        let link = self.link(sandbox).context("the VM's satellite is not linked")?;
        let answer = self.control(sandbox, serde_json::json!({ "roots_state": {} }), Duration::from_secs(15))?;
        let held: canvas_common::sat::RootsState = serde_json::from_value(answer["roots_state"].clone()).unwrap_or_default();
        let mut budget = SYNC_BUDGET;
        let mut complete = true;
        let empty = std::collections::BTreeMap::new();
        for root in roots {
            let revs: std::collections::BTreeMap<String, String> = root.files.iter().filter_map(|(name, source)| self.rev(source).map(|rev| (name.clone(), rev))).collect();
            let manifest = canvas_common::sat::RootManifest { target: root.target.clone(), writable: root.writes.is_some(), files: revs.clone(), folders: root.folders.clone(), gone: false };
            if !link.send(false, SatDown::Manifest { root: root.id.clone(), json: serde_json::to_string(&manifest)? }) {
                bail!("the link to {sandbox} dropped");
            }
            let has = held.roots.get(&root.id).unwrap_or(&empty);
            for (name, rev) in &revs {
                if has.get(name) == Some(rev) {
                    continue;
                }
                let Some(bytes) = root.files.get(name).and_then(|s| s.read()) else { continue };
                if bytes.len() as u64 > budget {
                    complete = false;
                    continue;
                }
                budget -= bytes.len() as u64;
                let rev = canvas_common::sat::rev_of(&bytes);
                let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[]] } else { bytes.chunks(canvas_common::sat::SAT_FILE_CHUNK).collect() };
                let mut offset = 0u64;
                for (i, piece) in pieces.iter().enumerate() {
                    let file = SatDown::File { root: root.id.clone(), name: name.clone(), rev: rev.clone(), offset, last: i + 1 == pieces.len(), bytes: piece.to_vec() };
                    if !link.send(false, file) {
                        bail!("the link to {sandbox} dropped");
                    }
                    offset += piece.len() as u64;
                }
            }
        }
        for id in held.roots.keys().filter(|id| !roots.iter().any(|r| r.id == **id)) {
            let gone = canvas_common::sat::RootManifest { gone: true, ..Default::default() };
            link.send(false, SatDown::Manifest { root: id.clone(), json: serde_json::to_string(&gone)? });
        }
        lock(&self.writes).insert(sandbox.to_owned(), roots.iter().filter_map(|r| r.writes.clone().map(|w| (r.id.clone(), w))).collect());
        if complete {
            lock(&self.synced).insert(sandbox.to_owned(), link.epoch);
        }
        Ok(())
    }

    /// Tests run relayed commands with another program.
    #[cfg(test)]
    fn set_exe(&self, exe: PathBuf) {
        *lock(&self.exe) = exe;
    }

    fn self_ref(&self) -> Option<Arc<Links>> {
        self.me.upgrade()
    }

    /// Tells `hook` of each session found inside a box's terminal in a guest.
    pub fn on_subsession(&self, hook: SubsessionHook) {
        *lock(&self.subsession) = Some(hook);
    }

    /// Whether Copilot session `id` is alive in `sandbox`'s guest (as its
    /// satellite last reported; nothing is alive while it is not linked).
    pub fn session_live(&self, sandbox: &str, id: &str) -> bool {
        lock(&self.live).get(sandbox).is_some_and(|live| live.contains(id))
    }

    fn link(&self, sandbox: &str) -> Option<Arc<Link>> {
        if sandbox.is_empty() {
            return None;
        }
        lock(&self.links).get(sandbox).cloned()
    }

    /// Whether `sandbox`'s satellite is linked now.
    pub fn linked(&self, sandbox: &str) -> bool {
        self.link(sandbox).is_some()
    }

    /// Asks `sandbox`'s satellite (`SatDown::Control`) and waits for its
    /// answer (JSON), at most `wait`.
    pub fn control(&self, sandbox: &str, json: serde_json::Value, wait: Duration) -> Result<serde_json::Value> {
        let link = self.link(sandbox).context("the VM's satellite is not linked")?;
        let id = self.control_ids.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = bounded::<String>(1);
        lock(&self.controls).insert(id, (sandbox.to_owned(), tx));
        let sent = link.send(false, SatDown::Control { id, json: json.to_string() });
        let answer = if sent { rx.recv_timeout(wait).ok() } else { None };
        lock(&self.controls).remove(&id);
        let answer: serde_json::Value = serde_json::from_str(&answer.context("the VM's satellite did not answer")?)?;
        anyhow::ensure!(answer["ok"] == true, "the VM's satellite: {}", answer["error"].as_str().unwrap_or("refused"));
        Ok(answer)
    }

    /// The thin lane's drain of `sandbox` through its satellite's link.
    pub fn drain(&self, sandbox: &str, request: &super::lite::DrainRequest) -> Result<super::lite::DrainResponse> {
        let answer = self.control(sandbox, serde_json::json!({ "drain": request }), Duration::from_secs(30))?;
        Ok(serde_json::from_value(answer["drain"].clone())?)
    }

    fn remotes(&self) -> Vec<Arc<Remote>> {
        lock(&self.remotes).values().cloned().collect()
    }

    /// A new terminal for box `id`'s launch (a new generation; the previous
    /// one, if any, is let go). Its program starts once the launch runs.
    pub fn launch(self: &Arc<Self>, id: &str) -> Arc<Remote> {
        let generation = now_ms().max(1);
        let remote = Arc::new(Remote { term: id.to_owned(), links: Arc::downgrade(self), state: Mutex::new(RemoteState { generation, cols: 100, rows: 30, ..Default::default() }) });
        lock(&self.remotes).insert(id.to_owned(), remote.clone());
        remote
    }

    /// At start: the terminals running in guests when the service last
    /// stopped (`Intent::terminal`), each handed to `adopt` (the host's
    /// `adopt_remote`); their satellites replay them once linked.
    pub fn adopt(self: &Arc<Self>, adopt: impl Fn(&str, Arc<Remote>)) {
        for intent in launch::all(&self.home).into_iter().filter(|i| i.terminal != 0 && !i.sandbox.is_empty() && !matches!(i.state, VmState::Deleted | VmState::Lost | VmState::Failed)) {
            let state = RemoteState { sandbox: intent.sandbox.clone(), generation: intent.terminal, spawned: true, cols: 100, rows: 30, ..Default::default() };
            let remote = Arc::new(Remote { term: intent.r#box.clone(), links: Arc::downgrade(self), state: Mutex::new(state) });
            lock(&self.remotes).insert(intent.r#box.clone(), remote.clone());
            (self.log)(format!("VM box {}: its terminal is adopted again (generation {}, sandbox {})", intent.r#box, intent.terminal, intent.sandbox));
            adopt(&intent.r#box, remote);
        }
    }

    /// Follows the launches of terminals not started yet (called often by
    /// the VM thread): their steps as note lines, the program started once
    /// the launch runs and its satellite is linked, the end of a launch
    /// that failed or whose VM went.
    pub fn progress(&self) {
        self.close_revoked();
        for remote in self.remotes() {
            let Ok(Some(intent)) = launch::load(&self.home, &remote.term) else { continue };
            let mut state = lock(&remote.state);
            if state.exited || state.feed.is_none() {
                continue;
            }
            if self.link(&state.sandbox).is_some() {
                state.buffered = None;
                state.buffered_bytes = 0;
            }
            let resuming = intent.state == VmState::Resuming || (matches!(intent.state, VmState::Suspended | VmState::Suspending) && intent.pending == "resume");
            if resuming {
                state.resuming_until = now_ms() + RESUME_WAIT_MS;
            }
            // Typing kept for a resume that did not link in time is dropped, not typed much later.
            if let Some((from, at)) = state.buffered
                && now_ms().saturating_sub(at) > RESUME_WAIT_MS
                && self.link(&state.sandbox).is_none()
            {
                state.unacked.retain(|(seq, _)| *seq < from);
                state.unacked_bytes = state.unacked.iter().map(|(_, b)| b.len()).sum();
                state.buffered = None;
                state.buffered_bytes = 0;
            }
            match intent.state {
                VmState::Failed => state.exit(1, Some(format!("the VM could not be started: {}", intent.error))),
                VmState::Deleted | VmState::Deleting => state.exit(1, Some("this session's VM was deleted".into())),
                VmState::Lost => state.exit(1, Some("this session's VM is gone (its work is in its pushed branch)".into())),
                _ if state.spawned => {}
                VmState::Queued => state.note(&format!("Waiting to start: {}", intent.note)),
                VmState::Claiming if intent.sandbox.is_empty() => state.note(&format!("Claiming a VM for {} on {}…", intent.placement.repo, intent.placement.branch.as_deref().unwrap_or("its branch"))),
                VmState::Running if !intent.sandbox.is_empty() => {
                    state.sandbox = intent.sandbox.clone();
                    match self.link(&intent.sandbox) {
                        None => state.note(&format!("Waiting for the VM's satellite ({})…", intent.sandbox)),
                        // A program that names the synced roots waits for them.
                        Some(_) if (intent.argv.iter().any(|a| a.contains(ROOTS)) || intent.env.iter().any(|(_, v)| v.contains(ROOTS))) && !self.synced(&intent.sandbox) => {
                            state.note(&format!("Bringing the canvas's files to the VM ({})…", intent.sandbox));
                        }
                        Some(link) => {
                            let generation = state.generation;
                            let place = |text: &String| text.replace(ROOTS, &link.roots);
                            let argv = intent.argv.iter().map(place).collect();
                            let env = intent.env.iter().map(|(k, v)| (k.clone(), place(v))).collect();
                            let spawn = SatDown::Spawn { term: remote.term.clone(), generation, argv, env, cwd: intent.cwd.clone(), cols: state.cols, rows: state.rows };
                            state.note(&format!("Starting {} in the VM ({})…", intent.agent, intent.sandbox));
                            // Recorded first: a service that stops right after the Spawn still adopts the program.
                            if launch::update(&self.home, &remote.term, |i| i.terminal = generation).is_err() {
                                continue;
                            }
                            if link.send(true, spawn) {
                                state.spawned = true;
                                state.fresh = true;
                                state.have = 0;
                            } else {
                                drop(state);
                                let _ = launch::update(&self.home, &remote.term, |i| {
                                    if i.terminal == generation {
                                        i.terminal = 0;
                                    }
                                });
                            }
                        }
                    }
                }
                VmState::Resuming | VmState::Suspended | VmState::Suspending => state.note("Resuming the VM…"),
                _ => state.note(&format!("Preparing the VM ({})…", intent.sandbox)),
            }
        }
    }

    /// Serves a satellite's link until it ends (`HostApp::serve_satellite`).
    pub fn serve(self: &Arc<Self>, link: SatelliteLink) -> Result<()> {
        let SatelliteLink { mut reader, writer, closer, info, .. } = link;
        let id = info.remote_id.clone().unwrap_or_default();
        let Some(sandbox) = super::store::Satellites::new(&self.home).sandbox_of(&id) else {
            closer.close();
            bail!("satellite {id:?} has no sandbox");
        };
        // The first message is its Hello, within the handshake's time.
        let hello = {
            let (done, wait) = bounded::<()>(1);
            let timer = closer.clone();
            std::thread::spawn(move || {
                if wait.recv_timeout(Duration::from_secs(20)).is_err() {
                    timer.close();
                }
            });
            let first = reader.recv::<ClientMsg>();
            let _ = done.send(());
            match first {
                Ok(Some(ClientMsg::Sat(up))) => match *up {
                    SatUp::Hello { version, json, .. } if (SAT_PROTOCOL_MIN..=SAT_PROTOCOL).contains(&version) => serde_json::from_str::<HelloInfo>(&json).unwrap_or_default(),
                    SatUp::Hello { version, .. } => {
                        closer.close();
                        bail!("satellite {id} speaks protocol {version}; this hub speaks {SAT_PROTOCOL_MIN} to {SAT_PROTOCOL}");
                    }
                    _ => {
                        closer.close();
                        bail!("satellite {id} did not begin with Hello");
                    }
                },
                _ => {
                    closer.close();
                    bail!("satellite {id} did not say Hello");
                }
            }
        };
        if hello.sandbox != sandbox {
            closer.close();
            bail!("satellite {id} names sandbox {:?}, not its own {sandbox}", hello.sandbox);
        }
        super::store::Satellites::new(&self.home).seen(&id);
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed) + 1;
        let (io, io_rx) = bounded::<ServiceMsg>(1024);
        let (other, other_rx) = bounded::<ServiceMsg>(1024);
        let heard = Arc::new(AtomicU64::new(now_ms()));
        let (stop, stopped) = bounded::<()>(1);
        let writing = {
            let (closer, heard) = (closer.clone(), heard.clone());
            std::thread::Builder::new().name("sat-link-writer".into()).spawn(move || write_lanes(writer, io_rx, other_rx, stopped, heard, closer))?
        };
        let current = Arc::new(Link { sandbox: sandbox.clone(), satellite: id.clone(), roots: hello.roots.clone(), guest_home: hello.home.clone(), mcp_port: hello.mcp_port, epoch, io, other, closer: closer.clone(), streams: Mutex::default() });
        if let Some(old) = lock(&self.links).insert(sandbox.clone(), current.clone()) {
            old.closer.close();
            // The older link's MCP sessions end with it (the guest's clients start new ones on this link).
            self.mcp.link_ended(&sandbox);
        }
        update_record(&self.home, &sandbox, |r| (r.linked, r.since_ms) = (true, now_ms()));
        (self.log)(format!("VM {sandbox}: satellite linked ({} on {}, {} terminal(s) there)", hello.build, hello.platform, hello.terms.len()));
        self.welcome(&current, &hello);
        let result = self.read_loop(&mut reader, &current, &heard);
        {
            let mut links = lock(&self.links);
            if links.get(&sandbox).is_some_and(|l| l.epoch == epoch) {
                links.remove(&sandbox);
            }
        }
        let _ = stop.send(());
        closer.close();
        drop(current);
        let _ = writing.join();
        if !self.linked(&sandbox) {
            self.mcp.link_ended(&sandbox);
            lock(&self.live).remove(&sandbox);
            lock(&self.writes).remove(&sandbox);
            update_record(&self.home, &sandbox, |r| (r.linked, r.since_ms) = (false, now_ms()));
        }
        (self.log)(format!("VM {sandbox}: satellite link ended{}", result.as_ref().err().map(|e| format!(" ({e:#})")).unwrap_or_default()));
        result
    }

    /// Tells a new link what the hub has of its sandbox's terminals (the
    /// satellite replays each from there), sends input not acknowledged yet
    /// and a stop asked for meanwhile. Input numbering goes on from what the
    /// satellite applied (a restarted hub numbers anew).
    fn welcome(&self, link: &Link, hello: &HelloInfo) {
        let mine: Vec<Arc<Remote>> = self.remotes().into_iter().filter(|r| lock(&r.state).sandbox == link.sandbox).collect();
        let mut terms = Vec::new();
        for remote in &mine {
            let mut state = lock(&remote.state);
            if let Some(held) = hello.terms.iter().find(|h| h.term == remote.term && h.generation == state.generation) {
                state.next_seq = state.next_seq.max(held.input_acked);
            }
            if state.spawned && !state.exited {
                state.resyncing = true;
                terms.push(TermHave { term: remote.term.clone(), generation: state.generation, have: state.have });
            }
        }
        let welcome = WelcomeInfo { terms, stats_secs: stats_secs() };
        if !link.send(true, SatDown::Welcome { json: serde_json::to_string(&welcome).unwrap_or_default() }) {
            return;
        }
        let stops: Vec<(String, u64)> = {
            let mut stops = lock(&self.stops);
            let (mine, others): (Vec<_>, Vec<_>) = stops.drain(..).partition(|(sandbox, _, _)| *sandbox == link.sandbox);
            *stops = others;
            mine.into_iter().map(|(_, term, generation)| (term, generation)).collect()
        };
        for (term, generation) in stops {
            link.send(true, SatDown::Stop { term, generation, force: true });
        }
        for remote in &mine {
            let state = lock(&remote.state);
            if !state.spawned || state.exited {
                continue;
            }
            for (seq, bytes) in &state.unacked {
                link.send(true, SatDown::Input { term: remote.term.clone(), generation: state.generation, seq: *seq, bytes: bytes.clone() });
            }
        }
    }

    fn read_loop(&self, reader: &mut FrameReader, link: &Link, heard: &AtomicU64) -> Result<()> {
        let mut flood = super::flood::Flood::default();
        loop {
            let message = match reader.recv::<ClientMsg>() {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            heard.store(now_ms(), Ordering::Relaxed);
            match message {
                ClientMsg::Heartbeat => {}
                ClientMsg::Sat(up) => {
                    // Past its allowance the hub stops reading a while: TCP slows the guest.
                    let wait = flood.take(super::flood::size(&up)?, Instant::now());
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                        heard.store(now_ms(), Ordering::Relaxed);
                    }
                    self.take(link, *up, &mut flood)?
                }
                _ => bail!("the satellite sent a client's message"),
            }
        }
    }

    /// One message from a satellite.
    fn take(&self, link: &Link, up: SatUp, flood: &mut super::flood::Flood) -> Result<()> {
        match up {
            SatUp::Mcp { stream, kind, bytes } => {
                if kind == canvas_common::sat::MCP_OPEN && !lock(&link.streams).open(stream) {
                    let refusal = serde_json::json!({ "message": format!("this VM holds {} MCP sessions already", super::flood::MCP_STREAMS) });
                    link.send(false, SatDown::Mcp { stream, kind: canvas_common::sat::MCP_ERROR, bytes: refusal.to_string().into_bytes() });
                    link.send(false, SatDown::Mcp { stream, kind: canvas_common::sat::MCP_CLOSE, bytes: Vec::new() });
                    return Ok(());
                }
                if kind == canvas_common::sat::MCP_CLOSE {
                    lock(&link.streams).close(stream);
                }
                self.mcp.up(&link.sandbox, stream, kind, bytes);
                Ok(())
            }
            SatUp::Hello { .. } => bail!("the satellite said Hello twice"),
            SatUp::Op { id, term, json } => {
                self.op(link, id, term, json);
                Ok(())
            }
            SatUp::Note { id, root, name, base_rev, bytes } => {
                // Only a writable root of this sandbox takes the guest's edits.
                let writes = lock(&self.writes).get(&link.sandbox).and_then(|w| w.get(&root)).cloned();
                let answer = match writes {
                    Some(writes) => super::sync::write(&writes, &name, base_rev.as_deref(), &bytes),
                    None => canvas_common::sat::NoteAnswer { error: "not a root the guest may write".into(), ..Default::default() },
                };
                if answer.ok {
                    (self.log)(format!("VM {}: {root}/{name} written from the guest", link.sandbox));
                }
                link.send(false, SatDown::NoteResult { id, json: serde_json::to_string(&answer)? });
                Ok(())
            }
            SatUp::Proc { json } => {
                if flood.proc_due(Instant::now()) {
                    self.processes(link, serde_json::from_str(&json).unwrap_or_default())?;
                }
                Ok(())
            }
            SatUp::Stats { json } => {
                if flood.stats_due(Instant::now()) {
                    let stats: StatsReport = serde_json::from_str(&json).unwrap_or_default();
                    update_record(&self.home, &link.sandbox, |r| (r.stats_ms, r.stats) = (now_ms(), stats));
                }
                Ok(())
            }
            up => self.take_term(link, up),
        }
    }

    /// A canvas command an agent ran in the guest: run here as its box (once:
    /// a repeat gets the answer it got), on a thread of its own, at most
    /// [`OPS_RUNNING`] at a time per sandbox; the answer goes back on the link.
    fn op(&self, link: &Link, id: String, term: String, json: String) {
        if !super::lite::valid_op_id(&id) {
            return;
        }
        let answer = |answer: &OpAnswer| link.send(false, SatDown::OpResult { id: id.clone(), json: serde_json::to_string(answer).unwrap_or_default() });
        if let Some(stored) = super::thin::stored_answer(&self.home, &link.sandbox, &id) {
            answer(&stored);
            return;
        }
        let refused = |text: &str| OpAnswer { ok: false, state: "refused".into(), text: text.into(), outputs: Vec::new() };
        let intent = match launch::load(&self.home, &term) {
            Ok(Some(intent)) if intent.sandbox == link.sandbox && !link.sandbox.is_empty() => intent,
            _ => {
                answer(&refused("this command does not come from a session of this VM"));
                return;
            }
        };
        let agent = intent.agent.clone();
        let Ok(mut op) = serde_json::from_str::<canvas_model::vm::CliOp>(&json) else {
            answer(&refused("the command could not be read"));
            return;
        };
        // Paths: the guest's in the command become this machine's, and the other way in its answer.
        let places = super::paths::Places::of(&self.home, &intent, &link.roots);
        if let Err(why) = places.command_in(&mut op) {
            answer(&refused(&why));
            return;
        }
        {
            let mut ops = lock(&self.ops);
            if ops.0.contains(&(link.sandbox.clone(), id.clone())) {
                return;
            }
            if ops.0.iter().filter(|(s, _)| *s == link.sandbox).count() >= OPS_RUNNING + OPS_WAITING {
                drop(ops);
                answer(&refused("this VM has too many canvas commands waiting; try again in a moment"));
                return;
            }
            ops.0.insert((link.sandbox.clone(), id.clone()));
        }
        let Some(links) = self.self_ref() else { return };
        let sandbox = link.sandbox.clone();
        let _ = std::thread::Builder::new().name("vm-op".into()).spawn(move || {
            {
                let mut ops = lock(&links.ops);
                while ops.1.get(&sandbox).copied().unwrap_or(0) >= OPS_RUNNING {
                    ops = links.ops_turn.wait(ops).unwrap_or_else(|e| e.into_inner());
                }
                *ops.1.entry(sandbox.clone()).or_default() += 1;
            }
            let exe = lock(&links.exe).clone();
            let mut result = super::thin::execute(&links.home, &exe, &id, &term, &agent, &op);
            result.text = places.text_out(&result.text);
            (links.log)(format!("VM {sandbox}: box {term} ran `atelier {}` ({})", op.words.first().cloned().unwrap_or_default(), result.state));
            let _ = super::thin::store_answer(&links.home, &sandbox, &id, &result);
            {
                let mut ops = lock(&links.ops);
                ops.0.remove(&(sandbox.clone(), id.clone()));
                if let Some(running) = ops.1.get_mut(&sandbox) {
                    *running = running.saturating_sub(1);
                }
            }
            links.ops_turn.notify_all();
            // The satellite resends it after a reconnect and then gets the stored answer.
            if let Some(link) = links.link(&sandbox) {
                link.send(false, SatDown::OpResult { id, json: serde_json::to_string(&result).unwrap_or_default() });
            }
        });
    }

    /// A process report: sessions found inside its sandbox's terminals become
    /// their boxes' subsessions (once each), the live sessions its overlay.
    fn processes(&self, link: &Link, report: ProcReport) -> Result<()> {
        for term in &report.terms {
            let Some(remote) = lock(&self.remotes).get(&term.term).cloned() else { continue };
            let state = lock(&remote.state);
            if state.sandbox != link.sandbox {
                bail!("the satellite of {} reported on terminal {}, which is not its sandbox's", link.sandbox, term.term);
            }
            if term.generation != state.generation {
                continue;
            }
            drop(state);
            for nested in &term.nested {
                if nested.session == term.term || !crate::index::is_session_id(&nested.session) || !lock(&self.reported).insert(nested.session.clone()) {
                    continue;
                }
                (self.log)(format!("VM {}: session {} started inside {}", link.sandbox, nested.session, term.term));
                if let Some(hook) = lock(&self.subsession).clone() {
                    hook(&term.term, &nested.session, &nested.name, &nested.cwd);
                }
            }
        }
        let live = report.live.into_iter().filter(|id| crate::index::is_session_id(id)).take(4096).collect();
        lock(&self.live).insert(link.sandbox.clone(), live);
        Ok(())
    }

    /// A terminal's output, screen, exit or input acknowledgement.
    fn take_term(&self, link: &Link, up: SatUp) -> Result<()> {
        if let SatUp::ControlResult { id, json } = &up {
            // Only the sandbox a control was sent to answers it.
            let waiting = lock(&self.controls).get(id).filter(|(sandbox, _)| *sandbox == link.sandbox).map(|(_, tx)| tx.clone());
            if let Some(tx) = waiting {
                let _ = tx.try_send(json.clone());
            }
            return Ok(());
        }
        let term = match &up {
            SatUp::Output { term, .. } | SatUp::Snapshot { term, .. } | SatUp::Exit { term, .. } | SatUp::InputAck { term, .. } => term.clone(),
            // History, operations and notes pushed by a satellite come with Phase 3 (they come through drains now).
            _ => return Ok(()),
        };
        let Some(remote) = lock(&self.remotes).get(&term).cloned() else { return Ok(()) };
        let mut state = lock(&remote.state);
        if state.sandbox != link.sandbox {
            bail!("the satellite of {} named terminal {term}, which is not its sandbox's", link.sandbox);
        }
        let generation = match &up {
            SatUp::Output { generation, .. } | SatUp::Snapshot { generation, .. } | SatUp::Exit { generation, .. } | SatUp::InputAck { generation, .. } => *generation,
            _ => 0,
        };
        if generation != state.generation {
            return Ok(());
        }
        match up {
            SatUp::Output { seq, bytes, .. } => {
                let end = seq + bytes.len() as u64;
                if seq > state.have {
                    // A gap: everything from what the hub has, once.
                    if !state.resyncing {
                        state.resyncing = true;
                        link.send(true, SatDown::Resync { term, generation, have: state.have });
                    }
                } else if end > state.have {
                    let mut fresh = bytes[(state.have - seq) as usize..].to_vec();
                    if std::mem::take(&mut state.fresh) && state.have == 0 {
                        // In the output's own stream, so it lands before the program's first byte.
                        fresh.splice(0..0, CLEAR.iter().copied());
                    }
                    if state.feed.as_ref().is_some_and(|f| f.output(fresh)) {
                        state.have = end;
                        state.resyncing = false;
                    } else if !state.resyncing {
                        // The engine fell behind: the rest comes again.
                        state.resyncing = true;
                        link.send(true, SatDown::Resync { term, generation, have: state.have });
                    }
                } else if bytes.is_empty() && seq == state.have {
                    // The satellite had nothing more: caught up.
                    state.resyncing = false;
                }
            }
            SatUp::Snapshot { seq, vt, .. } => {
                // Nothing of the hub's own is drawn in: a full-screen program's drawing must find its screen as it left it.
                if let Some(feed) = &state.feed {
                    feed.reset(vt);
                }
                (self.log)(format!("VM box {term}: output was skipped while its VM was away; its screen was repainted"));
                state.have = seq;
                state.fresh = false;
                state.resyncing = false;
            }
            SatUp::Exit { code, json, .. } => {
                let info: ExitInfo = serde_json::from_str(&json).unwrap_or_default();
                let error = if info.unknown { Some("the VM no longer runs this session".to_owned()) } else { Some(info.error).filter(|e| !e.is_empty()) };
                state.exit(code, error);
                drop(state);
                self.mcp.session_ended(&term);
                let _ = launch::update(&self.home, &term, |i| {
                    if i.terminal == generation {
                        i.terminal = 0;
                    }
                });
            }
            SatUp::InputAck { seq, .. } => {
                while state.unacked.front().is_some_and(|(s, _)| *s <= seq) {
                    if let Some((_, bytes)) = state.unacked.pop_front() {
                        state.unacked_bytes -= bytes.len();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Terminal I/O first, then the rest, then a heartbeat every 2 s; a link
/// silent for [`SILENCE`] is closed.
fn write_lanes(mut writer: FrameWriter, io: Receiver<ServiceMsg>, other: Receiver<ServiceMsg>, stop: Receiver<()>, heard: Arc<AtomicU64>, closer: Closer) {
    let ticker = crossbeam_channel::tick(HEARTBEAT);
    loop {
        let message = select_biased! {
            recv(io) -> m => m.ok(),
            recv(other) -> m => m.ok(),
            recv(stop) -> _ => None,
            recv(ticker) -> _ => {
                if now_ms().saturating_sub(heard.load(Ordering::Relaxed)) > SILENCE.as_millis() as u64 {
                    None
                } else {
                    Some(ServiceMsg::Heartbeat { input_ack: 0 })
                }
            }
        };
        let Some(message) = message else { break };
        if writer.send(&message).is_err() {
            break;
        }
    }
    closer.close();
}

impl Remote {
    fn current_link(&self, state: &RemoteState) -> Option<Arc<Link>> {
        self.links.upgrade()?.link(&state.sandbox)
    }
}

impl RemotePty for Remote {
    fn accepting(&self) -> Result<()> {
        let state = lock(&self.state);
        anyhow::ensure!(!state.exited, "the program has exited");
        anyhow::ensure!(state.spawned, "the VM is starting: input not sent");
        if self.current_link(&state).is_none() {
            anyhow::ensure!(now_ms() < state.resuming_until && state.buffered_bytes < RESUME_BUFFER, "VM offline: input not sent");
        }
        Ok(())
    }

    fn settled(&self) -> bool {
        let state = lock(&self.state);
        state.spawned && !state.exited && !state.resyncing && self.current_link(&state).is_some()
    }

    fn input(&self, bytes: Vec<u8>) {
        let mut state = lock(&self.state);
        if state.exited || !state.spawned {
            return;
        }
        let linked = self.current_link(&state).is_some();
        for piece in bytes.chunks(SAT_IO_CHUNK) {
            if state.unacked_bytes + piece.len() > UNACKED {
                return;
            }
            if !linked {
                // Kept for a resuming VM only (`accepting` refused the rest).
                if now_ms() >= state.resuming_until || state.buffered_bytes + piece.len() > RESUME_BUFFER {
                    return;
                }
                state.buffered_bytes += piece.len();
                if state.buffered.is_none() {
                    state.buffered = Some((state.next_seq + 1, now_ms()));
                }
            }
            state.next_seq += 1;
            let seq = state.next_seq;
            state.unacked.push_back((seq, piece.to_vec()));
            state.unacked_bytes += piece.len();
            if let Some(link) = self.current_link(&state) {
                link.send(true, SatDown::Input { term: self.term.clone(), generation: state.generation, seq, bytes: piece.to_vec() });
            }
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let mut state = lock(&self.state);
        (state.cols, state.rows) = (cols, rows);
        if state.spawned
            && let Some(link) = self.current_link(&state)
        {
            link.send(true, SatDown::Resize { term: self.term.clone(), generation: state.generation, cols, rows });
        }
    }

    fn stop(&self, force: bool) {
        let mut state = lock(&self.state);
        if !state.spawned {
            // Nothing runs in the guest yet: the terminal ends here (the VM stays claimed).
            state.exit(0, None);
            return;
        }
        match self.current_link(&state) {
            Some(link) => {
                link.send(true, SatDown::Stop { term: self.term.clone(), generation: state.generation, force });
            }
            None => {
                // Sent once the satellite is back; the terminal ends here now.
                if let Some(links) = self.links.upgrade() {
                    lock(&links.stops).push((state.sandbox.clone(), self.term.clone(), state.generation));
                }
                state.exit(0, Some("stopped while the VM was offline".into()));
            }
        }
    }

    fn started(&self, feed: RemoteFeed, cols: u16, rows: u16) {
        let mut state = lock(&self.state);
        state.feed = Some(feed);
        (state.cols, state.rows) = (cols, rows);
        if !state.spawned {
            state.shown.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_common::protocol::{CAP_SATELLITE, ClientInfo, Input};
    use canvas_common::sat::TermHeld;
    use canvas_host::emulator::Engine;
    use canvas_host::term::{Hooks, RemoteSpawn, Terminal};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::time::Instant;

    const A: &str = "sbx-aaaaaaaaaaaa";
    const B: &str = "sbx-bbbbbbbbbbbb";
    const BOX: &str = "0b1e5ca1-0000-4000-8000-0000000071e5";

    fn home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("atelier-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn links(home: &Path) -> Arc<Links> {
        Links::new(home, Arc::new(|_| {}))
    }

    /// The satellite's end of a link the hub serves on a loopback socket.
    struct Sat {
        reader: FrameReader,
        writer: FrameWriter,
        socket: TcpStream,
        served: std::thread::JoinHandle<Result<()>>,
    }

    impl Sat {
        fn connect(links: &Arc<Links>, id: &str) -> Sat {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let closing = server.try_clone().unwrap();
            let link = SatelliteLink {
                reader: FrameReader::new(Box::new(server.try_clone().unwrap())),
                writer: FrameWriter::new(Box::new(server)),
                closer: Closer::new(move || {
                    let _ = closing.shutdown(Shutdown::Both);
                }),
                info: ClientInfo { remote_id: Some(id.into()), caps: CAP_SATELLITE, ..Default::default() },
                websocket: false,
            };
            let served = std::thread::spawn({
                let links = links.clone();
                move || links.serve(link)
            });
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            Sat { reader: FrameReader::new(Box::new(socket.try_clone().unwrap())), writer: FrameWriter::new(Box::new(socket.try_clone().unwrap())), socket, served }
        }

        fn send(&mut self, up: SatUp) {
            self.writer.send(&ClientMsg::Sat(Box::new(up))).unwrap();
        }

        fn hello(&mut self, sandbox: &str, terms: Vec<TermHeld>) {
            let hello = HelloInfo { sandbox: sandbox.into(), build: "test".into(), platform: "linux".into(), home: "/home/agent".into(), terms, mcp_port: 0, roots: "/home/agent/.atelier/vm/roots".into() };
            self.send(SatUp::Hello { version: SAT_PROTOCOL, caps: 0, json: serde_json::to_string(&hello).unwrap() });
        }

        /// The hub's next message to the satellite (heartbeats skipped).
        fn next(&mut self) -> SatDown {
            loop {
                match self.reader.recv::<ServiceMsg>().unwrap().expect("the link ended") {
                    ServiceMsg::Sat(down) => return *down,
                    ServiceMsg::Heartbeat { .. } => {}
                    other => panic!("a satellite got a client's message: {other:?}"),
                }
            }
        }

        /// Drops the link and returns how serving it ended.
        fn close(self) -> Result<()> {
            let _ = self.socket.shutdown(Shutdown::Both);
            self.served.join().unwrap()
        }
    }

    /// A box's terminal adopted on sandbox `sandbox` at generation 7 (its intent says it runs there).
    fn adopted(home: &Path, links: &Arc<Links>, sandbox: &str) -> Arc<Terminal> {
        let mut intent = launch::Intent { r#box: BOX.into(), agent: "copilot".into(), sandbox: sandbox.into(), state: VmState::Running, terminal: 7, ..Default::default() };
        launch::save(home, &mut intent).unwrap();
        let made = Arc::new(Mutex::new(None));
        links.adopt(|id, remote| {
            let (pump, _) = bounded(1);
            let (exited, _) = crossbeam_channel::unbounded();
            let (clipboard, _) = bounded(1);
            let (changed, _) = bounded(16);
            let hooks = Hooks { pump, exited, clipboard, bytes: Default::default(), changed, notify: None, raw: None };
            let (terminal, feed) = Terminal::spawn_remote(RemoteSpawn { id, cols: 60, rows: 8, copilot: true, engine: Engine::Alacritty }, hooks, remote.clone());
            remote.started(feed, 60, 8);
            *lock(&made) = Some(terminal);
        });
        lock(&made).take().expect("adopted")
    }

    fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_satellite_is_served_only_as_its_own_sandbox_and_hello_comes_first() {
        let home = home();
        let links = links(&home);
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let mut sat = Sat::connect(&links, &minted.id);
        sat.writer.send(&ClientMsg::Heartbeat).unwrap();
        assert!(sat.close().unwrap_err().to_string().contains("Hello"));
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(B, Vec::new());
        assert!(sat.close().unwrap_err().to_string().contains("not its own"));
        let sat = Sat::connect(&links, "sat-0000000000000000");
        assert!(sat.close().unwrap_err().to_string().contains("has no sandbox"));
        // Its own sandbox: the hub's first frame is the satellite's Welcome, nothing a client gets.
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        assert!(links.linked(A) && !links.linked(B));
        sat.close().unwrap();
        assert!(!links.linked(A));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn output_is_taken_in_order_and_input_is_numbered_until_acknowledged() {
        let home = home();
        let links = links(&home);
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let terminal = adopted(&home, &links, A);
        assert!(terminal.send_input(&Input::Raw(b"x".to_vec())).unwrap_err().to_string().contains("VM offline"), "no link yet");
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, vec![TermHeld { term: BOX.into(), generation: 7, end: 0, alive: true, input_acked: 0 }]);
        let SatDown::Welcome { json } = sat.next() else { panic!("Welcome first") };
        let welcome: WelcomeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(welcome.terms, [TermHave { term: BOX.into(), generation: 7, have: 0 }]);

        let output = |seq: u64, generation: u64, text: &str| SatUp::Output { term: BOX.into(), generation, seq, bytes: text.as_bytes().to_vec() };
        sat.send(output(0, 7, "hello "));
        sat.send(output(20, 7, "gap"));
        assert!(matches!(sat.next(), SatDown::Resync { have: 6, generation: 7, .. }), "a gap asks for what the hub lacks");
        sat.send(output(3, 7, "lo world"));
        sat.send(output(11, 8, " stale"));
        wait_until("the overlap is trimmed", || terminal.snapshot().is_some_and(|s| s.text().contains("hello world")));
        std::thread::sleep(Duration::from_millis(50));
        let text = terminal.snapshot().unwrap().text();
        assert!(!text.contains("lolo") && !text.contains("stale"), "{text:?}");

        terminal.send_input(&Input::Raw(b"ls\r".to_vec())).unwrap();
        assert!(matches!(sat.next(), SatDown::Input { seq: 1, ref bytes, .. } if bytes == b"ls\r"));
        sat.close().unwrap();
        assert!(terminal.send_input(&Input::Raw(b"y".to_vec())).unwrap_err().to_string().contains("VM offline"), "refused while the link is down");

        // Back: what the hub has, then the input not acknowledged, again.
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, vec![TermHeld { term: BOX.into(), generation: 7, end: 11, alive: true, input_acked: 0 }]);
        let SatDown::Welcome { json } = sat.next() else { panic!("Welcome first") };
        assert_eq!(serde_json::from_str::<WelcomeInfo>(&json).unwrap().terms[0].have, 11);
        assert!(matches!(sat.next(), SatDown::Input { seq: 1, .. }), "sent again until acknowledged");
        sat.send(SatUp::InputAck { term: BOX.into(), generation: 7, seq: 1 });
        sat.send(SatUp::Exit { term: BOX.into(), generation: 7, code: 3, json: "{}".into() });
        wait_until("the program's exit reaches the terminal", || !terminal.alive.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(*terminal.raw_exit_code.lock().unwrap(), Some(3));
        wait_until("the launch forgets the terminal", || launch::load(&home, BOX).unwrap().unwrap().terminal == 0);
        sat.close().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_satellite_naming_another_sandboxs_terminal_or_sending_a_clients_message_is_closed() {
        let home = home();
        let links = links(&home);
        let store = super::super::store::Satellites::new(&home);
        let (a, b) = (store.mint(A).unwrap(), store.mint(B).unwrap());
        let _terminal = adopted(&home, &links, A);
        let mut sat = Sat::connect(&links, &b.id);
        sat.hello(B, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        sat.send(SatUp::Output { term: BOX.into(), generation: 7, seq: 0, bytes: b"not mine".to_vec() });
        let error = sat.served.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("not its sandbox's"), "{error}");
        let mut sat = Sat::connect(&links, &a.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        sat.writer.send(&ClientMsg::Input { id: BOX.into(), input: Input::Raw(b"x".to_vec()) }).unwrap();
        assert!(sat.served.join().unwrap().unwrap_err().to_string().contains("client's message"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn typing_while_a_vm_resumes_is_kept_and_sent_once_it_links_and_the_mailbox_waits_for_the_replay() {
        let home = home();
        let links = links(&home);
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let terminal = adopted(&home, &links, A);
        launch::update(&home, BOX, |i| i.state = VmState::Suspended).unwrap();
        links.progress();
        assert!(terminal.send_input(&Input::Raw(b"x".to_vec())).unwrap_err().to_string().contains("VM offline"), "a suspended VM refuses typing");
        launch::update(&home, BOX, |i| i.pending = "resume".into()).unwrap();
        links.progress();
        terminal.send_input(&Input::Raw(b"abc".to_vec())).unwrap();
        assert!(!terminal.remote_settled(), "no link: the mailbox waits");
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, vec![TermHeld { term: BOX.into(), generation: 7, end: 0, alive: true, input_acked: 0 }]);
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        assert!(matches!(sat.next(), SatDown::Input { seq: 1, ref bytes, .. } if bytes == b"abc"), "kept and sent once linked");
        assert!(!terminal.remote_settled(), "replaying: the mailbox still waits");
        sat.send(SatUp::Output { term: BOX.into(), generation: 7, seq: 0, bytes: Vec::new() });
        wait_until("caught up", || terminal.remote_settled());
        sat.close().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_stop_asked_while_offline_reaches_the_guest_even_after_a_new_launch() {
        let home = home();
        let links = links(&home);
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let terminal = adopted(&home, &links, A);
        terminal.stop(true);
        wait_until("the terminal ends here at once", || !terminal.alive.load(std::sync::atomic::Ordering::Acquire));
        // Launched again before the satellite is back: the old program's stop still goes out.
        let _next = links.launch(BOX);
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, vec![TermHeld { term: BOX.into(), generation: 7, end: 0, alive: true, input_acked: 0 }]);
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        assert!(matches!(sat.next(), SatDown::Stop { generation: 7, force: true, .. }));
        sat.close().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn an_mcp_stream_for_a_box_not_of_the_sandbox_is_refused_on_the_link() {
        let home = home();
        let links = links(&home);
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        let open = canvas_common::sat::McpOpen { term: BOX.into(), server: "forge".into() };
        sat.send(SatUp::Mcp { stream: 9, kind: canvas_common::sat::MCP_OPEN, bytes: serde_json::to_vec(&open).unwrap() });
        assert!(matches!(sat.next(), SatDown::Mcp { stream: 9, kind: canvas_common::sat::MCP_ERROR, .. }));
        assert!(matches!(sat.next(), SatDown::Mcp { stream: 9, kind: canvas_common::sat::MCP_CLOSE, .. }));
        sat.close().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn process_reports_become_subsessions_once_and_the_live_overlay_and_stats_are_kept() {
        use canvas_common::sat::{NestedSession, ProcReport, RepoStats, StatsReport, TermProcs};
        let home = home();
        let links = links(&home);
        let told: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        links.on_subsession({
            let told = told.clone();
            Arc::new(move |parent: &str, child: &str, name: &str, _cwd: &str| told.lock().unwrap().push((parent.into(), child.into(), name.into())))
        });
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let _terminal = adopted(&home, &links, A);
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, Vec::new());
        let SatDown::Welcome { json } = sat.next() else { panic!("Welcome first") };
        assert_eq!(serde_json::from_str::<WelcomeInfo>(&json).unwrap().stats_secs, 30);
        let child = "11111111-2222-4333-8444-555555555555";
        let nested = NestedSession { session: child.into(), name: "Nested".into(), cwd: "/home/agent/work/app".into() };
        let report = ProcReport { terms: vec![TermProcs { term: BOX.into(), generation: 7, nested: vec![nested] }], live: vec![child.into(), "not a session".into()] };
        sat.send(SatUp::Proc { json: serde_json::to_string(&report).unwrap() });
        wait_until("the subsession is told", || told.lock().unwrap().len() == 1);
        assert_eq!(told.lock().unwrap()[0], (BOX.to_owned(), child.to_owned(), "Nested".to_owned()));
        assert!(links.session_live(A, child) && !links.session_live(B, child));
        std::thread::sleep(Duration::from_millis(600));
        sat.send(SatUp::Proc { json: serde_json::to_string(&report).unwrap() });
        let repo = RepoStats { term: BOX.into(), branch: "atelier/x".into(), untracked: 2, ..Default::default() };
        sat.send(SatUp::Stats { json: serde_json::to_string(&StatsReport { repos: vec![repo], ..Default::default() }).unwrap() });
        wait_until("the stats are kept", || load_record(&home, A).is_some_and(|r| r.linked && r.stats.repos.first().is_some_and(|r| r.untracked == 2)));
        assert_eq!(told.lock().unwrap().len(), 1, "told once");
        sat.close().unwrap();
        assert!(!links.session_live(A, child), "nothing is live while the VM is away");
        assert!(load_record(&home, A).is_some_and(|r| !r.linked && r.since_ms > 0));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_satellite_over_its_limits_or_reporting_on_another_sandbox_is_closed() {
        let home = home();
        let links = links(&home);
        let store = super::super::store::Satellites::new(&home);
        let (a, b) = (store.mint(A).unwrap(), store.mint(B).unwrap());
        let _terminal = adopted(&home, &links, A);
        let mut sat = Sat::connect(&links, &a.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        sat.send(SatUp::Output { term: BOX.into(), generation: 7, seq: 0, bytes: vec![b'x'; 1 << 20] });
        assert!(sat.served.join().unwrap().unwrap_err().to_string().contains("output of"));
        let mut sat = Sat::connect(&links, &b.id);
        sat.hello(B, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        let report = canvas_common::sat::ProcReport { terms: vec![canvas_common::sat::TermProcs { term: BOX.into(), generation: 7, nested: Vec::new() }], live: Vec::new() };
        sat.send(SatUp::Proc { json: serde_json::to_string(&report).unwrap() });
        assert!(sat.served.join().unwrap().unwrap_err().to_string().contains("not its sandbox's"));
        let mut sat = Sat::connect(&links, &a.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        sat.hello(A, Vec::new());
        assert!(sat.served.join().unwrap().unwrap_err().to_string().contains("Hello twice"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_relayed_command_runs_once_as_its_box_and_its_answer_goes_back() {
        let home = home();
        let links = links(&home);
        // A program standing in for this one (on Unix it takes any words and prints them).
        links.set_exe(if cfg!(windows) { PathBuf::from(r"C:\Windows\System32\whoami.exe") } else { PathBuf::from("/bin/echo") });
        let minted = super::super::store::Satellites::new(&home).mint(A).unwrap();
        let _terminal = adopted(&home, &links, A);
        let mut sat = Sat::connect(&links, &minted.id);
        sat.hello(A, Vec::new());
        assert!(matches!(sat.next(), SatDown::Welcome { .. }));
        let op = canvas_model::vm::CliOp { words: vec!["status".into(), "ok".into(), "from the guest".into()], ..Default::default() };
        let send = |sat: &mut Sat, id: &str, term: &str| sat.send(SatUp::Op { id: id.into(), term: term.into(), json: serde_json::to_string(&op).unwrap() });
        send(&mut sat, "op-1", BOX);
        let answer = |sat: &mut Sat| loop {
            if let SatDown::OpResult { id, json } = sat.next() {
                return (id, serde_json::from_str::<OpAnswer>(&json).unwrap());
            }
        };
        let (id, first) = answer(&mut sat);
        assert_eq!(id, "op-1");
        // (Windows: whoami takes no words, so it refuses them; the answer still comes back and is kept.)
        if cfg!(unix) {
            assert!(first.ok && first.state == "applied", "{first:?}");
            assert_eq!(first.text.trim(), "status ok from the guest", "the box's command line");
        }
        assert!(super::super::thin::stored_answer(&home, A, "op-1").is_some(), "kept");
        // Sent again (a reconnect): the same answer, from the store.
        send(&mut sat, "op-1", BOX);
        assert_eq!(answer(&mut sat), ("op-1".to_owned(), first));
        // Another sandbox's box, a bad id: refused, nothing run.
        send(&mut sat, "op-2", "0b1e5ca1-0000-4000-8000-00000000ffff");
        let (id, refused) = answer(&mut sat);
        assert!(id == "op-2" && !refused.ok && refused.text.contains("not come from a session of this VM"));
        send(&mut sat, "../op", BOX);
        assert!(super::super::thin::stored_answer(&home, A, "op-2").is_none());
        sat.close().unwrap();
        let _ = std::fs::remove_dir_all(home);
    }
}
