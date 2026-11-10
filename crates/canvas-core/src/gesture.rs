//! Trackpad and touchpad gestures (MAC-LINUX-PLAN D15): pixel-precise
//! two-finger scrolling with the system's own momentum, pinch to zoom,
//! smart zoom (a two-finger double tap) and force click, as a native Mac app
//! has them. Front ends translate their platform's events into
//! [`Gesture`]s (fractional cells); everything they do is decided here.
//!
//! - **Latching.** A scroll or pinch goes to what is under the pointer when
//!   it begins (canvas, a terminal, the sidebar, a dialog) and stays there for
//!   the whole gesture, the momentum after lift-off included: moving across a
//!   terminal mid-gesture never hands the scroll over.
//! - **Canvas.** Scrolling pans pixel for pixel on both axes (the sign of the
//!   deltas carries the user's natural-scrolling setting); the canvas's own
//!   glide (`glide.rs`) never runs on top, the system's momentum is used
//!   instead. Command (Ctrl, Alt) + scroll and pinching zoom continuously
//!   around the pointer within the canvas's limits; a pinch that ends close
//!   to a zoom level (25 %, 50 %, 100 % …) settles on it.
//! - **Terminals.** Scrollback moves line by line from accumulated deltas
//!   (the fraction carries over, so it is never jumpy); programs that read
//!   the mouse get one wheel notch per three lines of movement, full-screen
//!   programs without mouse support one per half screen (they page). Shift
//!   always scrolls the scrollback. Pinching over a terminal pane changes the
//!   text size.
//! - **Smart zoom** over a box fits that box; over empty canvas it toggles
//!   between everything in view and the view before. Animated, unless motion
//!   is reduced.
//! - **Force click** over a terminal link opens it (as Command-click does);
//!   over a box it shows the box's details.
//! - **Notebook and Tasks.** Their lists, notes, info panel and details
//!   scroll row by row; the todo canvas pans cell by cell on both axes (the
//!   fractions carry over), pinching or Command + scroll steps its zoom
//!   levels and smart zoom fits every board. The session canvas behind them
//!   never moves.

use std::time::{Duration, Instant};

use crate::app::{App, CanvasHit, Focus, MAX_SCALE, MIN_SCALE};
use crate::paint::rect_contains;
use crate::input::{Gesture, GestureKind, GesturePhase, KeyCode, KeyEvent, Modifiers};
use crate::protocol::{ClientMsg, MODE_ALT_SCREEN, MODE_MOUSE};
use crate::spaces::Space;
use crate::view::{Motion, RenderMode};

/// Fingers lifted: the system's momentum begins within this, on the same target.
const MOMENTUM_GAP: Duration = Duration::from_millis(150);
/// A gesture that sends nothing for this long is over (devices without phases).
const STALE: Duration = Duration::from_millis(350);
/// Lines of movement per wheel notch for programs that read the mouse.
const NOTCH_LINES: f32 = 3.0;
/// Rows of Command+scroll that double (or halve) the zoom.
const ZOOM_SCROLL_ROWS: f32 = 16.0;
/// A pinch ending within this (in powers of two) of a zoom level settles on it.
const SNAP: f32 = 0.08;
/// Pinching this far (in powers of two) changes the text size by one step.
const TEXT_STEP: f32 = 0.2;
/// How long a smart zoom takes.
const SMART_ZOOM: Duration = Duration::from_millis(260);
/// Pinching (or Command + scrolling) this far, in powers of two, steps the
/// todo canvas's zoom one level.
const BOARD_LEVEL: f32 = 0.5;

/// What a gesture is latched to.
#[derive(Clone, Debug, PartialEq)]
pub enum Latched {
    Canvas,
    Terminal(String),
    Sidebar,
    Dialog,
    /// The Notebook's or Tasks' area, at the cell the gesture began over
    /// (the explorer, a note, the info panel; the sidebar, the details, the
    /// todo canvas).
    Space(u16, u16),
    /// A pinch over a terminal pane: the text size.
    TextSize,
    Nothing,
}

/// The gesture in progress.
#[derive(Clone, Debug)]
pub struct GestureLatch {
    pub target: Latched,
    pinch: bool,
    last: Instant,
    /// When the fingers (or the momentum) last lifted.
    lifted: Option<Instant>,
    /// Movement owed but not yet a whole line, row or notch.
    rest: f32,
    /// Pinch movement (powers of two) not yet a text size step.
    zoom_rest: f32,
    /// The TUI's canvas: world units owed but not yet whole.
    pan_rest: (f32, f32),
}

