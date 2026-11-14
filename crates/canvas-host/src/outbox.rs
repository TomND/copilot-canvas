//! What one client is sent, waiting for its writer thread
//! (`server::spawn_writer`). Not one FIFO: a keystroke's echo must not wait
//! behind a backlog of frames, replies or snapshots. The writer takes from
//! five lanes, in this order:
//!
//! 1. **Control**, in order: `Ready8`, errors, small replies (up to
//!    [`REPLY_SMALL`]), notices, and the terminal and session state. State is
//!    coalesced, never dropped: a
//!    `Terminals` update joins the one already waiting (the newest info per
//!    terminal; a stale info of an earlier run of it never replaces a newer
//!    one; `TerminalGone` replaces it), `Sessions` upserts and removals merge
//!    (a full list replaces what waits), and `Stats` or a tunnel token
//!    replaces the one waiting. A coalesced message keeps the place of its
//!    first part, so the lane's order holds (the terminal list a client gets
//!    on connect comes before the answer to its first `Stats`).
//! 2. **The focused tile's frame** (`Subscribe8::focused`).
//! 3. **Replies** larger than [`REPLY_SMALL`] and up to [`SMALL`], in order:
//!    they never go ahead of the echo (R3-A-02).
//! 4. **Other tiles' frames**, oldest first. At most one frame per tile waits:
//!    the pump makes the next one once the writer wrote it, from the screen
//!    then, so a slow link gets fewer, newer frames instead of a queue.
//! 5. **Bulk**, in order: documents and their patches, bus snapshots (a newer
//!    one replaces one still waiting), the reports inbox, large replies,
//!    images and their placements, a whole session list larger than
//!    [`SMALL`]. Bulk that has waited [`BULK_WAIT`] goes before other tiles'
//!    frames, so frames never starve it.
//!
//! A reply or bulk message that has waited [`BULK_WAIT`] also takes the
//! focused frame's turn once per [`BULK_WAIT`], so a tile that prints
//! without pause cannot starve them (R3-A-02). While the client types (in
//! the last [`TYPING_SPAN`]), a bulk message larger than what flow control
//! lets be in flight waits (at most [`BULK_HOLD`], one such message per
//! [`BULK_HOLD`]): once written it would hold every echo until it is
//! through, and it cannot be split (R3-C-01).
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
//! within [`WINDOW_MIN`]..[`WINDOW_MAX`]); control may have
//! [`URGENT_EXTRA`] more in flight, and so may the focused tile's frame once
//! its previous one was acknowledged (one focused frame beyond the budget at
//! most: a tile that prints fast never fills the tunnel ahead of its own
//! echo, R3-C-01). One message always goes when nothing is in flight.
//! Remote clients that do not acknowledge (older builds, the phone) get a
//! stopgap instead ([`Pace`]): while they typed in the last [`TYPING_SPAN`],
//! bulk goes at most [`TYPING_RATE`] bytes per second (other tiles' frames
//! are paced by the pump).

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
/// A reply larger than this waits in the replies lane, behind the focused frame.
pub const REPLY_SMALL: usize = 4 << 10;
/// Bulk that waited this long goes before other tiles' frames (and takes the
/// focused frame's turn once per this long).
pub const BULK_WAIT: Duration = Duration::from_millis(250);
/// While the client types, a bulk message too large to be in flight at once
/// waits at most this long, and one such message goes per this long.
pub const BULK_HOLD: Duration = Duration::from_secs(2);
/// Bytes other tiles' frames and bulk may have unacknowledged, at least and
/// at most (under a Dev Tunnel's 1 MiB window with [`URGENT_EXTRA`], R3-A-05).
pub const WINDOW_MIN: u64 = 32 << 10;
pub const WINDOW_MAX: u64 = 768 << 10;
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
/// After the client sent a large message (an upload), its acknowledgements
/// arrive late, all at once: they say nothing about the link for this long.
const UPLINK_QUIET: Duration = Duration::from_secs(1);
/// An acknowledgement may be ahead of the writer's count by the message it
/// is writing (pieces of a split frame included); never by more than this.
const AHEAD_MAX: u64 = 4 * crate::protocol::MAX_FRAME as u64;
/// Stopgap pacing (clients that do not acknowledge): while the client typed
/// this recently, other tiles' frames and bulk go at most `TYPING_RATE`
/// bytes per second, in bursts of `TYPING_BURST`.
pub const TYPING_SPAN: Duration = Duration::from_secs(2);
pub const TYPING_RATE: u64 = 128 << 10;
const TYPING_BURST: u64 = 32 << 10;
/// What one large message may hold the stopgap's bulk back at most.
const TYPING_DEBT: Duration = Duration::from_secs(1);

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

