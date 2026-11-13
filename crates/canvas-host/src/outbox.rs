//! What one client is sent, waiting for its writer thread
//! (`server::spawn_writer`). Not one FIFO: a keystroke's echo must not wait
//! behind a backlog of frames, replies or snapshots. The writer takes from
//! four lanes, in this order:
//!
//! 1. **Control**, in order: `Ready8`, errors, small replies, notices, and the
//!    terminal and session state. State is coalesced, never dropped: a
//!    `Terminals` update joins the one already waiting (the newest info per
//!    terminal; a stale info of an earlier run of it never replaces a newer
//!    one; `TerminalGone` replaces it), `Sessions` upserts and removals merge
//!    (a full list replaces what waits), and `Stats` or a tunnel token
//!    replaces the one waiting. A coalesced message keeps the place of its
//!    first part, so the lane's order holds (the terminal list a client gets
//!    on connect comes before the answer to its first `Stats`).
//! 2. **The focused tile's frame** (`Subscribe8::focused`).
//! 3. **Other tiles' frames**, oldest first. At most one frame per tile waits:
//!    the pump makes the next one once the writer wrote it, from the screen
//!    then, so a slow link gets fewer, newer frames instead of a queue.
//! 4. **Bulk**, in order: documents and their patches, bus snapshots (a newer
//!    one replaces one still waiting), the reports inbox, large replies,
//!    images and their placements. Bulk that has waited [`BULK_WAIT`] goes
//!    before other tiles' frames, so frames never starve it.
//!
//! Lanes are bounded in messages and bytes (frames by the one-per-tile rule).
//! A client whose control or bulk lane overflows is dropped (it reconnects
//! and gets everything again): nothing it is sent is lost silently.
//!
//! **Flow control.** The lanes order only what is still here: a tunnel that
//! buffers (Dev Tunnels holds about 1 MiB per channel, in arrival order)
//! would otherwise take everything at loopback speed and make an echo wait
//! behind it there. A client that acknowledges what it reads (`CAP_ACK`,
//! told so by `ServiceMsg::Caps`) gets a [`Window`]: other tiles' frames and
//! bulk go only while fewer than its budget of bytes are unacknowledged
//! (about what the link carries in [`FLOW_TARGET`] beyond its round trip,
//! within [`WINDOW_MIN`]..[`WINDOW_MAX`]); control and the focused tile's
//! frame may have [`URGENT_EXTRA`] more in flight. One message always goes
//! when nothing is in flight. Remote clients that do not acknowledge (older
//! builds, the phone) get a stopgap instead ([`Pace`]): while they typed in
//! the last [`TYPING_SPAN`], other tiles' frames and bulk go at most
//! [`TYPING_RATE`] bytes per second.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{ServiceMsg, SessionSummary, TermInfo};
use crate::term::lock;

/// Control messages waiting at most, and their bytes.
pub const CONTROL_LIMIT: usize = 1 << 16;
pub const CONTROL_BYTES: usize = 32 << 20;
/// Bulk messages waiting at most, and their bytes (one message always fits an empty lane).
pub const BULK_LIMIT: usize = 1 << 16;
pub const BULK_BYTES: usize = 96 << 20;
/// A message larger than this goes with the bulk (a big reply, a long clipboard text).
pub const SMALL: usize = 64 << 10;
/// Bulk that waited this long goes before other tiles' frames.
pub const BULK_WAIT: Duration = Duration::from_millis(250);
/// Bytes other tiles' frames and bulk may have unacknowledged, at least and at most.
pub const WINDOW_MIN: u64 = 32 << 10;
pub const WINDOW_MAX: u64 = 256 << 10;
/// The budget before any rate was measured.
pub const WINDOW_START: u64 = 128 << 10;
/// What control and the focused tile's frame may have in flight beyond the budget.
pub const URGENT_EXTRA: u64 = 128 << 10;
/// How long the bulk in flight may make an echo wait beyond the round trip:
/// the budget is the measured rate times the round trip plus this.
pub const FLOW_TARGET: Duration = Duration::from_millis(150);
/// The rate is measured over the acknowledgements of the last second of a busy spell.
const RATE_SPAN: Duration = Duration::from_secs(1);
const RATE_MIN_SPAN: Duration = Duration::from_millis(100);
/// Stopgap pacing (clients that do not acknowledge): while the client typed
/// this recently, other tiles' frames and bulk go at most `TYPING_RATE`
/// bytes per second, in bursts of `TYPING_BURST`.
pub const TYPING_SPAN: Duration = Duration::from_secs(2);
pub const TYPING_RATE: u64 = 128 << 10;
const TYPING_BURST: u64 = 32 << 10;

/// A message waiting for a client's writer: an ordinary one, or an image
/// whose PNG is shared by every client it is queued for.
pub enum Outgoing {
    Message(ServiceMsg),
    Image { term: String, image_id: u32, width: u32, height: u32, png: Arc<[u8]> },
}

/// What became of a message handed to the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// The client is gone (its connection ended, or it was dropped).
    Closed,
    /// No room: over the lane's budget, or a frame of that tile still waits.
    /// Frames and images wait for later; anything else must drop the client.
    Full,
}

/// One message for the writer; `frame_of`: the tile whose frame it is;
/// `urgent`: control or the focused tile's frame (flow control lets more of
/// those be in flight).
pub struct Taken {
    pub message: Outgoing,
    pub frame_of: Option<String>,
    pub urgent: bool,
}

/// What the writer gets from [`Outbox::next`].
pub enum Next {
    Item(Taken),
    /// The deadline passed with nothing to write (a heartbeat may be due).
    Timeout,
    Closed,
}

/// Messages whose newest one replaces one still waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Latest {
    Stats,
    TunnelAccess,
    Bus,
    Reports,
}

/// A place in a lane.
enum Entry {
    Item(Outgoing, usize),
    /// The terminal state waiting in [`Lanes::terms`].
    Terminals,
    /// The session state waiting in [`Lanes::sessions`].
    Sessions,
    Latest(Latest),
}