/// The canvas view: its top-left world point and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewState {
    pub origin: (f32, f32),
    pub scale: f32,
}

/// A view moving from one state to another (smart zoom).
#[derive(Clone, Debug)]
pub struct ViewAnim {
    start: Instant,
    from: ViewState,
    to: ViewState,
}

impl ViewAnim {
    /// The view `t` into the animation (ease-out), and whether it is done.
    fn at(&self, now: Instant) -> (ViewState, bool) {
        let t = (now.saturating_duration_since(self.start).as_secs_f32() / SMART_ZOOM.as_secs_f32()).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        // The scale moves evenly in powers of two; the world point at the
        // canvas's centre moves in a straight line.
        let scale = 2f32.powf(lerp(self.from.scale.log2(), self.to.scale.log2()));
        let origin = (lerp(self.from.origin.0, self.to.origin.0), lerp(self.from.origin.1, self.to.origin.1));
        (ViewState { origin, scale }, t >= 1.0)
    }
}

impl App {
    pub fn on_gesture(&mut self, gesture: Gesture) {
        self.on_gesture_at(gesture, Instant::now());
    }

    pub(crate) fn on_gesture_at(&mut self, gesture: Gesture, now: Instant) {
        self.pointer = (gesture.x, gesture.y);
        self.needs_draw = true;
        match gesture.kind {
            GestureKind::Scroll { dx, dy, phase } => {
                let target = self.latch(false, gesture.x, gesture.y, phase, now);
                self.scroll_gesture(target, dx, dy, gesture, now);
            }
            GestureKind::Pinch { delta, phase } => {
                let target = self.latch(true, gesture.x, gesture.y, phase, now);
                self.pinch_gesture(target, delta, gesture, phase);
            }
            GestureKind::SmartZoom => self.smart_zoom_at(gesture.x, gesture.y, now),
            GestureKind::ForceClick => self.force_click(gesture.x, gesture.y),
        }
    }

    /// What a new gesture at `(x, y)` goes to.
    fn gesture_target(&self, pinch: bool, x: f32, y: f32) -> Latched {
        let (cx, cy) = (x.max(0.0) as u16, y.max(0.0) as u16);
        if !self.modals.is_empty() {
            return if pinch { Latched::Nothing } else { Latched::Dialog };
        }
        // The Notebook and Tasks: their areas, never the session canvas behind them.
        if self.space != Space::Canvas {
            return if self.in_space_area(cx, cy) { Latched::Space(cx, cy) } else { Latched::Nothing };
        }
        if rect_contains(self.layout.sidebar, cx, cy) {
            return if pinch { Latched::Nothing } else { Latched::Sidebar };
        }
        if let Some((id, _, _)) = self.layout.tiles.iter().find(|(_, outer, _)| rect_contains(*outer, cx, cy)) {
            return if pinch { Latched::TextSize } else { Latched::Terminal(id.clone()) };
        }
        if rect_contains(self.layout.canvas, cx, cy) {
            return Latched::Canvas;
        }
        Latched::Nothing
    }

    /// The target of this event: the latched one while the gesture (or its
    /// momentum) goes on, else what is under the pointer now.
    fn latch(&mut self, pinch: bool, x: f32, y: f32, phase: GesturePhase, now: Instant) -> Latched {
        let keep = self.gesture.as_ref().is_some_and(|l| {
            l.pinch == pinch
                && match phase {
                    // Momentum begins right after the fingers lift.
                    GesturePhase::Began => l.lifted.is_some_and(|t| now.saturating_duration_since(t) <= MOMENTUM_GAP),
                    _ => now.saturating_duration_since(l.last) <= STALE,
                }
        });
        if !keep {
            let target = self.gesture_target(pinch, x, y);
            if target == Latched::Canvas {
                self.stop_glide();
                self.view_anim = None;
            }
            self.gesture = Some(GestureLatch { target, pinch, last: now, lifted: None, rest: 0.0, zoom_rest: 0.0, pan_rest: (0.0, 0.0) });
        }
        let latch = self.gesture.as_mut().expect("latched");
        latch.last = now;
        latch.lifted = match phase {
            GesturePhase::Ended => Some(now),
            _ => None,
        };
        latch.target.clone()
    }

    /// Movement owed to the latched target plus `delta`, as whole units of
    /// `per`; the fraction stays for the next event.
    fn take_units(&mut self, delta: f32, per: f32) -> i32 {
        let Some(latch) = self.gesture.as_mut() else { return 0 };
        // A change of direction starts afresh.
        if latch.rest * delta < 0.0 {
            latch.rest = 0.0;
        }
        latch.rest += delta / per;
        let whole = latch.rest.trunc();
        latch.rest -= whole;
        whole as i32
    }

