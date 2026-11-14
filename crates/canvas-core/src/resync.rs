//! Keeping this client's terminal and session lists in step with the
//! service's. The service sends them whole when a client connects and then
//! only changes (`Terminals`, `TerminalGone`, `Sessions` upserts): a change
//! lost on the way (a full queue, the moment between a connect's snapshot
//! and its registration) would stay wrong until the next reconnect: a box
//! that looks stopped or open elsewhere while it runs here, or a dead
//! terminal that looks alive.
//!
//! - Every `Stats` answer (asked for every 3 s) carries the service's
//!   terminal count and how many are alive. When this client's lists say
//!   otherwise on two answers in a row (the same counts both times, so not a
//!   change on its way), it sends `RefreshSessions`, which a service that
//!   answers it with its whole lists repairs at once; when the difference
//!   is still there two answers later, it reconnects to get the whole lists
//!   (at most every 5 minutes), saying so.
//! - `RefreshSessions` also goes every minute: older services only index
//!   again (cheap); one that answers with its whole lists repairs session
//!   states this way too (no wire change: an older client takes those as the
//!   lists they are).
//! - A reconnect (or a restarted local service) starts this client's state
//!   with the service from scratch: [`App::reset_service_state`].

use std::time::{Duration, Instant};

use crate::app::App;
use crate::protocol::{ClientMsg, ServiceStats};

/// `RefreshSessions` this often while connected.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// `Stats` answers in a row with the same difference before `RefreshSessions`.
const STRIKES_REFRESH: u32 = 2;
/// … and before reconnecting.
const STRIKES_RECONNECT: u32 = 4;
/// At most one reconnect to repair the lists this often.
const RECONNECT_GAP: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub struct Resync {
    /// The difference the last `Stats` showed: (the service's terminals and
    /// alive ones, this client's), and on how many answers in a row.
    mismatch: Option<((u32, u32), (u32, u32))>,
    strikes: u32,
    /// When `RefreshSessions` last went.
    pub refreshed_at: Instant,
    /// When this client last reconnected to repair its lists.
    reconnected_at: Option<Instant>,
    /// That reconnect is under way (its notice says so).
    pub repairing: bool,
}

impl Default for Resync {
    fn default() -> Self {
        Self { mismatch: None, strikes: 0, refreshed_at: Instant::now(), reconnected_at: None, repairing: false }
    }
}

impl App {
    /// The service's numbers (`Stats`), compared with this client's lists.
    pub fn on_stats(&mut self, stats: ServiceStats) {
        let theirs = (stats.terminals, stats.alive);
        self.stats = Some(stats);
        let mine = (self.terms.len() as u32, self.terms.values().filter(|t| t.alive).count() as u32);
        if theirs == mine || !self.connected || self.reconnecting || self.restarting.is_some() || self.resync_terms {
            self.resync.mismatch = None;
            self.resync.strikes = 0;
            return;
        }
        if self.resync.mismatch == Some((theirs, mine)) {
            self.resync.strikes += 1;
        } else {
            self.resync.mismatch = Some((theirs, mine));
            self.resync.strikes = 1;
        }
        if self.resync.strikes == STRIKES_REFRESH {
            self.refresh_lists();
        } else if self.resync.strikes >= STRIKES_RECONNECT && self.resync.reconnected_at.is_none_or(|at| at.elapsed() >= RECONNECT_GAP) && self.target.is_some() {
            self.resync.reconnected_at = Some(Instant::now());
            self.resync.mismatch = None;
            self.resync.strikes = 0;
            // The reader ends, the front end reconnects and the service sends its whole lists.
            self.resync.repairing = self.conn.close();
        }
    }

    /// Asks the service to look at its sessions again (one that answers
    /// with its whole lists repairs this client's).
    pub fn refresh_lists(&mut self) {
        self.resync.refreshed_at = Instant::now();
        self.send(ClientMsg::RefreshSessions);
    }

    /// Housekeeping: `RefreshSessions` every [`REFRESH_EVERY`].
    pub fn tick_resync(&mut self) {
        if self.connected && !self.reconnecting && !self.demo && self.resync.refreshed_at.elapsed() >= REFRESH_EVERY {
            self.refresh_lists();
        }
    }