enum TermState {
    Info(TermInfo),
    Gone,
}

#[derive(Default)]
struct SessionsWaiting {
    full: bool,
    upsert: Vec<SessionSummary>,
    remove: Vec<String>,
}

/// Flow control for a client that acknowledges what it reads (`CAP_ACK`):
/// bytes written since `ServiceMsg::Caps`, bytes acknowledged, and the
/// budget of unacknowledged bytes other tiles' frames and bulk may have.
/// The budget follows the link: the rate acknowledged during a busy spell
/// times its round trip plus [`FLOW_TARGET`], so on a slow link an echo
/// waits about `FLOW_TARGET` behind bulk, and a fast one is not held back
/// (within [`WINDOW_MIN`]..[`WINDOW_MAX`]).
#[derive(Debug)]
pub struct Window {
    written: u64,
    acked: u64,
    budget: u64,
    /// Messages in flight: (bytes written up to and including it, its size,
    /// when, whether nothing was in flight before it).
    sent: VecDeque<(u64, u64, Instant, bool)>,
    /// Round trips of messages written with nothing in flight before them
    /// (no queue ahead of them): from the write to the acknowledgement, and
    /// the message's size (its own transfer is taken off at the measured rate).
    alone: VecDeque<(Duration, u64)>,
    /// The rate measured last (bytes per second).
    rate: Option<f64>,
    /// Acknowledgements of the current busy spell: (when, bytes acknowledged).
    samples: VecDeque<(Instant, u64)>,
}

impl Default for Window {
    fn default() -> Self {
        Self { written: 0, acked: 0, budget: WINDOW_START, sent: VecDeque::new(), alone: VecDeque::new(), rate: None, samples: VecDeque::new() }
    }
}

impl Window {
    pub fn in_flight(&self) -> u64 {
        self.written - self.acked
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Whether a message may be written now (`urgent`: control or the focused frame).
    pub fn allows(&self, urgent: bool) -> bool {
        let in_flight = self.in_flight();
        in_flight == 0 || in_flight < self.budget + if urgent { URGENT_EXTRA } else { 0 }
    }

    /// `bytes` were written at `now`.
    pub fn wrote(&mut self, bytes: u64, now: Instant) {
        if bytes == 0 {
            return;
        }
        let alone = self.in_flight() == 0;
        if alone {
            // A new busy spell: its rate is measured from its own acknowledgements.
            self.samples.clear();
        }
        self.written += bytes;
        self.sent.push_back((self.written, bytes, now, alone));
    }

    /// The round trip without queueing: the shortest one of a message that
    /// had nothing ahead of it, less its own transfer at the measured rate.
    fn base_rtt(&self) -> Duration {
        let transfer = |size: u64| self.rate.map_or(Duration::ZERO, |rate| Duration::from_secs_f64(size as f64 / rate));
        self.alone.iter().map(|(rtt, size)| rtt.saturating_sub(transfer(*size))).min().unwrap_or_default()
    }

    /// The client acknowledged `total` bytes at `now`.
    pub fn acked(&mut self, total: u64, now: Instant) {
        let total = total.min(self.written);
        if total <= self.acked {
            return;
        }
        // Was the window what held the sender back (then the rate is the link's)?
        let limited = self.in_flight() >= self.budget / 2;
        self.acked = total;
        while self.sent.front().is_some_and(|(end, ..)| *end <= total) {
            if let Some((_, size, at, true)) = self.sent.pop_front() {
                if self.alone.len() == 8 {
                    self.alone.pop_front();
                }
                self.alone.push_back((now.saturating_duration_since(at), size));
            }
        }
        self.samples.push_back((now, total));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > RATE_SPAN) {
            self.samples.pop_front();
        }
        let Some(&(since, from)) = self.samples.front() else { return };
        let span = now.saturating_duration_since(since);
        if span < RATE_MIN_SPAN {
            return;
        }
        let rate = (total - from) as f64 / span.as_secs_f64();
        self.rate = Some(rate);
        let horizon = self.base_rtt() + FLOW_TARGET;
        let fit = ((rate * horizon.as_secs_f64()) as u64).clamp(WINDOW_MIN, WINDOW_MAX);
        // A rate measured while the sender had little to send says nothing
        // about the link: it may only raise the budget.
        if fit > self.budget || limited {
            self.budget = fit;
        }
    }
}

/// The stopgap for remote clients that do not acknowledge: while the client
/// typed in the last [`TYPING_SPAN`], other tiles' frames and bulk leave at
/// most [`TYPING_RATE`] bytes per second (a token bucket kept as the time
/// the next byte may go).
#[derive(Debug)]
struct Pace {
    typed_at: Option<Instant>,
    free_at: Instant,
}

impl Pace {
    fn typing(&self, now: Instant) -> bool {
        self.typed_at.is_some_and(|at| now.saturating_duration_since(at) < TYPING_SPAN)
    }

    /// `None`: non-urgent bytes may go now; else when to look again.
    fn blocked(&self, now: Instant) -> Option<Instant> {
        if !self.typing(now) || self.free_at <= now {
            return None;
        }
        let lifts = self.typed_at.map_or(now, |at| at + TYPING_SPAN);
        Some(self.free_at.min(lifts))
    }

    fn charge(&mut self, bytes: u64, now: Instant) {
        if !self.typing(now) {
            return;
        }
        let burst = Duration::from_secs_f64(TYPING_BURST as f64 / TYPING_RATE as f64);
        self.free_at = self.free_at.max(now.checked_sub(burst).unwrap_or(now)) + Duration::from_secs_f64(bytes as f64 / TYPING_RATE as f64);
    }
}