    fn scroll_gesture(&mut self, target: Latched, dx: f32, dy: f32, gesture: Gesture, now: Instant) {
        match target {
            Latched::Canvas => {
                if gesture.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT) {
                    // Command (or Ctrl, Alt) + scroll: continuous zoom around the pointer.
                    return self.zoom_around(gesture.x, gesture.y, 2f32.powf(dy / ZOOM_SCROLL_ROWS));
                }
                self.pan_quiet_until = Some(now + Duration::from_millis(200));
                self.pan_exact(dx, dy);
            }
            Latched::Terminal(id) => self.terminal_scroll(&id, dy, gesture),
            Latched::Sidebar => {
                let rows = self.take_units(dy, 1.0);
                if rows == 0 {
                    return;
                }
                if self.side_view == crate::side::SideView::Sessions {
                    self.side_scroll = (self.side_scroll as i64 - rows as i64).max(0) as usize;
                } else {
                    self.list_move(-rows as i64);
                }
            }
            Latched::Dialog => {
                let rows = self.take_units(dy, 1.0);
                let key = KeyEvent::new(if rows > 0 { KeyCode::Up } else { KeyCode::Down }, Modifiers::NONE);
                if !matches!(self.modals.last(), Some(crate::dialogs::Modal::NewSession(_) | crate::dialogs::Modal::Confirm(_) | crate::dialogs::Modal::Rename(_))) {
                    for _ in 0..rows.abs() {
                        self.modal_key(key);
                    }
                }
            }
            Latched::Space(x, y) => self.space_scroll(x, y, dx, dy, gesture.modifiers),
            Latched::TextSize | Latched::Nothing => {}
        }
    }

    /// A scroll over the Notebook's or Tasks' area: lists and notes move row
    /// by row, the todo canvas cell by cell on both axes (the content
    /// follows the fingers); Command (Ctrl, Alt) + scroll steps the board's zoom.
    fn space_scroll(&mut self, x: u16, y: u16, dx: f32, dy: f32, modifiers: Modifiers) {
        if self.space == Space::Tasks && crate::tasks::board_at(self, x, y) {
            if modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT) {
                return self.board_zoom(dy / ZOOM_SCROLL_ROWS);
            }
            let Some(latch) = self.gesture.as_mut() else { return };
            latch.pan_rest.0 += dx;
            latch.pan_rest.1 += dy;
            let (cx, cy) = (latch.pan_rest.0.trunc(), latch.pan_rest.1.trunc());
            latch.pan_rest.0 -= cx;
            latch.pan_rest.1 -= cy;
            if cx != 0.0 || cy != 0.0 {
                crate::tasks::pan_board(self, -cx as i32, -cy as i32);
            }
            return;
        }
        // Positive: towards the top (what is above comes into view).
        let rows = self.take_units(dy, 1.0);
        if rows == 0 {
            return;
        }
        match self.space {
            Space::Notebook => crate::notebook::scroll(self, x, y, -rows),
            Space::Tasks => crate::tasks::scroll(self, x, y, -rows),
            Space::Canvas => {}
        }
    }

    /// Steps the todo canvas's zoom once `by` (powers of two) adds up to a level.
    fn board_zoom(&mut self, by: f32) {
        let Some(latch) = self.gesture.as_mut() else { return };
        latch.zoom_rest += by;
        if latch.zoom_rest.abs() >= BOARD_LEVEL {
            let step = latch.zoom_rest.signum() as i32;
            latch.zoom_rest = 0.0;
            crate::tasks::zoom_board(self, step);
        }
    }

    /// Pans the canvas by exactly `(dx, dy)` cells (the TUI's whole-cell
    /// view moves in whole world units; the fraction waits).
    fn pan_exact(&mut self, dx: f32, dy: f32) {
        if self.render_mode == RenderMode::Gpu {
            return self.pan_cells(dx, dy);
        }
        let (n, d) = self.scale();
        let k = d as f32 / n as f32;
        let Some(latch) = self.gesture.as_mut() else { return };
        latch.pan_rest.0 += dx * k;
        latch.pan_rest.1 += dy * k;
        let (wx, wy) = (latch.pan_rest.0.trunc(), latch.pan_rest.1.trunc());
        latch.pan_rest.0 -= wx;
        latch.pan_rest.1 -= wy;
        if wx != 0.0 || wy != 0.0 {
            self.doc.view.x -= wx as i32;
            self.doc.view.y -= wy as i32;
            self.mark_dirty();
        }
    }

    /// Scrollback or wheel notches for the latched terminal.
    fn terminal_scroll(&mut self, id: &str, dy: f32, gesture: Gesture) {
        let Some(view) = self.views.get(id) else { return };
        let (modes, rows) = (view.modes, view.rows);
        if !self.terms.contains_key(id) {
            return;
        }
        let shift = gesture.modifiers.contains(Modifiers::SHIFT);
        let program = !shift && modes & (MODE_MOUSE | MODE_ALT_SCREEN) != 0;
        if !program {
            // The scrollback, line by line (positive: towards older lines).
            let lines = self.take_units(dy, 1.0);
            if lines != 0 {
                self.send(ClientMsg::Scroll { id: id.to_owned(), delta: lines });
            }
            return;
        }
        // Programs: wheel notches at a pace they handle (a full-screen
        // program without mouse support pages once per half screen).
        let per = if modes & MODE_MOUSE != 0 { NOTCH_LINES } else { (rows as f32 / 2.0).max(NOTCH_LINES) };
        let notches = self.take_units(dy, per);
        if notches == 0 {
            return;
        }
        let inner = self.layout.tiles.iter().find(|(t, _, _)| t == id).map(|(_, _, inner)| *inner);
        let (col, row) = inner.map_or((0, 0), |r| {
            let (x, y) = (gesture.x.max(0.0) as u16, gesture.y.max(0.0) as u16);
            (x.saturating_sub(r.x).min(r.width.saturating_sub(1)), y.saturating_sub(r.y).min(r.height.saturating_sub(1)))
        });
        for _ in 0..notches.unsigned_abs() {
            self.send(ClientMsg::Wheel { id: id.to_owned(), up: notches > 0, col, row, mods: crate::input::mods(gesture.modifiers) });
        }
    }

    fn pinch_gesture(&mut self, target: Latched, delta: f32, gesture: Gesture, phase: GesturePhase) {
        let factor = (1.0 + delta).max(0.05);
        match target {
            Latched::Canvas => {
                self.zoom_around(gesture.x, gesture.y, factor);
                if phase == GesturePhase::Ended {
                    self.settle_zoom(gesture.x, gesture.y);
                }
            }
            Latched::TextSize => {
                let Some(latch) = self.gesture.as_mut() else { return };
                latch.zoom_rest += factor.log2();
                let steps = (latch.zoom_rest / TEXT_STEP).trunc();
                latch.zoom_rest -= steps * TEXT_STEP;
                self.text_zoom_steps += steps as i32;
            }
            Latched::Space(x, y) if self.space == Space::Tasks && crate::tasks::board_at(self, x, y) => self.board_zoom(factor.log2()),
            _ => {}
        }
    }

    /// Zooms by `factor` keeping the point under the pointer in place, within
    /// the canvas's limits (the TUI steps between its levels).
    fn zoom_around(&mut self, x: f32, y: f32, factor: f32) {
        if self.render_mode == RenderMode::Gpu {
            return self.zoom_by(x, y, factor);
        }
        let Some(latch) = self.gesture.as_mut() else { return };
        latch.zoom_rest += factor.log2();
        if latch.zoom_rest.abs() >= 1.0 {
            let step = if latch.zoom_rest > 0.0 { -1 } else { 1 };
            latch.zoom_rest = 0.0;
            self.zoom_at(x as i32, y as i32, step);
        }
    }

    /// A pinch that ends close to a zoom level (a power of two) settles on it.
    fn settle_zoom(&mut self, x: f32, y: f32) {
        if self.render_mode != RenderMode::Gpu {
            return;
        }
        let level = self.view_scale.log2();
        let nearest = level.round();
        if (level - nearest).abs() < SNAP {
            self.zoom_to(x, y, 2f32.powf(nearest).clamp(MIN_SCALE, MAX_SCALE));
        }
    }

    fn view_state(&self) -> ViewState {
        ViewState { origin: self.origin_f(), scale: self.view_scale }
    }

    fn set_view_state(&mut self, state: ViewState) {
        self.set_scale(state.scale);
        self.set_origin(state.origin.0, state.origin.1);
        self.mark_dirty();
    }

    /// Smart zoom (a two-finger double tap) at a point.
    pub(crate) fn smart_zoom_at(&mut self, x: f32, y: f32, now: Instant) {
        let (cx, cy) = (x.max(0.0) as u16, y.max(0.0) as u16);
        // Over the todo canvas: every board in view.
        if self.space == Space::Tasks && self.modals.is_empty() && crate::tasks::board_at(self, cx, cy) {
            return crate::tasks::fit_board(self);
        }
        if self.render_mode != RenderMode::Gpu || !self.modals.is_empty() || self.space != Space::Canvas || !rect_contains(self.layout.canvas, cx, cy) {
            return;
        }
        self.stop_glide();
        let current = self.view_anim.as_ref().map(|a| a.to).unwrap_or_else(|| self.view_state());
        let target = match self.canvas_hit_f((x, y)) {
            CanvasHit::Node(id) => {
                let Some(rect) = self.doc.node(&id).map(|n| n.rect()) else { return };
                let fitted = self.fitted(rect);
                // Already fitted to it: back to where the zooming started.
                match self.smart_back {
                    Some(back) if close(current, fitted) => {
                        self.smart_back = None;
                        back
                    }
                    _ => {
                        self.smart_back.get_or_insert(current);
                        fitted
                    }
                }
            }
            _ => match self.smart_back.take() {
                Some(back) => back,
                None => {
                    self.smart_back = Some(current);
                    // Everything in view, as Show all nodes computes it.
                    let saved = self.view_state();
                    self.fit_all();
                    let all = self.view_state();
                    self.set_view_state(saved);
                    all
                }
            },
        };
        self.animate_view(target, now);
    }

    /// The view that shows `rect` (world units) whole, centred, with a margin.
    fn fitted(&self, rect: crate::doc::Rect) -> ViewState {
        let c = self.layout.canvas;
        let (width, height) = ((c.width as f32 - 8.0).max(4.0), (c.height as f32 - 4.0).max(2.0));
        let scale = (width / rect.w.max(1) as f32).min(height / rect.h.max(1) as f32).clamp(MIN_SCALE, MAX_SCALE);
        let (mx, my) = (rect.x as f32 + rect.w as f32 / 2.0, rect.y as f32 + rect.h as f32 / 2.0);
        let origin = (mx - c.width.max(20) as f32 / scale / 2.0, my - c.height.max(10) as f32 / scale / 2.0);
        ViewState { origin, scale }
    }

    /// Moves the view to `target`: animated, or at once when motion is reduced.
    fn animate_view(&mut self, target: ViewState, now: Instant) {
        if self.motion() != Motion::On {
            self.view_anim = None;
            return self.set_view_state(target);
        }
        self.view_anim = Some(ViewAnim { start: now, from: self.view_state(), to: target });
        self.needs_draw = true;
    }

    /// Advances a smart-zoom animation (front ends call it every frame while
    /// `view_anim` is set).
    pub fn step_view_anim(&mut self) {
        self.step_view_anim_at(Instant::now());
    }

    pub(crate) fn step_view_anim_at(&mut self, now: Instant) {
        let Some(anim) = self.view_anim.take() else { return };
        let (state, done) = anim.at(now);
        self.set_view_state(state);
        self.needs_draw = true;
        if !done {
            self.view_anim = Some(anim);
        }
    }

    /// A force click: a terminal link opens (as with Command-click), a box
    /// shows its details.
    fn force_click(&mut self, x: f32, y: f32) {
        let (cx, cy) = (x.max(0.0) as u16, y.max(0.0) as u16);
        if !self.modals.is_empty() {
            return;
        }
        if let Some(link) = self.layout.links.iter().find(|l| l.contains(cx, cy)).cloned() {
            if link.terminal && self.guest_link(&link.target, cx, cy) {
                return;
            }
            return self.open_link(&link.target);
        }
        if self.space == Space::Canvas
            && rect_contains(self.layout.canvas, cx, cy)
            && let CanvasHit::Node(id) = self.canvas_hit(cx, cy)
        {
            self.select_node(&id, false);
            self.set_focus(Focus::Canvas);
        }
    }
}

