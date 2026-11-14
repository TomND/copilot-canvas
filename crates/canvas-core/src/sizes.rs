//! Terminal sizes from this client's side (tmux "latest", see
//! `canvas_common::sizing`): the size each shown tile asks for (`Resize`,
//! sent once a size has held for a moment), the terminal that has the
//! keyboard here taking this client's size (`Claim`), and the repair of a
//! terminal whose real size (its frames) never reached the tile's.
//!
//! - A tile's size goes out when it changes and has held for
//!   [`RESIZE_SETTLE`] (a window drag sends one `Resize`, not one per frame);
//!   a terminal's first size goes out at once.
//! - The terminal with the keyboard is claimed when it gets it (a click in
//!   its tile, F6, a start from this window) and when the window comes back
//!   to the front; typing claims too (the service does that). Nothing else
//!   claims: not a terminal that comes alive while the keyboard is on its
//!   tile (the phone or the restore started it), not a reconnect (R2-D-04,
//!   R2-E-09); a terminal that is not running when it gets the keyboard is
//!   passed over.
//! - After each draw, a terminal whose frames keep another size than its
//!   tile for [`SIZE_GRACE`] after this client's last `Resize` gets the
//!   `Resize` again (and the `Claim`, when this client's claim of it is
//!   still held), with backoff: a lost message, a stale size or a resize the
//!   host did not finish ("resizing…" from the second repair until the
//!   third, then "sized elsewhere"). A terminal
//!   that had this client's size and then took another was taken by another
//!   client (the phone, another window): it is left to that client and the
//!   tile says so until you click into it; going back to the size it had
//!   before this client's `Resize` is that resize undone (a refused PTY
//!   size), repaired like a size that never arrived (R2-D-07).
//! - While the sizes differ, both front ends draw the terminal's bottom rows
//!   (or down to its cursor): `TermView::first_row`.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::app::{App, Drag, Focus};
use crate::protocol::ClientMsg;

/// A changed tile size is sent once it has held this long.
pub const RESIZE_SETTLE: Duration = Duration::from_millis(150);
/// How long a terminal may keep another size after this client's `Resize`
/// before it is sent again (a round trip through a tunnel, ConPTY's resize
/// and the agent's redraw).
pub const SIZE_GRACE: Duration = Duration::from_millis(1500);
/// The longest wait between two repairs of the same terminal.
const REPAIR_MAX: Duration = Duration::from_secs(30);

/// What a tile in `state` says: "sized elsewhere" when another client took
/// it or it is still another size after three repairs (about 10 s; a busy
/// engine takes a new size only after the output queued before it), and
/// "resizing…" after two until then.
fn noted(state: Option<SizeState>) -> Option<&'static str> {
    match state {
        Some(SizeState::Taken) => Some("sized elsewhere: click to fit"),
        Some(SizeState::Differs { tries, .. }) if tries >= 3 => Some("sized elsewhere: click to fit"),
        Some(SizeState::Differs { tries, .. }) if tries >= 2 => Some("resizing…"),
        _ => None,
    }
}

/// How a shown terminal's real size compares with its tile's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeState {
    /// Its frames have this client's size.
    Matched,
    /// Another size since `since`; repaired (`tries` times, last at `sent`)
    /// while that lasts.
    Differs { since: Instant, sent: Option<Instant>, tries: u32 },
    /// It had this client's size, then took another: another client's
    /// (left to it until you act here).
    Taken,
}

/// The size a tile asks for (the host's minimum is 20×5).
pub fn tile_size(inner: Rect) -> (u16, u16) {
    (inner.width.max(20), inner.height.max(5))
}

/// The inner rectangle of a tile's outline.
pub fn tile_inner(outer: Rect) -> Rect {
    Rect::new(outer.x + 1, outer.y + 1, outer.width.saturating_sub(2), outer.height.saturating_sub(2))
}

impl App {
    /// The terminal row shown at the top of tile `id` (`TermView::first_row`).
    pub fn tile_first_row(&self, id: &str) -> u16 {
        let height = self.layout.tiles.iter().find(|(t, _, _)| t == id).map_or(0, |(_, _, inner)| inner.height);
        self.views.get(id).map_or(0, |view| view.first_row(height))
    }