#[derive(Default)]
struct Lanes {
    control: VecDeque<Entry>,
    control_bytes: usize,
    /// With when each entry was queued.
    bulk: VecDeque<(Instant, Entry)>,
    bulk_bytes: usize,
    /// Terminal state waiting, by terminal id, and whether its `Entry::Terminals` is in `control`.
    terms: Vec<(String, TermState)>,
    terms_queued: bool,
    /// A terminal list was pushed (even an empty one: a client takes its first list as the whole of them).
    terms_list: bool,
    sessions: Option<SessionsWaiting>,
    latest: HashMap<Latest, ServiceMsg>,
    frames: HashMap<String, ServiceMsg>,
    frame_order: VecDeque<String>,
    /// The tile whose frame the writer took and has not written yet.
    writing: Option<String>,
    /// When each tile's last frame was written.
    written: HashMap<String, Instant>,
    focused: Option<String>,
    closed: bool,
    /// Why the client is dropped: a lane overflowed.
    overflow: Option<String>,
    /// Flow control, once the client was told to acknowledge (`ServiceMsg::Caps`).
    window: Option<Window>,
    /// The stopgap for a remote client that does not acknowledge.
    pace: Option<Pace>,
    /// When to look again at what the pace holds back.
    retry_at: Option<Instant>,
}

/// A client's outgoing lanes, shared by its senders and its writer.
#[derive(Default)]
pub struct Outbox {
    lanes: Mutex<Lanes>,
    ready: Condvar,
    /// Bytes of images queued and not written yet (the image budget).
    image_bytes: AtomicUsize,
}

/// About how many bytes a message takes on the wire (budgets only).
pub fn weight(message: &ServiceMsg) -> usize {
    let runs8 = |rows: &[(u16, Vec<crate::protocol::Run8>)]| rows.iter().map(|(_, row)| 4 + row.iter().map(|r| 24 + r.text.len()).sum::<usize>()).sum::<usize>();
    64 + match message {
        ServiceMsg::Frame8(frame) => runs8(&frame.changed) + frame.links.iter().map(String::len).sum::<usize>(),
        ServiceMsg::Frame(frame) => frame.changed.iter().map(|(_, row)| 4 + row.iter().map(|r| 16 + r.text.len()).sum::<usize>()).sum(),
        ServiceMsg::Bus { json } | ServiceMsg::BusEvent { json } | ServiceMsg::Reports { json } | ServiceMsg::Doc { json, .. } | ServiceMsg::RpcReply { json, .. } => json.len(),
        ServiceMsg::DocPatch { ops_json, .. } => ops_json.len(),
        ServiceMsg::Clipboard(text) => text.len(),
        ServiceMsg::Error { message, .. } => message.len(),
        ServiceMsg::Notification { title, body, .. } => title.len() + body.len(),
        ServiceMsg::Image { rgba_or_png, .. } => rgba_or_png.len(),
        ServiceMsg::Placements { placements, .. } => placements.len() * 64,
        ServiceMsg::Terminals(infos) => infos.len() * 160,
        ServiceMsg::Sessions { upsert, remove, .. } => upsert.len() * 320 + remove.len() * 40,
        _ => 0,
    }
}

impl Lanes {
    fn push(&mut self, message: ServiceMsg) -> Push {
        match message {
            ServiceMsg::Terminals(infos) => {
                self.terms_list = true;
                for info in infos {
                    self.merge_term(info.id.clone(), TermState::Info(info));
                }
                self.ensure_terms();
            }
            ServiceMsg::TerminalGone(id) => {
                self.merge_term(id, TermState::Gone);
                self.ensure_terms();
            }
            ServiceMsg::Sessions { full, upsert, remove } => {
                let waiting = match &mut self.sessions {
                    Some(waiting) => waiting,
                    None => {
                        self.control.push_back(Entry::Sessions);
                        self.sessions.insert(SessionsWaiting::default())
                    }
                };
                if full {
                    *waiting = SessionsWaiting { full: true, upsert, remove };
                } else {
                    for summary in upsert {
                        waiting.remove.retain(|id| *id != summary.id);
                        match waiting.upsert.iter_mut().find(|s| s.id == summary.id) {
                            Some(old) => *old = summary,
                            None => waiting.upsert.push(summary),
                        }
                    }
                    for id in remove {
                        waiting.upsert.retain(|s| s.id != id);
                        if !waiting.remove.contains(&id) {
                            waiting.remove.push(id);
                        }
                    }
                }
            }
            message @ ServiceMsg::Stats(_) => self.latest(Latest::Stats, message, false),
            message @ ServiceMsg::TunnelAccess { .. } => self.latest(Latest::TunnelAccess, message, false),
            message @ ServiceMsg::Bus { .. } => self.latest(Latest::Bus, message, true),
            message @ ServiceMsg::Reports { .. } => self.latest(Latest::Reports, message, true),
            message => {
                let weight = weight(&message);
                let bulk = weight > SMALL
                    || matches!(
                        message,
                        ServiceMsg::Doc { .. } | ServiceMsg::DocPatch { .. } | ServiceMsg::DocSaved { .. } | ServiceMsg::BusEvent { .. } | ServiceMsg::Image { .. } | ServiceMsg::Placements { .. }
                    );
                return if bulk { self.push_bulk(Outgoing::Message(message), weight) } else { self.push_control(Outgoing::Message(message), weight) };
            }
        }
        Push::Queued
    }

    fn push_control(&mut self, item: Outgoing, weight: usize) -> Push {
        if self.control.len() >= CONTROL_LIMIT || (self.control_bytes > 0 && self.control_bytes + weight > CONTROL_BYTES) {
            return Push::Full;
        }
        self.control_bytes += weight;
        self.control.push_back(Entry::Item(item, weight));
        Push::Queued
    }

    fn push_bulk(&mut self, item: Outgoing, weight: usize) -> Push {
        if self.bulk.len() >= BULK_LIMIT || (self.bulk_bytes > 0 && self.bulk_bytes + weight > BULK_BYTES) {
            return Push::Full;
        }
        self.bulk_bytes += weight;
        self.bulk.push_back((Instant::now(), Entry::Item(item, weight)));
        Push::Queued
    }