/// The lane a message was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Control,
    Focused,
    Reply,
    Frame,
    Bulk,
}

/// One message for the writer; `frame_of`: the tile whose frame it is.
pub struct Taken {
    pub message: Outgoing,
    pub frame_of: Option<String>,
    pub lane: Lane,
}

impl Taken {
    /// Control or the focused tile's frame (flow control lets more of those be in flight).
    pub fn urgent(&self) -> bool {
        matches!(self.lane, Lane::Control | Lane::Focused)
    }
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
    /// Where the focused tile's last frame ends (bytes written up to it).
    focused_end: u64,
    /// Until when acknowledgements do not measure the link (the client's
    /// upload held them back: [`UPLINK_QUIET`]).
    quiet_until: Option<Instant>,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            written: 0,
            acked: 0,
            budget: WINDOW_START,
            sent: VecDeque::new(),
            alone: VecDeque::new(),
            rate: None,
            samples: VecDeque::new(),
            focused_end: 0,
            quiet_until: None,
        }
    }
}

impl Window {
    /// Bytes written and not acknowledged (an acknowledgement may overtake
    /// the writer's own count of a message: R3-A-04).
    pub fn in_flight(&self) -> u64 {
        self.written.saturating_sub(self.acked)
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Whether a message may be written now (`urgent`: control).
    pub fn allows(&self, urgent: bool) -> bool {
        let in_flight = self.in_flight();
        in_flight == 0 || in_flight < self.budget + if urgent { URGENT_EXTRA } else { 0 }
    }

    /// Whether the focused tile's frame may be written now: within the
    /// budget, or beyond it (up to [`URGENT_EXTRA`]) once its previous frame
    /// was acknowledged, so at most one of its frames is in flight beyond
    /// the budget and its echo never queues behind its own output (R3-C-01).
    pub fn allows_focused(&self) -> bool {
        let in_flight = self.in_flight();
        in_flight == 0 || in_flight < self.budget || (self.acked >= self.focused_end && in_flight < self.budget + URGENT_EXTRA)
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
        // An acknowledgement that overtook this count already covers it.
        if self.written > self.acked {
            self.sent.push_back((self.written, bytes, now, alone));
        }
    }

    /// The focused tile's frame of `bytes` was written at `now`.
    pub fn wrote_focused(&mut self, bytes: u64, now: Instant) {
        self.wrote(bytes, now);
        self.focused_end = self.written;
    }

    /// The client's reader got a large message from it: its acknowledgements
    /// waited behind it and come all at once, which says nothing about the link (R3-A-06).
    pub fn uplink_busy(&mut self, now: Instant) {
        self.quiet_until = Some(now + UPLINK_QUIET);
        self.samples.clear();
    }

    /// The round trip without queueing: the shortest one of a message that
    /// had nothing ahead of it, less its own transfer at the measured rate.
    fn base_rtt(&self) -> Duration {
        let transfer = |size: u64| self.rate.map_or(Duration::ZERO, |rate| Duration::from_secs_f64(size as f64 / rate));
        self.alone.iter().map(|(rtt, size)| rtt.saturating_sub(transfer(*size))).min().unwrap_or_default()
    }

    /// The client acknowledged `total` bytes at `now`. It may be ahead of
    /// what the writer counted so far (the client read a message before the
    /// writer noted it written): kept, never clamped away (R3-A-04).
    pub fn acked(&mut self, total: u64, now: Instant) {
        // Only the message being written can be ahead (a bogus total is bounded).
        let total = total.min(self.written.saturating_add(AHEAD_MAX));
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
        // Acknowledgements an upload held back measure nothing.
        if self.quiet_until.is_some_and(|until| now < until) {
            self.samples.clear();
            return;
        }
        self.quiet_until = None;
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
/// typed in the last [`TYPING_SPAN`], bulk leaves at most [`TYPING_RATE`]
/// bytes per second (a token bucket kept as the time the next byte may go).
/// Other tiles' frames are not charged: the pump paces them (4 a second) on
/// their own, so a large transfer never freezes them (R3-A-09); and what a
/// large message costs is never owed for more than [`TYPING_DEBT`].
#[derive(Debug)]
struct Pace {
    free_at: Instant,
}

impl Pace {
    /// `None`: bulk may go now; else when to look again (`typed_at`: when the client last typed).
    fn blocked(&self, typed_at: Option<Instant>, now: Instant) -> Option<Instant> {
        if !typing(typed_at, now) || self.free_at <= now {
            return None;
        }
        let lifts = typed_at.map_or(now, |at| at + TYPING_SPAN);
        Some(self.free_at.min(lifts))
    }

    fn charge(&mut self, bytes: u64, typed_at: Option<Instant>, now: Instant) {
        if !typing(typed_at, now) {
            return;
        }
        let burst = Duration::from_secs_f64(TYPING_BURST as f64 / TYPING_RATE as f64);
        let free_at = self.free_at.max(now.checked_sub(burst).unwrap_or(now)) + Duration::from_secs_f64(bytes as f64 / TYPING_RATE as f64);
        self.free_at = free_at.min(now + TYPING_DEBT);
    }
}

/// Whether the client typed in the last [`TYPING_SPAN`].
fn typing(typed_at: Option<Instant>, now: Instant) -> bool {
    typed_at.is_some_and(|at| now.saturating_duration_since(at) < TYPING_SPAN)
}

#[derive(Default)]
struct Lanes {
    control: VecDeque<Entry>,
    control_bytes: usize,
    /// Replies over [`REPLY_SMALL`] (up to [`SMALL`]), with when each was queued.
    replies: VecDeque<(Instant, Outgoing, usize)>,
    /// With when each entry was queued.
    bulk: VecDeque<(Instant, Entry)>,
    bulk_bytes: usize,
    /// Terminal state waiting, by terminal id, and whether its `Entry::Terminals` is in `control`.
    terms: Vec<(String, TermState)>,
    terms_queued: bool,
    /// A terminal list was pushed (even an empty one: a client takes its first list as the whole of them).
    terms_list: bool,
    sessions: Option<SessionsWaiting>,
    /// The waiting `Entry::Sessions` is in the bulk lane (a large whole list).
    sessions_bulk: bool,
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
    /// When to look again at what the pace or the typing hold keeps back.
    retry_at: Option<Instant>,
    /// When the client last typed (its echo is due).
    typed_at: Option<Instant>,
    /// When a reply or bulk message last took the focused frame's turn.
    shared_at: Option<Instant>,
    /// Since when the large bulk message at the head of the lane has been
    /// held while the client types, and when one held went last.
    held_since: Option<Instant>,
    large_at: Option<Instant>,
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
                // A large whole list (every session indexed) waits with the bulk:
                // it never goes ahead of a focused frame (deltas that follow join it).
                let large = full && 64 + upsert.len() * 320 + remove.len() * 40 > SMALL;
                if large && self.sessions.is_some() && !self.sessions_bulk {
                    self.control.retain(|entry| !matches!(entry, Entry::Sessions));
                    self.sessions = None;
                }
                let waiting = match &mut self.sessions {
                    Some(waiting) => waiting,
                    None => {
                        self.sessions_bulk = large;
                        if large {
                            self.bulk.push_back((Instant::now(), Entry::Sessions));
                        } else {
                            self.control.push_back(Entry::Sessions);
                        }
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
                if bulk {
                    return self.push_bulk(Outgoing::Message(message), weight);
                }
                // A mid-sized reply (a file preview, a folder, a transcript) waits behind the echo.
                if weight > REPLY_SMALL && matches!(message, ServiceMsg::RpcReply { .. }) {
                    return self.push_reply(Outgoing::Message(message), weight);
                }
                return self.push_control(Outgoing::Message(message), weight);
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

    /// Replies count against the control lane's budget.
    fn push_reply(&mut self, item: Outgoing, weight: usize) -> Push {
        if self.control.len() + self.replies.len() >= CONTROL_LIMIT || (self.control_bytes > 0 && self.control_bytes + weight > CONTROL_BYTES) {
            return Push::Full;
        }
        self.control_bytes += weight;
        self.replies.push_back((Instant::now(), item, weight));
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
            Entry::Sessions => {
                self.sessions_bulk = false;
                self.sessions.take().map(|s| Outgoing::Message(ServiceMsg::Sessions { full: s.full, upsert: s.upsert, remove: s.remove }))
            }
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
                    return Some(Taken { message, frame_of: None, lane: Lane::Control });
                }
            }
        }
        let open = self.window.as_ref().is_none_or(|w| w.allows(false));
        let focused = self.focused.clone().filter(|tile| self.frames.contains_key(tile));
        // A reply or bulk message that waited takes the focused frame's turn
        // once in a while: a tile printing without pause never starves them.
        if open && focused.is_some() && self.shared_at.is_none_or(|at| now.saturating_duration_since(at) >= BULK_WAIT) {
            let reply_due = self.replies.front().is_some_and(|(at, ..)| now.saturating_duration_since(*at) >= BULK_WAIT);
            let bulk_due = self.bulk.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) >= BULK_WAIT) && self.bulk_may_go(now);
            if reply_due || bulk_due {
                self.shared_at = Some(now);
                if reply_due {
                    return self.take_reply();
                }
                if let Some(taken) = self.take_bulk(now) {
                    return Some(taken);
                }
            }
        }
        if let Some(tile) = focused
            && self.window.as_ref().is_none_or(Window::allows_focused)
        {
            return self.take_frame(&tile, Lane::Focused);
        }
        if !open {
            return None;
        }
        if !self.replies.is_empty() {
            return self.take_reply();
        }
        let overdue = self.bulk.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) >= BULK_WAIT) && self.bulk_may_go(now);
        if !overdue && let Some(tile) = self.frame_order.front().cloned() {
            return self.take_frame(&tile, Lane::Frame);
        }
        if self.bulk_may_go(now)
            && let Some(taken) = self.take_bulk(now)
        {
            return Some(taken);
        }
        let tile = self.frame_order.front().cloned()?;
        self.take_frame(&tile, Lane::Frame)
    }

    /// Whether the bulk lane may give a message now: the stopgap's pace
    /// (clients that do not acknowledge), and the typing hold of a message
    /// too large to be in flight at once. Sets `retry_at` when it may later.
    fn bulk_may_go(&mut self, now: Instant) -> bool {
        if self.bulk.is_empty() {
            return false;
        }
        if let Some(at) = self.pace.as_ref().and_then(|p| p.blocked(self.typed_at, now)) {
            self.retry_at = Some(self.retry_at.map_or(at, |r| r.min(at)));
            return false;
        }
        let limit = match (&self.window, &self.pace) {
            (Some(window), _) => window.budget() as usize,
            // The stopgap: about a second of its rate.
            (None, Some(_)) => TYPING_RATE as usize,
            // A local client: nothing waits in a tunnel.
            (None, None) => return true,
        };
        if !typing(self.typed_at, now) || self.head_weight() <= limit {
            self.held_since = None;
            return true;
        }
        let since = *self.held_since.get_or_insert(now);
        let lifts = [since + BULK_HOLD, self.typed_at.map_or(now, |at| at + TYPING_SPAN)].into_iter().min().unwrap_or(now);
        // One held message goes per BULK_HOLD while the typing goes on.
        let turn = self.large_at.is_none_or(|at| now.saturating_duration_since(at) >= BULK_HOLD);
        if now >= lifts && turn {
            return true;
        }
        let at = if now >= lifts { self.large_at.map_or(now, |at| at + BULK_HOLD) } else { lifts };
        self.retry_at = Some(self.retry_at.map_or(at, |r| r.min(at)));
        false
    }

    /// About how many bytes the message at the head of the bulk lane takes.
    fn head_weight(&self) -> usize {
        match self.bulk.front() {
            Some((_, Entry::Item(_, weight))) => *weight,
            Some((_, Entry::Latest(kind))) => self.latest.get(kind).map_or(0, weight),
            Some((_, Entry::Sessions)) => self.sessions.as_ref().map_or(0, |s| 64 + s.upsert.len() * 320 + s.remove.len() * 40),
            Some((_, Entry::Terminals)) | None => 0,
        }
    }

    fn take_reply(&mut self) -> Option<Taken> {
        let (_, message, weight) = self.replies.pop_front()?;
        self.control_bytes = self.control_bytes.saturating_sub(weight);
        Some(Taken { message, frame_of: None, lane: Lane::Reply })
    }

    fn take_bulk(&mut self, now: Instant) -> Option<Taken> {
        let held = self.held_since.take().is_some();
        while let Some((_, entry)) = self.bulk.pop_front() {
            if let Entry::Item(_, weight) = &entry {
                self.bulk_bytes = self.bulk_bytes.saturating_sub(*weight);
            }
            if let Some(message) = self.materialize(entry) {
                if held {
                    self.large_at = Some(now);
                }
                return Some(Taken { message, frame_of: None, lane: Lane::Bulk });
            }
        }
        None
    }

    fn take_frame(&mut self, tile: &str, lane: Lane) -> Option<Taken> {
        self.frame_order.retain(|t| t != tile);
        let message = self.frames.remove(tile)?;
        self.writing = Some(tile.to_owned());
        Some(Taken { message: Outgoing::Message(message), frame_of: Some(tile.to_owned()), lane })
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

    /// The writer wrote `bytes` taken from `lane` (a heartbeat counts as control).
    pub fn wrote(&self, bytes: usize, lane: Lane) {
        let now = Instant::now();
        let mut lanes = lock(&self.lanes);
        if let Some(window) = &mut lanes.window {
            match lane {
                Lane::Focused => window.wrote_focused(bytes as u64, now),
                _ => window.wrote(bytes as u64, now),
            }
        }
        let typed_at = lanes.typed_at;
        if lane == Lane::Bulk
            && let Some(pace) = &mut lanes.pace
        {
            pace.charge(bytes as u64, typed_at, now);
        }
    }

    /// The client's reader got a message of `bytes` from it: a large one
    /// (an upload) held its acknowledgements back (R3-A-06).
    pub fn read(&self, bytes: u64) {
        if bytes > SMALL as u64
            && let Some(window) = &mut lock(&self.lanes).window
        {
            window.uplink_busy(Instant::now());
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
            lanes.pace = Some(Pace { free_at: Instant::now() });
        }
    }

    /// The client typed (its echo is due: what is not urgent waits more).
    pub fn typed(&self) {
        lock(&self.lanes).typed_at = Some(Instant::now());
    }

    /// Other tiles' frames are slowed down: a client under the stopgap typed recently.
    pub fn slow_others(&self) -> bool {
        let lanes = lock(&self.lanes);
        lanes.pace.is_some() && typing(lanes.typed_at, Instant::now())
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
        Waiting { control: self.control.len() + self.replies.len(), frames: self.frames.len(), bulk: self.bulk.len(), bytes: self.control_bytes + self.bulk_bytes }
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
    fn bulk_that_waited_goes_before_other_tiles_frames_and_takes_the_focused_frames_turn_once_in_a_while() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.push(ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() });
        outbox.push(ServiceMsg::Doc { key: "l".into(), version: 1, json: "{}".into() });
        std::thread::sleep(BULK_WAIT + Duration::from_millis(20));
        outbox.push_frame("busy", frame("busy", 1));
        outbox.push_frame("echo", frame("echo", 1));
        // One bulk message takes the echo's turn (a tile that prints without
        // pause would starve it, R3-A-02), the next one waits its turn again.
        assert_eq!(kinds(&outbox), ["doc k", "frame echo", "doc l", "frame busy"]);
        outbox.push(ServiceMsg::Doc { key: "m".into(), version: 1, json: "{}".into() });
        std::thread::sleep(Duration::from_millis(50));
        outbox.push_frame("echo", frame("echo", 2));
        assert_eq!(kinds(&outbox), ["frame echo", "doc m"], "bulk that did not wait goes after the echo");
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

    #[test]
    fn a_large_whole_session_list_waits_with_the_bulk_and_deltas_join_it() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        let many: Vec<SessionSummary> = (0..1000).map(|i| session(&format!("s{i}"), "x")).collect();
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("a", "1")], remove: Vec::new() });
        // The whole list replaces the delta waiting in control and moves to the bulk lane.
        outbox.push(ServiceMsg::Sessions { full: true, upsert: many, remove: Vec::new() });
        outbox.push(ServiceMsg::Sessions { full: false, upsert: vec![session("b", "1")], remove: Vec::new() });
        outbox.push_frame("echo", frame("echo", 1));
        outbox.push(ServiceMsg::Error { id: None, message: "e".into() });
        let out = drain(&outbox);
        assert_eq!(out.iter().map(kind).take(2).collect::<Vec<_>>(), ["error e", "frame echo"], "the echo goes first");
        let [_, _, ServiceMsg::Sessions { full: true, upsert, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(upsert.len(), 1001, "the delta joined the list");
        // A small whole list stays in control.
        outbox.push_frame("echo", frame("echo", 2));
        outbox.push(ServiceMsg::Sessions { full: true, upsert: vec![session("c", "1")], remove: Vec::new() });
        assert!(matches!(drain(&outbox).first(), Some(ServiceMsg::Sessions { full: true, .. })));
    }

    /// Takes what the writer would take now and says it was written (`bytes` each).
    fn write_all(outbox: &Outbox, bytes: usize) -> Vec<String> {
        let mut out = Vec::new();
        while let Next::Item(Taken { message, frame_of, lane }) = outbox.next(Instant::now()) {
            outbox.wrote(bytes, lane);
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
        assert_eq!(in_flight, 0, "nothing is in flight once everything was acknowledged");
    }

    /// R3-A-04: the client may acknowledge a message before the writer
    /// counted it written; that acknowledgement is not lost.
    #[test]
    fn an_acknowledgement_that_overtakes_the_writers_count_is_kept() {
        let outbox = Outbox::new();
        outbox.start_window();
        let big = (WINDOW_MAX + URGENT_EXTRA) as usize;
        outbox.push(ServiceMsg::RpcReply { id: 1, json: "x".repeat(SMALL + 1) });
        let Next::Item(Taken { lane, .. }) = outbox.next(Instant::now()) else { panic!("the reply") };
        // The client read it all and said so before the writer's count.
        outbox.acked(big as u64);
        outbox.wrote(big, lane);
        assert_eq!(outbox.flow().unwrap().0, 0, "nothing is in flight");
        outbox.push(ServiceMsg::RpcReply { id: 2, json: "x".repeat(SMALL + 1) });
        assert_eq!(write_all(&outbox, 1000), ["reply 2"], "the next bulk message goes at once");
        // A bogus total is bounded: the window still counts later writes.
        outbox.acked(u64::MAX);
        let (in_flight, _) = outbox.flow().unwrap();
        assert_eq!(in_flight, 0);
    }

    /// The writer as `write_all` does, for one message (none: `None`).
    fn write_one(outbox: &Outbox, bytes: usize) -> Option<(String, Lane)> {
        let Next::Item(Taken { message, frame_of, lane }) = outbox.next(Instant::now()) else { return None };
        outbox.wrote(bytes, lane);
        if let Some(tile) = &frame_of {
            outbox.frame_written(tile);
        }
        let Outgoing::Message(message) = message else { return Some(("image".into(), lane)) };
        Some((kind(&message), lane))
    }

    /// R3-C-01: the tile you type in, printing fast, has at most one frame
    /// in flight beyond the budget: its echo never waits behind 128 KiB of its own output.
    #[test]
    fn the_focused_tile_has_one_frame_at_most_beyond_the_budget() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.start_window();
        let frame_bytes = 20 << 10;
        let mut beyond = 0;
        for n in 0..40 {
            outbox.push_frame("echo", frame("echo", n));
            match write_one(&outbox, frame_bytes) {
                Some((_, Lane::Focused)) => {
                    let (in_flight, budget) = outbox.flow().unwrap();
                    if in_flight > budget {
                        beyond += 1;
                    }
                    assert!(in_flight <= budget + frame_bytes as u64, "{in_flight} in flight, budget {budget}");
                }
                Some(other) => panic!("{other:?}"),
                None => break,
            }
        }
        assert_eq!(beyond, 1, "one frame beyond the budget, then it waits");
        assert!(write_one(&outbox, frame_bytes).is_none(), "the next frame waits for an acknowledgement");
        // Its acknowledgement lets the next one go (beyond the budget again).
        let (in_flight, _) = outbox.flow().unwrap();
        outbox.acked(in_flight);
        assert!(matches!(write_one(&outbox, frame_bytes), Some((_, Lane::Focused))));
        // Control still has its extra room.
        outbox.push(ServiceMsg::Error { id: None, message: "e".into() });
        assert!(matches!(write_one(&outbox, 1000), Some((_, Lane::Control))));
    }

    /// R3-A-02 (a): mid-sized replies wait behind the echo, not ahead of it.
    #[test]
    fn mid_sized_replies_wait_behind_the_echo() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        for id in 0..20 {
            outbox.push(ServiceMsg::RpcReply { id, json: "x".repeat(50 << 10) });
        }
        outbox.push(ServiceMsg::RpcReply { id: 99, json: "small".into() });
        outbox.push_frame("busy", frame("busy", 1));
        outbox.push_frame("echo", frame("echo", 1));
        let out = kinds(&outbox);
        assert_eq!(out[..2], ["reply 99", "frame echo"], "a small reply stays in control; the echo goes before the large ones");
        assert_eq!(out[2..22], (0..20).map(|id| format!("reply {id}")).collect::<Vec<_>>()[..], "the replies in order, before other tiles");
        assert_eq!(out[22], "frame busy");
    }

    /// A link of `rate` bytes per second and `rtt` round trip behind a
    /// tunnel buffer (in real time); every 16 ms the focused tile has a
    /// frame of `frame_bytes`, and `queued` is pushed at `push_at`. Returns
    /// when each of `queued` (replies by id) was written, and the longest a
    /// focused frame waited in the link beyond the round trip (after the
    /// first second, before `push_at`).
    fn simulate_printing(rate: f64, rtt: Duration, frame_bytes: u64, queued: Vec<ServiceMsg>, push_at: Duration, seconds: u64) -> (Vec<Option<Duration>>, Duration) {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.start_window();
        let mut queued = Some(queued);
        let mut done: Vec<Option<Duration>> = vec![None; queued.as_ref().map_or(0, Vec::len)];
        let start = Instant::now();
        let mut flight: VecDeque<(Duration, u64)> = VecDeque::new();
        let mut link_free = Duration::ZERO;
        let mut written = 0u64;
        let mut worst = Duration::ZERO;
        let mut next_frame = Duration::ZERO;
        let mut n = 0;
        loop {
            let now = start.elapsed();
            if now >= Duration::from_secs(seconds) {
                break;
            }
            while flight.front().is_some_and(|(at, _)| *at <= now) {
                let (_, total) = flight.pop_front().unwrap();
                outbox.acked(total);
            }
            if now >= push_at && let Some(messages) = queued.take() {
                for message in messages {
                    outbox.push(message);
                }
            }
            if now >= next_frame && !outbox.frame_state("echo").0 {
                n += 1;
                outbox.push_frame("echo", frame("echo", n));
                next_frame = now + Duration::from_millis(16);
            }
            while let Next::Item(Taken { message, frame_of, lane }) = outbox.next(Instant::now()) {
                let size = match (&message, lane) {
                    (_, Lane::Focused) => frame_bytes,
                    (Outgoing::Message(m), _) => weight(m) as u64,
                    _ => 1000,
                };
                outbox.wrote(size as usize, lane);
                written += size;
                if let Some(tile) = &frame_of {
                    outbox.frame_written(tile);
                }
                let begins = link_free.max(now + rtt / 2);
                let arrives = begins + Duration::from_secs_f64(size as f64 / rate);
                link_free = arrives;
                if lane == Lane::Focused && now > Duration::from_secs(1) && now < push_at {
                    worst = worst.max(arrives.saturating_sub(now) - rtt / 2);
                }
                if let Outgoing::Message(ServiceMsg::RpcReply { id, .. }) = message {
                    done[id as usize] = Some(now);
                }
                flight.push_back((arrives + rtt / 2, written));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        (done, worst)
    }

    /// R3-C-01: the tile you type in printing faster than a slow link (150
    /// KB/s) carries: its frames wait about the budget plus one frame in the
    /// tunnel, not the budget plus 128 KiB (about a second here).
    #[test]
    fn a_tile_printing_without_pause_does_not_bury_its_own_echo() {
        let (_, worst) = simulate_printing(150e3, Duration::from_millis(60), 6 << 10, Vec::new(), Duration::from_secs(60), 3);
        assert!(worst < Duration::from_millis(500), "an echo waited {worst:?} in the link");
    }

    /// R3-A-02 (b): the same tile printing does not starve a large reply.
    #[test]
    fn a_tile_printing_without_pause_does_not_starve_bulk() {
        let reply = ServiceMsg::RpcReply { id: 0, json: "x".repeat(300 << 10) };
        let (done, _) = simulate_printing(150e3, Duration::from_millis(60), 6 << 10, vec![reply], Duration::from_secs(1), 4);
        assert!(done[0].is_some_and(|at| at < Duration::from_secs(3)), "the reply went: {done:?}");
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
    /// in the link's queue beyond the round trip, the bytes per second the
    /// window let through).
    fn simulate(rate: f64, rtt: Duration, seconds: u64) -> (u64, Duration, f64) {
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
        (window.budget(), worst, window.acked as f64 / seconds as f64)
    }

    #[test]
    fn the_budget_follows_the_link() {
        // A slow link (200 KB/s): an echo waits about FLOW_TARGET plus one message behind bulk.
        let (budget, worst, _) = simulate(200e3, Duration::from_millis(60), 20);
        assert!(budget <= 64 << 10, "slow link: budget {budget}");
        assert!(worst < Duration::from_millis(700), "slow link: bulk queued {worst:?} ahead of an echo");
        // A fast link far away (20 MB/s, 120 ms): the window opens to its
        // maximum, which lets bulk use a fair part of the link (R3-A-05).
        let (budget, _, throughput) = simulate(20e6, Duration::from_millis(120), 10);
        assert_eq!(budget, WINDOW_MAX, "fast link: budget {budget}");
        assert!(throughput > 5e6, "fast link: bulk at {throughput:.0} B/s");
        // 1 MB/s, as the faults step: under half a second of queue.
        let (budget, worst, _) = simulate(1e6, Duration::from_millis(20), 20);
        assert!((WINDOW_MIN..=WINDOW_MAX).contains(&budget));
        assert!(worst < Duration::from_millis(500), "1 MB/s: {worst:?}");
    }

    /// R3-A-06: acknowledgements an upload held back (they come late, all at
    /// once) do not lower the budget.
    #[test]
    fn acknowledgements_held_back_by_an_upload_do_not_shrink_the_budget() {
        let mut window = Window::default();
        let start = Instant::now();
        // A steady 2 MB/s with the window full: the budget settles.
        let mut at = start;
        for _ in 0..200 {
            window.wrote(16 << 10, at);
            at += Duration::from_millis(8);
            window.acked(window.written.saturating_sub(WINDOW_START), at);
        }
        let before = window.budget();
        // An upload: the reader gets a large message, the acknowledgements behind it come after a second, at once.
        window.uplink_busy(at);
        for _ in 0..8 {
            window.wrote(16 << 10, at);
        }
        at += Duration::from_secs(1);
        for step in 0..8 {
            window.acked(window.written - (8 - step) * (16 << 10), at);
        }
        window.acked(window.written, at + Duration::from_millis(150));
        assert!(window.budget() >= before, "budget {} fell below {before}", window.budget());
        // Without the upload, the same stall would have measured a low rate.
        let mut plain = Window::default();
        let mut at = start;
        for _ in 0..200 {
            plain.wrote(16 << 10, at);
            at += Duration::from_millis(8);
            plain.acked(plain.written.saturating_sub(WINDOW_START), at);
        }
        let before = plain.budget();
        for _ in 0..8 {
            plain.wrote(16 << 10, at);
        }
        at += Duration::from_secs(1);
        plain.acked(plain.written, at + Duration::from_millis(150));
        assert!(plain.budget() < before, "the test's stall does measure low without the upload");
    }

    /// R3-A-09: under the stopgap a large transfer written while typing
    /// never freezes other tiles, and holds later bulk a second at most.
    #[test]
    fn a_large_transfer_while_typing_does_not_freeze_other_tiles() {
        let outbox = Outbox::new();
        outbox.set_paced();
        outbox.push(ServiceMsg::RpcReply { id: 1, json: "x".repeat(2 << 20) });
        let Next::Item(Taken { lane, .. }) = outbox.next(Instant::now()) else { panic!("the reply") };
        outbox.typed();
        outbox.wrote(2 << 20, lane);
        outbox.push_frame("other", frame("other", 1));
        let started = Instant::now();
        assert_eq!(write_all(&outbox, 1000), ["frame other"], "another tile's frame goes at once");
        assert!(started.elapsed() < Duration::from_millis(300));
        outbox.push(ServiceMsg::Doc { key: "k".into(), version: 1, json: "{}".into() });
        let started = Instant::now();
        assert!(matches!(outbox.next(Instant::now() + Duration::from_secs(5)), Next::Item(Taken { lane: Lane::Bulk, .. })));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(800) && waited < Duration::from_millis(1500), "bulk waited {waited:?}");
    }

    /// R3-C-01: while the client types, a bulk message too large to be in
    /// flight at once waits (at most BULK_HOLD); a small one does not.
    #[test]
    fn a_large_bulk_message_waits_while_the_client_types() {
        let outbox = Outbox::new();
        outbox.set_focused(Some("echo".into()));
        outbox.start_window();
        outbox.typed();
        outbox.push(ServiceMsg::Doc { key: "small".into(), version: 1, json: "{}".into() });
        assert_eq!(write_all(&outbox, 1000), ["doc small"], "a small one goes");
        outbox.push(ServiceMsg::Doc { key: "large".into(), version: 1, json: "x".repeat(600 << 10) });
        outbox.push_frame("busy", frame("busy", 1));
        outbox.push_frame("echo", frame("echo", 1));
        assert_eq!(write_all(&outbox, 1000), ["frame echo", "frame busy"], "the large one waits while you type");
        let started = Instant::now();
        let Next::Item(Taken { lane: Lane::Bulk, .. }) = outbox.next(Instant::now() + Duration::from_secs(5)) else { panic!("the large one goes") };
        let waited = started.elapsed();
        assert!(waited > Duration::from_millis(1500) && waited < Duration::from_millis(2500), "it waited {waited:?}");
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
        let Next::Item(Taken { lane: Lane::Bulk, .. }) = outbox.next(Instant::now() + Duration::from_secs(5)) else { panic!("the next reply") };
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(200), "paced: {waited:?}");
        // A client told to acknowledge leaves the stopgap.
        outbox.start_window();
        assert!(!outbox.slow_others());
    }
}