    /// A new connection (a reconnect, a restarted service): the service starts
    /// this client from scratch, so subscriptions, sizes, tracking and routing
    /// go again, launches it was asked for are its own now, and the first
    /// terminal list replaces the known terminals.
    pub fn reset_service_state(&mut self) {
        self.last_subscribed.clear();
        self.last_focused_sub = None;
        self.last_sizes.clear();
        self.pending_sizes.clear();
        self.size_states.clear();
        // The new connection's claim is gone, and a reconnect is no reason to
        // take the terminal from a phone that has it: the size goes out as a
        // wish, the next click, key or return to the window claims (R2-E-09).
        self.claim_held = None;
        self.resized_from.clear();
        self.last_tracked.clear();
        self.routing_sent.clear();
        // What was asked on the previous connection is not answered on this one.
        self.docs_asked = 0;
        self.resync_terms = true;
        // Launches the previous connection asked for are the service's now (a
        // restarted one resumes what it ran): never "preparing" forever.
        self.launching.clear();
        self.stats = None;
        self.resync.mismatch = None;
        self.resync.strikes = 0;
        self.resync.refreshed_at = Instant::now();
        self.terms_rev += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::protocol::{ServiceMsg, TermInfo};
    use crate::sync::Opened;

    fn rig() -> (App, Receiver<ClientMsg>) {
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(Doc::default()), false, ".".into(), app_tx, None);
        app.target = Some(crate::conn::ConnTarget::Tcp { addr: "127.0.0.1:1".into(), remote_id: None, secret: vec![1] });
        (app, sent)
    }

    fn stats(terminals: u32, alive: u32) -> ServiceMsg {
        ServiceMsg::Stats(ServiceStats { terminals, alive, ..Default::default() })
    }

    fn refreshes(sent: &Receiver<ClientMsg>) -> usize {
        sent.try_iter().filter(|m| matches!(m, ClientMsg::RefreshSessions)).count()
    }

    #[test]
    fn lists_that_stay_out_of_step_with_the_service_are_repaired() {
        let (mut app, sent) = rig();
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: true, ..Default::default() }]));
        // In step: nothing to do.
        for _ in 0..5 {
            app.on_service(stats(1, 1));
        }
        assert_eq!(refreshes(&sent), 0);
        // A launch on its way shows once, then its Terminals arrives: nothing.
        app.on_service(stats(2, 2));
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "b".into(), alive: true, ..Default::default() }]));
        app.on_service(stats(2, 2));
        assert_eq!(refreshes(&sent), 0);
        // "b" exited but its Terminals(alive: false) was lost: the same difference twice.
        app.on_service(stats(2, 1));
        assert_eq!(refreshes(&sent), 0);
        app.on_service(stats(2, 1));
        assert_eq!(refreshes(&sent), 1, "first: ask the service for its lists");
        app.on_service(stats(2, 1));
        assert!(!app.resync.repairing);
        app.on_service(stats(2, 1));
        assert!(app.resync.reconnected_at.is_some(), "then reconnect for the whole lists");
        assert!(!app.resync.repairing, "a detached connection cannot be closed");
        // At most every 5 minutes.
        let at = app.resync.reconnected_at;
        for _ in 0..8 {
            app.on_service(stats(2, 1));
        }
        assert_eq!(app.resync.reconnected_at, at);
        // A reconnect starts over.
        app.reset_service_state();
        assert!(app.resync_terms && app.stats.is_none() && app.resync.strikes == 0);
    }

    #[test]
    fn a_restarted_local_service_gets_subscriptions_and_tracking_again() {
        let (tx, _sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let node: crate::doc::Node = serde_json::from_value(serde_json::json!({ "id": "a", "kind": "session", "name": "a", "x": 0, "y": 0, "cwd": "." })).unwrap();
        let doc = Doc { nodes: vec![node], ..Doc::default() };
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        let draw = |app: &mut App| {
            let area = ratatui::layout::Rect::new(0, 0, 160, 50);
            crate::ui::render(app, &mut ratatui::buffer::Buffer::empty(area), area, None);
            app.after_draw();
        };
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: true, ..Default::default() }]));
        app.select_node("a", false);
        app.show_terminal("a");
        draw(&mut app);
        assert_eq!(app.last_subscribed, ["a"]);
        // ⟳ Update service: the old service goes, the new one resumes "a" in place.
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        app.restarting = Some(vec!["a".into()]);
        app.on_app_event(crate::app::AppEvent::ServiceRestarted { result: Ok(Conn::detached(tx, service_rx)), mouse: false });
        draw(&mut app);
        let messages: Vec<ClientMsg> = sent.try_iter().collect();
        assert!(messages.iter().any(|m| matches!(m, ClientMsg::Launch { id, .. } if id == "a")), "{messages:?}");
        assert!(messages.iter().any(|m| matches!(m, ClientMsg::Subscribe8 { ids, .. } if ids == &["a"])), "the new service is subscribed to the tile: {messages:?}");
        assert!(messages.iter().any(|m| matches!(m, ClientMsg::Track { .. })), "and tracks the canvas's sessions: {messages:?}");
    }

    #[test]
    fn sessions_are_refreshed_every_minute_while_connected() {
        let (mut app, sent) = rig();
        app.tick_resync();
        assert_eq!(refreshes(&sent), 0);
        app.resync.refreshed_at = Instant::now() - REFRESH_EVERY;
        app.tick_resync();
        assert_eq!(refreshes(&sent), 1);
        app.tick_resync();
        assert_eq!(refreshes(&sent), 0);
        app.connected = false;
        app.resync.refreshed_at = Instant::now() - REFRESH_EVERY;
        app.tick_resync();
        assert_eq!(refreshes(&sent), 0, "not while offline");
    }
}