    /// Records the newest info (or the end) of a terminal: an info of an
    /// earlier run (older `started_ms`) never replaces a newer one.
    fn merge_term(&mut self, id: String, state: TermState) {
        match self.terms.iter_mut().find(|(known, _)| *known == id) {
            Some((_, TermState::Info(was))) if matches!(&state, TermState::Info(new) if new.started_ms < was.started_ms) => {}
            Some((_, old)) => *old = state,
            None => self.terms.push((id, state)),
        }
    }

    fn ensure_terms(&mut self) {
        if !self.terms_queued {
            self.terms_queued = true;
            self.control.push_back(Entry::Terminals);
        }
    }

    fn latest(&mut self, kind: Latest, message: ServiceMsg, bulk: bool) {
        if self.latest.insert(kind, message).is_none() {
            if bulk {
                self.bulk.push_back((Instant::now(), Entry::Latest(kind)));
            } else {
                self.control.push_back(Entry::Latest(kind));
            }
        }
    }

    /// The message an entry stands for now (`None`: nothing left of it).
    fn materialize(&mut self, entry: Entry) -> Option<Outgoing> {
        match entry {
            // Its bytes were given back by its lane.
            Entry::Item(item, _) => Some(item),
            Entry::Terminals => {
                self.terms_queued = false;
                let list = std::mem::take(&mut self.terms_list);
                let mut infos = Vec::new();
                let mut gone = Vec::new();
                for (id, state) in std::mem::take(&mut self.terms) {
                    match state {
                        TermState::Info(info) => infos.push(info),
                        TermState::Gone => gone.push(id),
                    }
                }
                // Terminals that went away follow the list, next in the lane.
                // A list with no terminal is still sent: a client that just
                // connected replaces what it knew with it (a restarted
                // service with nothing running yet).
                let mut messages: Vec<ServiceMsg> = Vec::new();
                if !infos.is_empty() || list {
                    messages.push(ServiceMsg::Terminals(infos));
                }
                messages.extend(gone.into_iter().map(ServiceMsg::TerminalGone));
                let mut messages = messages.into_iter();
                let first = messages.next();
                for message in messages.rev() {
                    self.control.push_front(Entry::Item(Outgoing::Message(message), 0));
                }
                first.map(Outgoing::Message)
            }
            Entry::Sessions => self.sessions.take().map(|s| Outgoing::Message(ServiceMsg::Sessions { full: s.full, upsert: s.upsert, remove: s.remove })),
            Entry::Latest(kind) => self.latest.remove(&kind).map(Outgoing::Message),
        }
    }

