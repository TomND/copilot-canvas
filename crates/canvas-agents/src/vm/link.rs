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
//!   the terminal's engine and is marked with a note line.
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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canvas_common::protocol::{ClientMsg, ServiceMsg};
use canvas_common::sat::{ExitInfo, HelloInfo, SAT_IO_CHUNK, SAT_PROTOCOL, SAT_PROTOCOL_MIN, SatDown, SatUp, TermHave, WelcomeInfo};
use canvas_common::wire::{Closer, FrameReader, FrameWriter};
use canvas_host::server::SatelliteLink;
use canvas_host::term::{RemoteFeed, RemotePty};
use canvas_model::vm::VmState;
use crossbeam_channel::{Receiver, Sender, bounded, select_biased};

use super::launch;
use super::satellite::SILENCE;
use crate::paths::now_ms;

/// Input kept until the satellite acknowledges it.
const UNACKED: usize = 1 << 20;
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
}

/// A satellite's live link.
struct Link {
    sandbox: String,
    epoch: u64,
    io: Sender<ServiceMsg>,
    other: Sender<ServiceMsg>,
    closer: Closer,
}

impl Link {
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
    next_seq: u64,
    unacked: VecDeque<(u64, Vec<u8>)>,
    unacked_bytes: usize,
    cols: u16,
    rows: u16,
    /// The last launch step shown as a note line.
    shown: String,
    exited: bool,
    /// A stop asked for while the link was down.
    stop: bool,
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
        Arc::new(Links { home: home.to_path_buf(), log, links: Mutex::default(), remotes: Mutex::default(), epochs: AtomicU64::new(0), controls: Mutex::default(), control_ids: AtomicU64::new(0) })
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
        for remote in self.remotes() {
            let Ok(Some(intent)) = launch::load(&self.home, &remote.term) else { continue };
            let mut state = lock(&remote.state);
            if state.exited || state.feed.is_none() {
                continue;
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
                        Some(link) => {
                            let spawn = SatDown::Spawn { term: remote.term.clone(), generation: state.generation, argv: intent.argv.clone(), env: intent.env.clone(), cwd: intent.cwd.clone(), cols: state.cols, rows: state.rows };
                            state.note(&format!("Starting {} in the VM ({})…", intent.agent, intent.sandbox));
                            if link.send(false, spawn) {
                                state.spawned = true;
                                state.have = 0;
                                let generation = state.generation;
                                drop(state);
                                let _ = launch::update(&self.home, &remote.term, |i| i.terminal = generation);
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
        let current = Arc::new(Link { sandbox: sandbox.clone(), epoch, io, other, closer: closer.clone() });
        if let Some(old) = lock(&self.links).insert(sandbox.clone(), current.clone()) {
            old.closer.close();
        }
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
        let welcome = WelcomeInfo { terms };
        if !link.send(false, SatDown::Welcome { json: serde_json::to_string(&welcome).unwrap_or_default() }) {
            return;
        }
        for remote in &mine {
            let mut state = lock(&remote.state);
            if !state.spawned {
                continue;
            }
            for (seq, bytes) in &state.unacked {
                link.send(true, SatDown::Input { term: remote.term.clone(), generation: state.generation, seq: *seq, bytes: bytes.clone() });
            }
            if state.stop && link.send(true, SatDown::Stop { term: remote.term.clone(), generation: state.generation, force: true }) {
                state.stop = false;
            }
        }
    }

    fn read_loop(&self, reader: &mut FrameReader, link: &Link, heard: &AtomicU64) -> Result<()> {
        loop {
            let message = match reader.recv::<ClientMsg>() {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            heard.store(now_ms(), Ordering::Relaxed);
            match message {
                ClientMsg::Heartbeat => {}
                ClientMsg::Sat(up) => self.take(link, *up)?,
                _ => bail!("the satellite sent a client's message"),
            }
        }
    }

    /// One message from a satellite.
    fn take(&self, link: &Link, up: SatUp) -> Result<()> {
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
            // History, operations, notes, MCP and stats come with Phase 3; liveness with the mirrors.
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
                        link.send(false, SatDown::Resync { term, generation, have: state.have });
                    }
                } else if end > state.have {
                    let fresh = bytes[(state.have - seq) as usize..].to_vec();
                    if state.feed.as_ref().is_some_and(|f| f.output(fresh)) {
                        state.have = end;
                        state.resyncing = false;
                    } else if !state.resyncing {
                        // The engine fell behind: the rest comes again.
                        state.resyncing = true;
                        link.send(false, SatDown::Resync { term, generation, have: state.have });
                    }
                }
            }
            SatUp::Snapshot { seq, vt, .. } => {
                if let Some(feed) = &state.feed {
                    feed.reset(vt);
                    feed.note("Output was skipped while the VM was away: this is its screen now.");
                }
                state.have = seq;
                state.resyncing = false;
            }
            SatUp::Exit { code, json, .. } => {
                let info: ExitInfo = serde_json::from_str(&json).unwrap_or_default();
                let error = if info.unknown { Some("the VM no longer runs this session".to_owned()) } else { Some(info.error).filter(|e| !e.is_empty()) };
                state.exit(code, error);
                drop(state);
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
        self.current_link(&state).context("VM offline: input not sent")?;
        Ok(())
    }

    fn input(&self, bytes: Vec<u8>) {
        let mut state = lock(&self.state);
        if state.exited || !state.spawned {
            return;
        }
        for piece in bytes.chunks(SAT_IO_CHUNK) {
            if state.unacked_bytes + piece.len() > UNACKED {
                return;
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
                state.stop = true;
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