/// Two views that show the same thing.
fn close(a: ViewState, b: ViewState) -> bool {
    (a.scale.log2() - b.scale.log2()).abs() < 0.02 && (a.origin.0 - b.origin.0).abs() < 0.5 && (a.origin.1 - b.origin.1).abs() < 0.5
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::layout::Rect;

    use super::*;
    use crate::input::{Gesture, GestureKind, GesturePhase::*, InputEvent, Modifiers};
    use crate::protocol::{ClientMsg, MODE_ALT_SCREEN, MODE_MOUSE};
    use crate::undo::fixture::app;
    use crate::view::Motion;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn scroll(dx: f32, dy: f32, phase: crate::input::GesturePhase, at: (f32, f32)) -> Gesture {
        Gesture { kind: GestureKind::Scroll { dx, dy, phase }, x: at.0, y: at.1, modifiers: Modifiers::NONE }
    }

    fn pinch(delta: f32, phase: crate::input::GesturePhase, at: (f32, f32)) -> Gesture {
        Gesture { kind: GestureKind::Pinch { delta, phase }, x: at.0, y: at.1, modifiers: Modifiers::NONE }
    }

    /// A window with a canvas (x < 100), a terminal pane "t" (x ≥ 100) whose
    /// program has `modes`, and a sidebar is absent.
    fn split(modes: u32) -> (App, crossbeam_channel::Receiver<ClientMsg>) {
        let (mut app, sent) = app(true);
        app.layout.canvas = Rect::new(0, 0, 100, 60);
        app.layout.tiles = vec![("t".into(), Rect::new(100, 0, 100, 60), Rect::new(101, 1, 98, 58))];
        app.terms.insert("t".into(), crate::protocol::TermInfo { id: "t".into(), alive: true, ..Default::default() });
        app.views.insert("t".into(), canvas_termview::TermView { rows: 58, cols: 98, modes, ..Default::default() });
        while sent.try_recv().is_ok() {}
        (app, sent)
    }

    fn scrolls(sent: &crossbeam_channel::Receiver<ClientMsg>) -> (i32, i32) {
        let (mut lines, mut notches) = (0, 0);
        for msg in sent.try_iter() {
            match msg {
                ClientMsg::Scroll { delta, .. } => lines += delta,
                ClientMsg::Wheel { up, .. } => notches += if up { 1 } else { -1 },
                _ => {}
            }
        }
        (lines, notches)
    }

    #[test]
    fn two_finger_scrolling_pans_the_canvas_exactly_and_never_glides() {
        let (mut app, _) = app(true);
        app.visual.motion = Some(Motion::On);
        let t0 = Instant::now();
        let start = app.origin_f();
        // Fingers right and down: the content follows them (the view's origin goes left and up).
        app.on_gesture_at(scroll(0.0, 0.0, Began, (150.0, 50.0)), t0);
        for i in 1..=10 {
            app.on_gesture_at(scroll(1.5, 0.75, Changed, (150.0, 50.0)), t0 + ms(i * 8));
        }
        app.on_gesture_at(scroll(0.0, 0.0, Ended, (150.0, 50.0)), t0 + ms(90));
        let moved = (start.0 - app.origin_f().0, start.1 - app.origin_f().1);
        assert!((moved.0 - 15.0).abs() < 1e-3 && (moved.1 - 7.5).abs() < 1e-3, "pixel for pixel on both axes: {moved:?}");
        assert!(app.glide.is_none(), "the canvas's own glide never runs on a trackpad pan");
        // The system's momentum continues the same gesture, decaying.
        app.on_gesture_at(scroll(1.0, 0.0, Began, (150.0, 50.0)), t0 + ms(95));
        app.on_gesture_at(scroll(0.5, 0.0, Changed, (150.0, 50.0)), t0 + ms(110));
        app.on_gesture_at(scroll(0.0, 0.0, Ended, (150.0, 50.0)), t0 + ms(130));
        assert!((start.0 - app.origin_f().0 - 16.5).abs() < 1e-3);
        assert!(app.glide.is_none());
        // Command (Ctrl after the key policy) + scroll zooms around the pointer.
        let before = app.view_scale;
        let mut zoom = scroll(0.0, ZOOM_SCROLL_ROWS, Began, (50.0, 30.0));
        zoom.modifiers = Modifiers::CONTROL;
        app.on_gesture_at(zoom, t0 + ms(2000));
        assert!((app.view_scale / before - 2.0).abs() < 1e-3, "{} → {}", before, app.view_scale);
    }

    #[test]
    fn a_gesture_stays_latched_to_what_it_began_over_momentum_included() {
        let (mut app, sent) = split(0);
        let t0 = Instant::now();
        let origin = app.origin_f();
        // Begins over the canvas, moves across the terminal: still the canvas.
        app.on_gesture_at(scroll(0.0, 2.0, Began, (50.0, 30.0)), t0);
        app.on_gesture_at(scroll(0.0, 2.0, Changed, (150.0, 30.0)), t0 + ms(10));
        app.on_gesture_at(scroll(0.0, 0.0, Ended, (150.0, 30.0)), t0 + ms(20));
        // The momentum that follows at once stays on the canvas too.
        app.on_gesture_at(scroll(0.0, 3.0, Began, (150.0, 30.0)), t0 + ms(40));
        app.on_gesture_at(scroll(0.0, 0.0, Ended, (150.0, 30.0)), t0 + ms(60));
        assert_eq!(scrolls(&sent), (0, 0), "nothing reached the terminal");
        assert!((origin.1 - app.origin_f().1 - 7.0).abs() < 1e-3);
        // A new gesture a moment later goes to what is under the pointer now.
        app.on_gesture_at(scroll(0.0, 3.0, Began, (150.0, 30.0)), t0 + ms(600));
        assert_eq!(scrolls(&sent), (3, 0), "the terminal's scrollback");
        // And momentum over the terminal never overshoots into the canvas.
        let origin = app.origin_f();
        app.on_gesture_at(scroll(0.0, 0.0, Ended, (50.0, 30.0)), t0 + ms(610));
        app.on_gesture_at(scroll(0.0, -50.0, Began, (50.0, 30.0)), t0 + ms(620));
        assert_eq!(app.origin_f(), origin);
        assert_eq!(scrolls(&sent), (-50, 0));
    }

    #[test]
    fn terminal_scrollback_accumulates_fractions_and_mouse_programs_get_notches() {
        let (mut app, sent) = split(0);
        let t0 = Instant::now();
        app.on_gesture_at(scroll(0.0, 0.4, Began, (150.0, 30.0)), t0);
        for i in 1..=9 {
            app.on_gesture_at(scroll(0.0, 0.4, Changed, (150.0, 30.0)), t0 + ms(i * 8));
        }
        assert_eq!(scrolls(&sent), (4, 0), "4 whole lines out of 4.0, never a jump");
        // A program that reads the mouse: one notch per three lines.
        let (mut app, sent) = split(MODE_MOUSE);
        app.on_gesture_at(scroll(0.0, 10.0, Began, (150.0, 30.0)), t0);
        assert_eq!(scrolls(&sent), (0, 3));
        app.on_gesture_at(scroll(0.0, -2.5, Changed, (150.0, 30.0)), t0 + ms(8));
        assert_eq!(scrolls(&sent), (0, 0), "a change of direction starts afresh, under a notch");
        app.on_gesture_at(scroll(0.0, -1.0, Changed, (150.0, 30.0)), t0 + ms(16));
        assert_eq!(scrolls(&sent), (0, -1));
        // Shift always scrolls the scrollback.
        let mut shifted = scroll(0.0, 2.0, Changed, (150.0, 30.0));
        shifted.modifiers = Modifiers::SHIFT;
        app.on_gesture_at(shifted, t0 + ms(24));
        assert_eq!(scrolls(&sent).0, 2);
        // A full-screen program without the mouse pages once per half screen.
        let (mut app, sent) = split(MODE_ALT_SCREEN);
        app.on_gesture_at(scroll(0.0, 60.0, Began, (150.0, 30.0)), t0);
        assert_eq!(scrolls(&sent), (0, 2));
    }

    #[test]
    fn pinching_zooms_around_the_pointer_within_the_limits_and_settles_on_levels() {
        let (mut app, _) = app(true);
        let t0 = Instant::now();
        let at = (40.0, 20.0);
        let world = app.to_world_f(at.0, at.1);
        app.on_gesture_at(pinch(0.2, Began, at), t0);
        app.on_gesture_at(pinch(0.2, Changed, at), t0 + ms(10));
        let after = app.to_world_f(at.0, at.1);
        assert!((world.0 - after.0).abs() < 1e-3 && (world.1 - after.1).abs() < 1e-3, "the point under the fingers stays");
        assert!((app.view_scale - 1.44).abs() < 1e-3);
        // Ends near 150 %: stays there; ends near 200 %: settles on it.
        app.on_gesture_at(pinch(0.0, Ended, at), t0 + ms(20));
        assert!((app.view_scale - 1.44).abs() < 1e-3);
        app.on_gesture_at(pinch(0.36, Began, at), t0 + ms(1000));
        app.on_gesture_at(pinch(0.0, Ended, at), t0 + ms(1010));
        assert_eq!(app.view_scale, 2.0, "within reach of 200 %: settled");
        for i in 0..40 {
            app.on_gesture_at(pinch(0.5, Changed, at), t0 + ms(1020 + i));
        }
        assert_eq!(app.view_scale, MAX_SCALE);
        for i in 0..80 {
            app.on_gesture_at(pinch(-0.5, Changed, at), t0 + ms(1100 + i));
        }
        assert_eq!(app.view_scale, MIN_SCALE);
        // Over a terminal pane: the text size, one step per fifth of a doubling.
        let (mut app, _) = split(0);
        app.on_gesture_at(pinch(0.0, Began, (150.0, 30.0)), t0);
        app.on_gesture_at(pinch(0.32, Changed, (150.0, 30.0)), t0 + ms(10));
        assert_eq!(app.text_zoom_steps, 2);
        assert_eq!(app.view_scale, 1.0, "the canvas did not zoom");
    }

    #[test]
    fn smart_zoom_fits_a_box_or_everything_and_back_animated_unless_reduced() {
        let (mut app, _) = app(true);
        app.visual.motion = Some(Motion::On);
        let t0 = Instant::now();
        let initial = app.view_state();
        // Over box "p" (at 2,2): it fills the canvas, animated.
        app.smart_zoom_at(5.0, 3.0, t0);
        assert!(app.view_anim.is_some());
        app.step_view_anim_at(t0 + ms(100));
        let midway = app.view_state();
        assert!(midway.scale > initial.scale, "zooming in");
        app.step_view_anim_at(t0 + ms(1000));
        assert!(app.view_anim.is_none());
        let fitted = app.view_state();
        assert_eq!(fitted.scale, MAX_SCALE, "a small box: as close as the canvas goes");
        let node = app.doc.node("p").unwrap().rect();
        let (cx, cy) = app.to_screen_f(node.x as f32 + node.w as f32 / 2.0, node.y as f32 + node.h as f32 / 2.0);
        assert!((cx - 100.0).abs() < 1.0 && (cy - 30.0).abs() < 1.0, "centred: {cx},{cy}");
        // Again over the same box: back to where it started.
        app.smart_zoom_at(100.0, 30.0, t0 + ms(2000));
        app.step_view_anim_at(t0 + ms(3000));
        assert!(close(app.view_state(), initial), "{:?} {:?}", app.view_state(), initial);
        // Empty canvas: everything in view, then back; reduced motion jumps.
        app.visual.motion = Some(Motion::Reduced);
        app.smart_zoom_at(199.0, 59.0, t0 + ms(4000));
        assert!(app.view_anim.is_none(), "no animation when motion is reduced");
        let all = app.view_state();
        app.set_view_state(initial);
        app.fit_all();
        assert!(close(app.view_state(), all), "everything in view, as Show all nodes: {all:?}");
        assert!(!close(all, initial));
        app.set_view_state(all);
        app.smart_zoom_at(199.0, 59.0, t0 + ms(5000));
        assert!(close(app.view_state(), initial));
    }

    #[test]
    fn force_click_shows_a_boxs_details() {
        let (mut app, _) = app(true);
        app.on_input(vec![InputEvent::Gesture(Gesture { kind: GestureKind::ForceClick, x: 5.0, y: 3.0, modifiers: Modifiers::NONE })]);
        assert_eq!(app.selection, ["p"]);
    }

    #[test]
    fn command_scroll_is_the_zoom_modifier_on_macos() {
        let g = Gesture { kind: GestureKind::SmartZoom, x: 0.0, y: 0.0, modifiers: Modifiers::SUPER };
        assert_eq!(crate::input::KeyPolicy::CommandApp.gesture(g).modifiers, Modifiers::CONTROL);
        assert_eq!(crate::input::KeyPolicy::CtrlApp.gesture(g).modifiers, Modifiers::SUPER);
    }

    #[test]
    fn command_plus_minus_and_zero_set_the_terminal_text_size() {
        use crate::input::{InputEvent, KeyCode, KeyEvent, KeyPolicy};
        let (mut app, sent) = split(0);
        app.focused_term = Some("t".into());
        app.focus = Focus::Terminal;
        let command = |c: char| InputEvent::Key(KeyPolicy::CommandApp.key(KeyEvent::new(KeyCode::Char(c), Modifiers::SUPER)));
        app.on_input(vec![command('='), command('='), command('-')]);
        assert_eq!((app.text_zoom_steps, app.text_zoom_reset), (1, false));
        app.on_input(vec![command('0')]);
        assert_eq!((app.text_zoom_steps, app.text_zoom_reset), (0, true), "Command+0: the default size");
        assert_eq!(app.focus, Focus::Terminal, "the terminal keeps the keyboard");
        assert!(sent.try_iter().all(|m| !matches!(m, ClientMsg::Input { .. })), "nothing reached the program");
    }
}