    fn take(&mut self, now: Instant) -> Option<Taken> {
        self.retry_at = None;
        if self.window.as_ref().is_none_or(|w| w.allows(true)) {
            while let Some(entry) = self.control.pop_front() {
                if let Entry::Item(_, weight) = &entry {
                    self.control_bytes = self.control_bytes.saturating_sub(*weight);
                }
                if let Some(message) = self.materialize(entry) {
                    return Some(Taken { message, frame_of: None, urgent: true });
                }
            }
            if let Some(tile) = self.focused.clone().filter(|tile| self.frames.contains_key(tile)) {
                return self.take_frame(&tile, true);
            }
        }
        if !self.window.as_ref().is_none_or(|w| w.allows(false)) {
            return None;
        }
        if let Some(at) = self.pace.as_ref().and_then(|p| p.blocked(now)) {
            self.retry_at = Some(at);
            return None;
        }
        let overdue = self.bulk.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) >= BULK_WAIT);
        if !overdue && let Some(tile) = self.frame_order.front().cloned() {
            return self.take_frame(&tile, false);
        }
        while let Some((_, entry)) = self.bulk.pop_front() {
            if let Entry::Item(_, weight) = &entry {
                self.bulk_bytes = self.bulk_bytes.saturating_sub(*weight);
            }
            if let Some(message) = self.materialize(entry) {
                return Some(Taken { message, frame_of: None, urgent: false });
            }
        }
        let tile = self.frame_order.front().cloned()?;
        self.take_frame(&tile, false)
    }

    fn take_frame(&mut self, tile: &str, urgent: bool) -> Option<Taken> {
        self.frame_order.retain(|t| t != tile);
        let message = self.frames.remove(tile)?;
        self.writing = Some(tile.to_owned());
        Some(Taken { message: Outgoing::Message(message), frame_of: Some(tile.to_owned()), urgent })
    }
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a message (shaped for the client already). Frames from the
    /// pump go through [`Outbox::push_frame`].
    pub fn push(&self, message: ServiceMsg) -> Push {
        let mut lanes = lock(&self.lanes);
        if lanes.closed {
            return Push::Closed;
        }
        let pushed = lanes.push(message);
        drop(lanes);
        if pushed == Push::Queued {
            self.ready.notify_one();
        }
        pushed
    }

    /// Queues the frame of `tile`: `Full` while one of it still waits or is being written.
    pub fn push_frame(&self, tile: &str, frame: ServiceMsg) -> Push {
        let mut lanes = lock(&self.lanes);
        if lanes.closed {
            return Push::Closed;
        }
        if lanes.frames.contains_key(tile) || lanes.writing.as_deref() == Some(tile) {
            return Push::Full;
        }
        lanes.frames.insert(tile.to_owned(), frame);
        lanes.frame_order.push_back(tile.to_owned());
        drop(lanes);
        self.ready.notify_one();
        Push::Queued
    }

    /// Queues an image with the bulk and counts it against the image budget
    /// until it is written. `Full`: over the bulk budget (it waits).
    pub fn push_image(&self, image: Outgoing, len: usize) -> Push {
        let mut lanes = lock(&self.lanes);
        if lanes.closed {
            return Push::Closed;
        }
        let pushed = lanes.push_bulk(image, len + 64);
        if pushed == Push::Queued {
            // Counted before the writer can take it (it takes under this lock).
            self.image_bytes.fetch_add(len, Ordering::AcqRel);
        }
        drop(lanes);
        if pushed == Push::Queued {
            self.ready.notify_one();
        }
        pushed
    }

    /// Bytes of images queued and not written yet.
    pub fn image_bytes(&self) -> usize {
        self.image_bytes.load(Ordering::Acquire)
    }

    /// The writer wrote an image of `len` bytes.
    pub fn image_written(&self, len: usize) {
        let _ = self.image_bytes.try_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(len)));
    }

    /// Whether a frame of `tile` waits or is being written, and when its last one was written.
    pub fn frame_state(&self, tile: &str) -> (bool, Option<Instant>) {
        let lanes = lock(&self.lanes);
        (lanes.frames.contains_key(tile) || lanes.writing.as_deref() == Some(tile), lanes.written.get(tile).copied())
    }

    /// The writer wrote the frame of `tile` it took.
    pub fn frame_written(&self, tile: &str) {
        let mut lanes = lock(&self.lanes);
        if lanes.writing.as_deref() == Some(tile) {
            lanes.writing = None;
        }
        lanes.written.insert(tile.to_owned(), Instant::now());
    }

    /// The focused tile (its frames go first).
    pub fn set_focused(&self, tile: Option<String>) {
        lock(&self.lanes).focused = tile;
    }

    /// Forgets the frames of tiles the client no longer shows.
    pub fn retain_tiles(&self, shown: &[String]) {
        let mut lanes = lock(&self.lanes);
        lanes.frames.retain(|tile, _| shown.contains(tile));
        lanes.frame_order.retain(|tile| shown.contains(tile));
        lanes.written.retain(|tile, _| shown.contains(tile));
    }

    /// The next message to write, waiting until `deadline` for one (one
    /// that flow control lets go: [`Window`], [`Pace`]).
    pub fn next(&self, deadline: Instant) -> Next {
        let mut lanes = lock(&self.lanes);
        loop {
            if lanes.closed {
                return Next::Closed;
            }
            let now = Instant::now();
            if let Some(taken) = lanes.take(now) {
                return Next::Item(taken);
            }
            if now >= deadline {
                return Next::Timeout;
            }
            let until = lanes.retry_at.map_or(deadline, |at| at.min(deadline)).max(now);
            lanes = self.ready.wait_timeout(lanes, until - now).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// The writer wrote `bytes` (`urgent`: control, the focused frame or a heartbeat).
    pub fn wrote(&self, bytes: usize, urgent: bool) {
        let now = Instant::now();
        let mut lanes = lock(&self.lanes);
        if let Some(window) = &mut lanes.window {
            window.wrote(bytes as u64, now);
        }
        if !urgent && let Some(pace) = &mut lanes.pace {
            pace.charge(bytes as u64, now);
        }
    }

    /// The client was told to acknowledge (`ServiceMsg::Caps` was written):
    /// flow control counts from the next byte on, and the stopgap ends.
    pub fn start_window(&self) {
        let mut lanes = lock(&self.lanes);
        lanes.window = Some(Window::default());
        lanes.pace = None;
    }

    /// The client acknowledged `total` bytes since `ServiceMsg::Caps`.
    pub fn acked(&self, total: u64) {
        let mut lanes = lock(&self.lanes);
        if let Some(window) = &mut lanes.window {
            window.acked(total, Instant::now());
            drop(lanes);
            self.ready.notify_one();
        }
    }

    /// A remote client that does not acknowledge: the stopgap pacing applies to it while it types.
    pub fn set_paced(&self) {
        let mut lanes = lock(&self.lanes);
        if lanes.window.is_none() {
            lanes.pace = Some(Pace { typed_at: None, free_at: Instant::now() });
        }
    }

    /// The client typed (its echo is due: the stopgap holds back what is not urgent).
    pub fn typed(&self) {
        if let Some(pace) = &mut lock(&self.lanes).pace {
            pace.typed_at = Some(Instant::now());
        }
    }

    /// Other tiles' frames are slowed down: a client under the stopgap typed recently.
    pub fn slow_others(&self) -> bool {
        lock(&self.lanes).pace.as_ref().is_some_and(|p| p.typing(Instant::now()))
    }

    /// Bytes unacknowledged and the budget (`None`: no flow control).
    pub fn flow(&self) -> Option<(u64, u64)> {
        lock(&self.lanes).window.as_ref().map(|w| (w.in_flight(), w.budget()))
    }

    /// Nothing more is written or queued.
    pub fn close(&self) {
        lock(&self.lanes).closed = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.lanes).closed
    }

    /// A lane overflowed: closes the outbox and keeps why (once).
    pub fn overflowed(&self, why: impl FnOnce(&Waiting) -> String) {
        let mut lanes = lock(&self.lanes);
        if lanes.overflow.is_none() {
            let waiting = lanes.waiting();
            lanes.overflow = Some(why(&waiting));
        }
        lanes.closed = true;
        drop(lanes);
        self.ready.notify_all();
    }

    /// Why the client is dropped, when a lane overflowed.
    pub fn overflow(&self) -> Option<String> {
        lock(&self.lanes).overflow.clone()
    }

    /// What waits now.
    pub fn waiting(&self) -> Waiting {
        lock(&self.lanes).waiting()
    }
}

/// What waits in an outbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Waiting {
    pub control: usize,
    pub frames: usize,
    pub bulk: usize,
    /// Bytes of control and bulk messages (about).
    pub bytes: usize,
}