    /// The tile's note when its terminal keeps another client's size (or
    /// has not taken this client's yet).
    pub fn size_note(&self, id: &str) -> Option<&'static str> {
        noted(self.size_states.get(id).copied())
    }

    /// The size a launch of `id` asks for: the tile it will get (its own
    /// tile when it shows, else where `show_terminal` puts it), else the
    /// right panel's size, else a guess.
    pub fn launch_size(&self, id: &str) -> (u16, u16) {
        if let Some((_, _, inner)) = self.layout.tiles.iter().find(|(t, _, _)| t == id) {
            return tile_size(*inner);
        }
        let area = self.layout.terms_shown;
        if area.width < 10 || area.height < 3 {
            return (100, 30);
        }
        let ids: Vec<String> = if self.doc.ui.tiled {
            let slots = self.slots_showing(&self.tile_slots, id);
            slots.into_iter().filter(|t| t == id || self.terms.contains_key(t)).collect()
        } else {
            vec![id.to_owned()]
        };
        let position = ids.iter().position(|t| t == id).unwrap_or(0);
        match crate::ui::tile_rects(area, ids.len()).get(position) {
            Some(outer) => tile_size(tile_inner(*outer)),
            None => (100, 30),
        }
    }

    /// The window came to the front (or went to the back): the terminal
    /// with the keyboard is claimed again at the next draw.
    pub fn window_focused(&mut self, focused: bool) {
        if focused {
            self.last_claimed = None;
        }
    }

    /// Forgets what this client sent about a terminal's size (it is sent
    /// again at the next draw): a launch, a relaunch, an explicit click.
    pub fn forget_size(&mut self, id: &str) {
        self.last_sizes.remove(id);
        self.pending_sizes.remove(id);
        self.size_states.remove(id);
        self.resized_from.remove(id);
        if self.last_claimed.as_deref() == Some(id) {
            self.last_claimed = None;
        }
        if self.claim_held.as_deref() == Some(id) {
            self.claim_held = None;
        }
    }

    /// After a draw (and on ticks, for sizes still settling): sends the tiles'
    /// sizes, claims the terminal with the keyboard and repairs sizes that
    /// never arrived.
    pub fn sync_sizes(&mut self, now: Instant) {
        let shown = &self.last_subscribed;
        self.last_sizes.retain(|id, _| shown.contains(id));
        self.pending_sizes.retain(|id, _| shown.contains(id));
        self.size_states.retain(|id, _| shown.contains(id));
        self.resized_from.retain(|id, _| shown.contains(id));
        if !self.connected || matches!(self.drag, Some(Drag::SidebarDivider | Drag::TermDivider)) {
            return;
        }
        let tiles: Vec<(String, (u16, u16))> = self.layout.tiles.iter().map(|(id, _, inner)| (id.clone(), tile_size(*inner))).collect();
        for (id, size) in &tiles {
            if !self.owned_alive(id) {
                self.pending_sizes.remove(id);
                continue;
            }
            if self.last_sizes.get(id) == Some(size) {
                self.pending_sizes.remove(id);
                continue;
            }
            let settled = match self.pending_sizes.get(id) {
                _ if !self.last_sizes.contains_key(id) => true,
                Some((pending, since)) if pending == size => now.duration_since(*since) >= RESIZE_SETTLE,
                _ => {
                    self.pending_sizes.insert(id.clone(), (*size, now));
                    false
                }
            };
            if settled && self.queue(ClientMsg::Resize { id: id.clone(), cols: size.0, rows: size.1 }) {
                self.pending_sizes.remove(id);
                self.last_sizes.insert(id.clone(), *size);
                self.size_states.remove(id);
                self.note_resized_from(id, *size);
            }
        }
        // The terminal the keyboard went to here takes this client's size once
        // it runs and its size went out; one not running is passed over.
        let keyboard = self.focused_term.clone().filter(|_| self.focus == Focus::Terminal);
        if keyboard != self.last_claimed {
            match keyboard {
                Some(id) if self.owned_alive(&id) => {
                    if self.last_sizes.contains_key(&id) && self.queue(ClientMsg::Claim { id: id.clone() }) {
                        self.size_states.remove(&id);
                        self.claim_held = Some(id.clone());
                        self.last_claimed = Some(id);
                    }
                }
                other => self.last_claimed = other,
            }
        }
        for (id, size) in tiles {
            self.repair_size(&id, size, now);
        }
    }

    /// Compares tile `id`'s terminal with the size this client asked for and
    /// asks again when it never arrived.
    fn repair_size(&mut self, id: &str, want: (u16, u16), now: Instant) {
        let have = self.views.get(id).filter(|v| v.cols > 0 && v.rows > 0).map(|v| (v.cols, v.rows));
        // Only a size this client sent (not one still settling) of a running terminal.
        let (Some(have), true) = (have, self.owned_alive(id) && self.last_sizes.get(id) == Some(&want)) else { return };
        let state = self.size_states.get(id).copied();
        let next = match state {
            _ if have == want => SizeState::Matched,
            // Back to the size it had before this client's `Resize`: that resize
            // was undone (the PTY refused it), not taken by another client.
            Some(SizeState::Matched) if self.resized_from.get(id) == Some(&have) => {
                self.resized_from.remove(id);
                if self.claim_held.as_deref() == Some(id) {
                    self.claim_held = None;
                }
                SizeState::Differs { since: now, sent: None, tries: 0 }
            }
            Some(SizeState::Matched | SizeState::Taken) => {
                // Another client took it: never claimed back without you.
                if self.claim_held.as_deref() == Some(id) {
                    self.claim_held = None;
                }
                SizeState::Taken
            }
            Some(SizeState::Differs { since, sent, tries }) => {
                let wait = match sent {
                    None => since + SIZE_GRACE,
                    Some(at) => at + (SIZE_GRACE * 2u32.saturating_pow(tries.min(5))).min(REPAIR_MAX),
                };
                if now < wait {
                    return;
                }
                let claim = self.claim_held.as_deref() == Some(id) && self.focus == Focus::Terminal;
                if !self.queue(ClientMsg::Resize { id: id.to_owned(), cols: want.0, rows: want.1 }) {
                    return;
                }
                self.note_resized_from(id, want);
                if claim {
                    self.queue(ClientMsg::Claim { id: id.to_owned() });
                }
                SizeState::Differs { since, sent: Some(now), tries: tries + 1 }
            }
            None => SizeState::Differs { since: now, sent: None, tries: 0 },
        };
        if state != Some(next) {
            self.needs_draw |= noted(state) != noted(Some(next));
            self.size_states.insert(id.to_owned(), next);
        }
    }

    /// A `Resize` to `size` went out for `id`: the size it had (when another).
    fn note_resized_from(&mut self, id: &str, size: (u16, u16)) {
        match self.views.get(id).filter(|v| v.cols > 0 && v.rows > 0).map(|v| (v.cols, v.rows)) {
            Some(have) if have != size => {
                self.resized_from.insert(id.to_owned(), have);
            }
            _ => {}
        }
    }

    /// A size is settling or a repair is due: the front end ticks soon.
    pub fn sizes_pending(&self) -> bool {
        !self.pending_sizes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::Conn;
    use crate::doc::{Doc, Node};
    use crate::protocol::{Frame8, ServiceMsg, TermInfo};
    use crate::sync::Opened;

    fn node(id: &str) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0, "reviewed_ms": 1, "cwd": ".", "command": ["sh"] })).unwrap()
    }

    fn rig(ids: &[&str]) -> (App, Receiver<ClientMsg>) {
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: ids.iter().map(|id| node(id)).collect(), ..Doc::default() };
        let app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), true, ".".into(), app_tx, None);
        (app, sent)
    }

    fn alive(app: &mut App, id: &str) {
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: id.into(), alive: true, ..Default::default() }]));
    }

    fn frame(app: &mut App, id: &str, cols: u16, rows: u16) {
        let frame = Frame8 { id: id.into(), cols, rows, full: true, changed: Vec::new(), cursor: None, cursor_shape: 0, cursor_blink: false, display_offset: 0, history: 0, modes: 0, links: Vec::new() };
        app.on_service(ServiceMsg::Frame8(frame));
    }

    fn draw(app: &mut App) {
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 160, 50));
        crate::ui::render(app, &mut buf, Rect::new(0, 0, 160, 50), None);
        app.after_draw();
    }

    fn sizes(sent: &Receiver<ClientMsg>) -> Vec<ClientMsg> {
        sent.try_iter().filter(|m| matches!(m, ClientMsg::Resize { .. } | ClientMsg::Claim { .. } | ClientMsg::Launch { .. })).collect()
    }

    fn tile(app: &App, id: &str) -> (u16, u16) {
        app.layout.tiles.iter().find(|(t, _, _)| t == id).map(|(_, _, inner)| tile_size(*inner)).expect("shown")
    }

    #[test]
    fn a_relaunch_in_the_third_tile_gets_that_tiles_size() {
        let (mut app, sent) = rig(&["a", "b", "c"]);
        app.doc.ui.tiled = true;
        for id in ["a", "b", "c"] {
            alive(&mut app, id);
            app.show_terminal(id);
        }
        draw(&mut app);
        draw(&mut app);
        let third = tile(&app, "c");
        assert_ne!(third, tile(&app, "a"), "slot 3 is full width");
        let _ = sizes(&sent);
        // It exits and stays in its tile; Enter starts it again.
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "c".into(), alive: false, exit_code: Some(0), ..Default::default() }]));
        draw(&mut app);
        app.start_node("c");
        let launched = sizes(&sent);
        assert!(matches!(launched.as_slice(), [ClientMsg::Launch { cols, rows, .. }] if (*cols, *rows) == third), "launched at its own tile's size: {launched:?}");
        // Alive again at a wrong size (say, an older client's launch): the size goes out once it is alive.
        alive(&mut app, "c");
        draw(&mut app);
        assert!(sizes(&sent).iter().any(|m| matches!(m, ClientMsg::Resize { id, cols, rows } if id == "c" && (*cols, *rows) == third)));
    }

    #[test]
    fn a_size_that_never_arrives_is_sent_again_and_a_taken_one_is_left_alone() {
        let (mut app, sent) = rig(&["a"]);
        alive(&mut app, "a");
        app.select_node("a", false);
        app.show_terminal("a");
        app.set_focus(Focus::Terminal);
        draw(&mut app);
        draw(&mut app);
        let want = tile(&app, "a");
        let first = sizes(&sent);
        assert!(first.contains(&ClientMsg::Resize { id: "a".into(), cols: want.0, rows: want.1 }), "{first:?}");
        assert!(first.contains(&ClientMsg::Claim { id: "a".into() }), "the terminal with the keyboard is claimed: {first:?}");
        // The terminal stays taller than the tile (the resize was lost).
        frame(&mut app, "a", want.0, want.1 + 10);
        draw(&mut app);
        assert!(sizes(&sent).is_empty(), "within the grace");
        let since = match app.size_states.get("a") {
            Some(SizeState::Differs { since, .. }) => *since,
            other => panic!("{other:?}"),
        };
        app.sync_sizes(since + SIZE_GRACE);
        assert_eq!(sizes(&sent), [ClientMsg::Resize { id: "a".into(), cols: want.0, rows: want.1 }, ClientMsg::Claim { id: "a".into() }]);
        app.sync_sizes(since + SIZE_GRACE + Duration::from_millis(100));
        assert!(sizes(&sent).is_empty(), "backoff");
        assert!(app.size_note("a").is_none(), "a size on its way is not called elsewhere");
        app.sync_sizes(since + SIZE_GRACE * 3 + Duration::from_millis(1));
        assert_eq!(sizes(&sent).len(), 2, "again later");
        assert_eq!(app.size_note("a"), Some("resizing…"), "still another size after two repairs: a busy engine, say");
        app.sync_sizes(since + SIZE_GRACE * 7 + Duration::from_millis(1));
        assert_eq!(sizes(&sent).len(), 2);
        assert_eq!(app.size_note("a"), Some("sized elsewhere: click to fit"), "after three, the tile says so");
        // It arrives; then another client takes the terminal: left to it, the tile says so.
        frame(&mut app, "a", want.0, want.1);
        draw(&mut app);
        assert_eq!(app.size_states.get("a"), Some(&SizeState::Matched));
        frame(&mut app, "a", 60, 70);
        draw(&mut app);
        app.sync_sizes(Instant::now() + Duration::from_secs(60));
        assert!(sizes(&sent).is_empty(), "never fights the other client");
        assert!(app.size_note("a").is_some());
        assert_eq!(app.tile_first_row("a"), 70 - tile(&app, "a").1, "drawn from its bottom meanwhile");
        // A click into the tile takes it back.
        let (_, _, inner) = app.layout.tiles[0].clone();
        app.on_mouse(crate::input::MouseEvent::at(crate::input::MouseEventKind::Down(crate::input::MouseButton::Left), inner.x as f32 + 1.5, inner.y as f32 + 1.5, crate::input::Modifiers::NONE));
        draw(&mut app);
        assert!(sizes(&sent).contains(&ClientMsg::Claim { id: "a".into() }));
        assert!(app.size_note("a").is_none());
    }

    #[test]
    fn a_window_drag_sends_one_resize_once_the_size_holds() {
        let (mut app, sent) = rig(&["a"]);
        alive(&mut app, "a");
        app.select_node("a", false);
        app.show_terminal("a");
        draw(&mut app);
        let _ = sizes(&sent);
        let start = Instant::now();
        for (i, width) in [150u16, 140, 130, 120].into_iter().enumerate() {
            let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, width, 50));
            crate::ui::render(&mut app, &mut buf, Rect::new(0, 0, width, 50), None);
            app.sync_sizes(start + Duration::from_millis(20 * i as u64));
        }
        assert!(sizes(&sent).is_empty(), "nothing while the size changes");
        assert!(app.sizes_pending());
        app.sync_sizes(start + Duration::from_millis(60) + RESIZE_SETTLE);
        let want = tile(&app, "a");
        assert_eq!(sizes(&sent), [ClientMsg::Resize { id: "a".into(), cols: want.0, rows: want.1 }]);
        assert!(!app.sizes_pending());
    }

    #[test]
    fn a_launch_from_the_sidebar_gets_the_panels_size_not_a_guess() {
        let (mut app, sent) = rig(&["a", "b"]);
        // Nothing selected: no right panel on screen.
        app.selection.clear();
        draw(&mut app);
        assert_eq!(app.layout.terms.width, 0);
        let shown = app.layout.terms_shown;
        assert!(shown.width > 20);
        app.start_node("a");
        let launched = sizes(&sent);
        assert!(matches!(launched.as_slice(), [ClientMsg::Launch { cols, rows, .. }] if (*cols, *rows) == tile_size(tile_inner(shown))), "{launched:?} vs {shown:?}");
    }
    /// "a" shown with the keyboard here, running at its tile's size and claimed.
    fn claimed(ids: &[&str]) -> (App, Receiver<ClientMsg>) {
        let (mut app, sent) = rig(ids);
        alive(&mut app, "a");
        app.select_node("a", false);
        app.show_terminal("a");
        app.set_focus(Focus::Terminal);
        draw(&mut app);
        draw(&mut app);
        let want = tile(&app, "a");
        frame(&mut app, "a", want.0, want.1);
        draw(&mut app);
        assert!(sizes(&sent).contains(&ClientMsg::Claim { id: "a".into() }));
        assert_eq!(app.size_states.get("a"), Some(&SizeState::Matched));
        (app, sent)
    }

    fn claims(sent: &Receiver<ClientMsg>) -> usize {
        sizes(sent).iter().filter(|m| matches!(m, ClientMsg::Claim { .. })).count()
    }

    #[test]
    fn a_terminal_started_elsewhere_while_the_keyboard_is_on_its_tile_is_not_claimed() {
        let (mut app, sent) = claimed(&["a"]);
        // It exits; you keep looking at it; the phone (or the restore) starts it again.
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: false, exit_code: Some(0), ..Default::default() }]));
        draw(&mut app);
        alive(&mut app, "a");
        draw(&mut app);
        let sent_now = sizes(&sent);
        assert!(sent_now.iter().any(|m| matches!(m, ClientMsg::Resize { id, .. } if id == "a")), "its size goes out as a wish: {sent_now:?}");
        assert!(!sent_now.contains(&ClientMsg::Claim { id: "a".into() }), "not taken from whoever started it (R2-D-04): {sent_now:?}");
        // Started from this window: claimed once it runs.
        app.on_service(ServiceMsg::Terminals(vec![TermInfo { id: "a".into(), alive: false, exit_code: Some(0), ..Default::default() }]));
        draw(&mut app);
        app.start_node("a");
        draw(&mut app);
        let _ = sizes(&sent);
        alive(&mut app, "a");
        draw(&mut app);
        assert_eq!(claims(&sent), 1, "a start from here takes it");
    }

    #[test]
    fn a_taken_terminal_is_never_claimed_back_by_a_repair_or_a_reconnect() {
        let (mut app, sent) = claimed(&["a"]);
        // The phone takes it, then this window's tile changes size: the new
        // size goes out as a wish, repairs follow, but never a claim.
        frame(&mut app, "a", 60, 70);
        draw(&mut app);
        assert_eq!(app.size_states.get("a"), Some(&SizeState::Taken));
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 140, 50));
        crate::ui::render(&mut app, &mut buf, Rect::new(0, 0, 140, 50), None);
        let start = Instant::now();
        app.sync_sizes(start);
        app.sync_sizes(start + RESIZE_SETTLE);
        assert!(sizes(&sent).iter().any(|m| matches!(m, ClientMsg::Resize { .. })));
        frame(&mut app, "a", 60, 70);
        for step in 1..8u32 {
            app.sync_sizes(start + RESIZE_SETTLE + REPAIR_MAX * step);
        }
        let later = sizes(&sent);
        assert!(later.iter().any(|m| matches!(m, ClientMsg::Resize { .. })) && !later.iter().any(|m| matches!(m, ClientMsg::Claim { .. })), "{later:?}");
        // A reconnect sends the size again as a wish, without a claim (R2-E-09) …
        app.reset_service_state();
        draw(&mut app);
        let again = sizes(&sent);
        assert!(again.iter().any(|m| matches!(m, ClientMsg::Resize { .. })) && !again.iter().any(|m| matches!(m, ClientMsg::Claim { .. })), "{again:?}");
        // … and coming back to the window claims.
        app.window_focused(true);
        draw(&mut app);
        assert_eq!(claims(&sent), 1);
    }

    #[test]
    fn a_resize_undone_is_repaired_not_taken() {
        let (mut app, sent) = claimed(&["a"]);
        let want = tile(&app, "a");
        // The window narrows: a frame at the new size slips out, then the PTY
        // refuses it and the engine goes back to the size it had.
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 140, 50));
        crate::ui::render(&mut app, &mut buf, Rect::new(0, 0, 140, 50), None);
        let start = Instant::now();
        app.sync_sizes(start);
        app.sync_sizes(start + RESIZE_SETTLE);
        let narrow = tile(&app, "a");
        assert_ne!(narrow, want);
        let _ = sizes(&sent);
        frame(&mut app, "a", narrow.0, narrow.1);
        app.sync_sizes(start + RESIZE_SETTLE);
        assert_eq!(app.size_states.get("a"), Some(&SizeState::Matched));
        frame(&mut app, "a", want.0, want.1);
        app.sync_sizes(start + RESIZE_SETTLE);
        let since = match app.size_states.get("a") {
            Some(SizeState::Differs { since, .. }) => *since,
            other => panic!("a revert is not another client's size: {other:?}"),
        };
        assert!(app.size_note("a").is_none());
        app.sync_sizes(since + SIZE_GRACE);
        assert!(sizes(&sent).contains(&ClientMsg::Resize { id: "a".into(), cols: narrow.0, rows: narrow.1 }), "repaired");
    }
}