impl Lanes {
    fn waiting(&self) -> Waiting {
        Waiting { control: self.control.len(), frames: self.frames.len(), bulk: self.bulk.len(), bytes: self.control_bytes + self.bulk_bytes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame8, ServiceStats};

    fn frame(tile: &str, rows: u16) -> ServiceMsg {
        ServiceMsg::Frame8(Frame8 {
            id: tile.into(),
            cols: 10,
            rows,
            full: true,
            changed: Vec::new(),
            cursor: None,
            cursor_shape: 0,
            cursor_blink: false,
            display_offset: 0,
            history: 0,
            modes: 0,
            links: Vec::new(),
        })
    }

    fn info(id: &str, alive: bool, started_ms: u64) -> TermInfo {
        TermInfo { id: id.into(), alive, started_ms, ..Default::default() }
    }

    fn session(id: &str, name: &str) -> SessionSummary {
        SessionSummary { id: id.into(), name: name.into(), ..Default::default() }
    }

    /// Everything the writer would take now, in order (frames marked written at once).
    fn drain(outbox: &Outbox) -> Vec<ServiceMsg> {
        let mut out = Vec::new();
        while let Next::Item(Taken { message, frame_of, .. }) = outbox.next(Instant::now()) {
            if let Some(tile) = frame_of {
                outbox.frame_written(&tile);
            }
            match message {
                Outgoing::Message(message) => out.push(message),
                Outgoing::Image { image_id, .. } => out.push(ServiceMsg::Placements { term: format!("image {image_id}"), placements: Vec::new() }),
            }
        }
        out
    }

    fn kind(message: &ServiceMsg) -> String {
        match message {
            ServiceMsg::Frame8(f) => format!("frame {}", f.id),
            ServiceMsg::RpcReply { id, .. } => format!("reply {id}"),
            ServiceMsg::Error { message, .. } => format!("error {message}"),
            ServiceMsg::Doc { key, .. } => format!("doc {key}"),
            ServiceMsg::Bus { json } => format!("bus {json}"),
            ServiceMsg::Terminals(infos) => format!("terminals {}", infos.iter().map(|i| format!("{}{}", i.id, if i.alive { "+" } else { "-" })).collect::<Vec<_>>().join(",")),
            ServiceMsg::TerminalGone(id) => format!("gone {id}"),
            ServiceMsg::Stats(stats) => format!("stats {}", stats.alive),
            ServiceMsg::Ready8 { .. } => "ready".into(),
            other => format!("{other:?}"),
        }
    }

    fn kinds(outbox: &Outbox) -> Vec<String> {
        drain(outbox).iter().map(kind).collect()
    }

    #[test]
    fn an_echo_goes_before_bulk_and_other_tiles_but_after_control() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        // A backlog: large replies and a document, then other tiles' frames.
        for id in 0..3 {
            outbox.push(ServiceMsg::RpcReply { id, json: "x".repeat(200 << 10) });
        }
        outbox.push(ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() });
        assert_eq!(outbox.push_frame("busy", frame("busy", 1)), Push::Queued);
        assert_eq!(outbox.push_frame("echo", frame("echo", 1)), Push::Queued);
        outbox.push(ServiceMsg::Error { id: None, message: "e".into() });
        assert_eq!(kinds(&outbox), ["error e", "frame echo", "frame busy", "reply 0", "reply 1", "reply 2", "doc k"]);
    }

    #[test]
    fn bulk_that_waited_goes_before_other_tiles_frames_never_before_the_focused_one() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.push(ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() });
        std::thread::sleep(BULK_WAIT + Duration::from_millis(20));
        outbox.push_frame("busy", frame("busy", 1));
        outbox.push_frame("echo", frame("echo", 1));
        assert_eq!(kinds(&outbox), ["frame echo", "doc k", "frame busy"]);
    }

    #[test]
    fn one_frame_per_tile_waits_until_it_is_written() {
        let outbox = Outbox::new();
        assert_eq!(outbox.push_frame("t", frame("t", 1)), Push::Queued);
        assert_eq!(outbox.push_frame("t", frame("t", 2)), Push::Full, "the first still waits");
        assert_eq!(outbox.frame_state("t"), (true, None));
        let Next::Item(Taken { frame_of, .. }) = outbox.next(Instant::now()) else { panic!("a frame") };
        assert_eq!(frame_of.as_deref(), Some("t"));
        assert!(outbox.frame_state("t").0, "being written is still pending");
        assert_eq!(outbox.push_frame("t", frame("t", 2)), Push::Full);
        outbox.frame_written("t");
        let (pending, written) = outbox.frame_state("t");
        assert!(!pending && written.is_some(), "pacing counts from the write");
        assert_eq!(outbox.push_frame("t", frame("t", 2)), Push::Queued);
        // A tile no longer shown loses its waiting frame.
        outbox.retain_tiles(&[]);
        assert!(drain(&outbox).is_empty());
    }

    #[test]
    fn terminal_state_is_coalesced_in_place_and_never_lost() {
        let outbox = Outbox::new();
        outbox.push(ServiceMsg::Ready8 { pid: 1, input_ack: 0 });
        outbox.push(ServiceMsg::Terminals(vec![info("a", true, 10), info("b", true, 10)]));
        outbox.push(ServiceMsg::Stats(ServiceStats { alive: 2, ..Default::default() }));
        // Later updates join the list waiting ahead of the answer to Stats.
        outbox.push(ServiceMsg::Terminals(vec![info("c", true, 10)]));
        outbox.push(ServiceMsg::Terminals(vec![info("a", false, 10)]));
        outbox.push(ServiceMsg::TerminalGone("b".into()));
        // A stale exit of an earlier run of `c` arrives after its relaunch's info.
        outbox.push(ServiceMsg::Terminals(vec![info("c", false, 5)]));
        outbox.push(ServiceMsg::Stats(ServiceStats { alive: 1, ..Default::default() }));
        assert_eq!(kinds(&outbox), ["ready", "terminals a-,c+", "gone b", "stats 1"]);
        // Once written, the next update starts a new message.
        outbox.push(ServiceMsg::Terminals(vec![info("b", true, 20)]));
        assert_eq!(kinds(&outbox), ["terminals b+"]);
    }

    #[test]
    fn session_updates_merge_and_a_full_list_replaces_what_waits() {
        let outbox = Outbox::new();
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("x", "1"), session("y", "1")], remove: vec!["z".into()] });
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("x", "2"), session("z", "1")], remove: vec!["y".into()] });
        let out = drain(&outbox);
        let [ServiceMsg::Sessions { full, upsert, remove }] = out.as_slice() else { panic!("one message: {out:?}") };
        assert!(!full);
        assert_eq!(upsert.iter().map(|s| format!("{}{}", s.id, s.name)).collect::<Vec<_>>(), ["x2", "z1"]);
        assert_eq!(remove, &["y".to_owned()]);
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("x", "3")], remove: Vec::new() });
        outbox.push(ServiceMsg::Sessions { full: true, upsert: vec![session("w", "1")], remove: Vec::new() });
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("v", "1")], remove: Vec::new() });
        let out = drain(&outbox);
        let [ServiceMsg::Sessions { full, upsert, .. }] = out.as_slice() else { panic!("one message: {out:?}") };
        assert!(full, "still a full list, with what came after it");
        assert_eq!(upsert.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["w", "v"]);
    }

    #[test]
    fn a_newer_bus_snapshot_replaces_one_still_waiting() {
        let outbox = Outbox::new();
        outbox.push(ServiceMsg::Bus { json: "1".into() });
        outbox.push(ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() });
        outbox.push(ServiceMsg::Bus { json: "2".into() });
        assert_eq!(kinds(&outbox), ["bus 2", "doc k"]);
    }

    #[test]
    fn lanes_are_bounded_and_one_large_message_always_fits() {
        let outbox = Outbox::new();
        let huge = ServiceMsg::RpcReply { id: 1, json: "x".repeat(BULK_BYTES + 1) };
        assert_eq!(outbox.push(huge), Push::Queued, "one message fits an empty lane");
        assert_eq!(outbox.push(ServiceMsg::RpcReply { id: 2, json: "x".repeat(SMALL + 1) }), Push::Full, "the bulk lane is over its budget");
        assert_eq!(outbox.push(ServiceMsg::Error { id: None, message: "small".into() }), Push::Queued, "control has its own budget");
        let outbox = Outbox::new();
        for _ in 0..CONTROL_LIMIT {
            assert_eq!(outbox.push(ServiceMsg::Clipboard("c".into())), Push::Queued);
        }
        assert_eq!(outbox.push(ServiceMsg::Clipboard("c".into())), Push::Full);
        // Coalesced state still fits: it takes no new place once waiting.
        assert_eq!(outbox.push(ServiceMsg::Terminals(vec![info("a", true, 1)])), Push::Queued);
        outbox.overflowed(|w| format!("{} control messages waiting", w.control));
        assert_eq!(outbox.overflow().as_deref(), Some("65537 control messages waiting"));
        assert!(matches!(outbox.next(Instant::now()), Next::Closed));
        assert_eq!(outbox.push(ServiceMsg::Clipboard("c".into())), Push::Closed);
    }

    #[test]
    fn the_writer_waits_for_work_or_its_deadline() {
        let outbox = Arc::new(Outbox::new());
        let started = Instant::now();
        assert!(matches!(outbox.next(started + Duration::from_millis(50)), Next::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(50));
        let pusher = outbox.clone();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            pusher.push(ServiceMsg::Clipboard("late".into()));
        });
        assert!(matches!(outbox.next(Instant::now() + Duration::from_secs(5)), Next::Item(_)), "woken by the push");
        late.join().unwrap();
        outbox.close();
        assert!(matches!(outbox.next(Instant::now() + Duration::from_secs(5)), Next::Closed));
    }

    #[test]
    fn an_empty_terminal_list_is_sent() {
        // A restarted service with nothing running yet: the connecting client
        // must learn that the terminals it knew are gone (R2-E-01).
        let outbox = Outbox::new();
        outbox.push(ServiceMsg::Ready8 { pid: 1, input_ack: 0 });
        outbox.push(ServiceMsg::Terminals(Vec::new()));
        outbox.push(ServiceMsg::Stats(ServiceStats::default()));
        assert_eq!(kinds(&outbox), ["ready", "terminals ", "stats 0"]);
        // An empty list joined by an end still says both.
        outbox.push(ServiceMsg::Terminals(Vec::new()));
        outbox.push(ServiceMsg::TerminalGone("a".into()));
        assert_eq!(kinds(&outbox), ["terminals ", "gone a"]);
        // An end alone is not a list.
        outbox.push(ServiceMsg::TerminalGone("b".into()));
        assert_eq!(kinds(&outbox), ["gone b"]);
    }

    /// Takes what the writer would take now and says it was written (`bytes` each).
    fn write_all(outbox: &Outbox, bytes: usize) -> Vec<String> {
        let mut out = Vec::new();
        while let Next::Item(Taken { message, frame_of, urgent }) = outbox.next(Instant::now()) {
            outbox.wrote(bytes, urgent);
            if let Some(tile) = &frame_of {
                outbox.frame_written(tile);
            }
            if let Outgoing::Message(message) = message {
                out.push(kind(&message));
            }
        }
        out
    }

    #[test]
    fn unacknowledged_bytes_hold_bulk_and_other_tiles_but_not_the_echo() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.start_window();
        for id in 0..8 {
            outbox.push(ServiceMsg::RpcReply { id, json: "x".repeat(100 << 10) });
        }
        // The budget starts at WINDOW_START: two 100 KB replies go, then nothing until an acknowledgement.
        assert_eq!(write_all(&outbox, 100 << 10), ["reply 0", "reply 1"]);
        assert_eq!(outbox.flow(), Some((200 << 10, WINDOW_START)));
        outbox.push_frame("busy", frame("busy", 1));
        assert!(write_all(&outbox, 1000).is_empty(), "another tile's frame waits too");
        outbox.retain_tiles(&["echo".into()]);
        // The echo and control still go, up to URGENT_EXTRA more.
        outbox.push_frame("echo", frame("echo", 1));
        outbox.push(ServiceMsg::Error { id: None, message: "e".into() });
        assert_eq!(write_all(&outbox, 1000), ["error e", "frame echo"]);
        outbox.push_frame("echo", frame("echo", 2));
        outbox.push(ServiceMsg::Error { id: None, message: "big".into() });
        assert_eq!(write_all(&outbox, URGENT_EXTRA as usize), ["error big"], "past the urgent limit even the echo waits");
        // Acknowledgements open it again, in order: the frame first.
        outbox.acked(400 << 10);
        assert_eq!(write_all(&outbox, 100 << 10), ["frame echo", "reply 2"]);
        outbox.acked(u64::MAX);
        let (in_flight, _) = outbox.flow().unwrap();
        assert_eq!(in_flight, 0, "an acknowledgement never counts more than was written");
    }

    #[test]
    fn one_message_always_goes_when_nothing_is_in_flight() {
        let outbox = Outbox::new();
        outbox.start_window();
        outbox.push(ServiceMsg::RpcReply { id: 1, json: "x".repeat(4 << 20) });
        outbox.push(ServiceMsg::RpcReply { id: 2, json: "x".repeat(SMALL + 1) });
        assert_eq!(write_all(&outbox, 4 << 20), ["reply 1"]);
        outbox.acked(4 << 20);
        assert_eq!(write_all(&outbox, SMALL + 1), ["reply 2"]);
    }

    #[test]
    fn the_writer_is_woken_by_an_acknowledgement() {
        let outbox = Arc::new(Outbox::new());
        outbox.start_window();
        outbox.push(ServiceMsg::RpcReply { id: 1, json: "x".repeat(SMALL + 1) });
        outbox.push(ServiceMsg::RpcReply { id: 2, json: "x".repeat(SMALL + 1) });
        assert_eq!(write_all(&outbox, WINDOW_MAX as usize), ["reply 1"]);
        let acker = outbox.clone();
        let ack = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            acker.acked(WINDOW_MAX);
        });
        let started = Instant::now();
        assert!(matches!(outbox.next(Instant::now() + Duration::from_secs(5)), Next::Item(_)));
        assert!(started.elapsed() < Duration::from_secs(2), "woken by the acknowledgement");
        ack.join().unwrap();
    }

    /// A link of `rate` bytes per second and `rtt` round trip behind a buffer
    /// that takes everything (a tunnel): the writer sends 64 KB of bulk
    /// whenever the window lets it, the client acknowledges each message as
    /// it arrives. Returns (the budget at the end, the longest a byte waited
    /// in the link's queue beyond the round trip).
    fn simulate(rate: f64, rtt: Duration, seconds: u64) -> (u64, Duration) {
        let mut window = Window::default();
        let start = Instant::now();
        let mut now = start;
        // (the acknowledgement reaches the service, bytes written up to it).
        let mut flight: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut link_free = start;
        let mut worst = Duration::ZERO;
        let step = Duration::from_millis(1);
        while now < start + Duration::from_secs(seconds) {
            while flight.front().is_some_and(|(acked, _)| *acked <= now) {
                let (_, total) = flight.pop_front().unwrap();
                window.acked(total, now);
            }
            while window.allows(false) {
                let size = 64 << 10;
                window.wrote(size, now);
                let begins = link_free.max(now + rtt / 2);
                let arrives = begins + Duration::from_secs_f64(size as f64 / rate);
                link_free = arrives;
                if now > start + Duration::from_secs(seconds / 2) {
                    worst = worst.max(arrives.saturating_duration_since(now) - rtt / 2);
                }
                flight.push_back((arrives + rtt / 2, window.written));
            }
            now += step;
        }
        (window.budget(), worst)
    }

    #[test]
    fn the_budget_follows_the_link() {
        // A slow link (200 KB/s): an echo waits about FLOW_TARGET plus one message behind bulk.
        let (budget, worst) = simulate(200e3, Duration::from_millis(60), 20);
        assert!(budget <= 64 << 10, "slow link: budget {budget}");
        assert!(worst < Duration::from_millis(700), "slow link: bulk queued {worst:?} ahead of an echo");
        // A fast link far away (20 MB/s, 120 ms): the window opens to its maximum.
        let (budget, _) = simulate(20e6, Duration::from_millis(120), 10);
        assert_eq!(budget, WINDOW_MAX, "fast link: budget {budget}");
        // 1 MB/s, as the faults step: under half a second of queue.
        let (budget, worst) = simulate(1e6, Duration::from_millis(20), 20);
        assert!((WINDOW_MIN..=WINDOW_MAX).contains(&budget));
        assert!(worst < Duration::from_millis(500), "1 MB/s: {worst:?}");
    }

    #[test]
    fn clients_that_do_not_acknowledge_get_bulk_paced_while_they_type() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.set_paced();
        for id in 0..4 {
            outbox.push(ServiceMsg::RpcReply { id, json: "x".repeat(SMALL + 1) });
        }
        // Not typing: everything goes at once.
        assert_eq!(write_all(&outbox, 64 << 10).len(), 4);
        assert!(!outbox.slow_others());
        outbox.typed();
        assert!(outbox.slow_others(), "other tiles are slowed down");
        for id in 4..8 {
            outbox.push(ServiceMsg::RpcReply { id, json: "x".repeat(SMALL + 1) });
        }
        // While typing: one burst, then about TYPING_RATE.
        assert_eq!(write_all(&outbox, 64 << 10), ["reply 4"]);
        outbox.push_frame("echo", frame("echo", 1));
        assert_eq!(write_all(&outbox, 1000), ["frame echo"], "the echo is never paced");
        let started = Instant::now();
        let Next::Item(Taken { urgent: false, .. }) = outbox.next(Instant::now() + Duration::from_secs(5)) else { panic!("the next reply") };
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(200), "paced: {waited:?}");
        // A client told to acknowledge leaves the stopgap.
        outbox.start_window();
        assert!(!outbox.slow_others());
    }
}
