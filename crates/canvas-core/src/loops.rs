//! The loopback animation of looping sessions, driven by the clock rather
//! than by frames: where the lit dot is at any instant (a pure function of
//! time, so it never jumps, restarts or depends on the frame rate, panning,
//! zooming or dragging), its comet trail, the glow pulse at each lap, how
//! an arc grows in when a session starts looping and fades out when it
//! stops, and the TUI's cell stepping. The GPU client draws the dot along a
//! smooth curve at sub-pixel positions; the TUI along the arc's cells.

use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::time::Duration;

use crate::app::App;
use crate::view::{Motion, RenderMode};

/// One lap around the arc (ms).
pub const LAP_MS: u64 = 2_000;
/// An arc grows in (or fades out) over this long (ms).
pub const GROW_MS: u64 = 450;
/// Speed variation along the lap: fastest leaving the box, slowest at the
/// far bend (the speed runs from 1 − EASE to 1 + EASE).
const EASE: f32 = 0.35;
/// The glow at the arrowhead after each lap lasts this part of a lap.
const PULSE_LAP: f32 = 0.18;
/// Loops that appear this soon after the session history arrived are part
/// of the canvas as it opened: they show at once, without growing in.
const SETTLE: Duration = Duration::from_secs(2);

/// A small, stable hash of a box id (each loop's lap offset).
pub fn seed(id: &str) -> u32 {
    id.bytes().fold(2_166_136_261u32, |h, b| (h ^ b as u32).wrapping_mul(16_777_619))
}

/// The lap fraction (0..1) at `clock_ms`, offset per box so that loops
/// next to each other do not march in step.
pub fn lap(clock_ms: u64, seed: u32) -> f32 {
    let offset = (seed as u64).wrapping_mul(2_654_435_761) % LAP_MS;
    ((clock_ms.wrapping_add(offset)) % LAP_MS) as f32 / LAP_MS as f32
}

/// How far along the arc (0..1 of its length) the dot is at lap fraction
/// `t`: it leaves the box quickly, eases round the far bend and picks up
/// speed on the way back. Continuous and periodic (0 → 0, 1 → 1, the same
/// speed at both ends), so one lap joins the next without a jump.
pub fn along(t: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    (t + EASE * (2.0 * PI * t).sin() / (2.0 * PI)).clamp(0.0, 1.0)
}

/// The glow at the arrowhead as the dot comes home: 1 right after a lap
/// starts, fading to 0 over a fifth of the lap.
pub fn lap_pulse(t: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < PULSE_LAP { (1.0 - t / PULSE_LAP).powi(2) } else { 0.0 }
}

/// Ease out (cubic).
fn ease_out(p: f32) -> f32 {
    1.0 - (1.0 - p.clamp(0.0, 1.0)).powi(3)
}

/// How much of an arc shows at `clock_ms` (0..1): `changed_ms` is when the
/// loop started (it grows in) or, with `ending`, stopped (it fades out).
pub fn grow(clock_ms: u64, changed_ms: Option<u64>, ending: bool) -> f32 {
    let Some(start) = changed_ms else { return if ending { 0.0 } else { 1.0 } };
    let p = ease_out(clock_ms.saturating_sub(start) as f32 / GROW_MS as f32);
    if ending { 1.0 - p } else { p }
}

/// A grow-in or fade-out is still running at `clock_ms`.
pub fn changing(clock_ms: u64, changed_ms: Option<u64>) -> bool {
    changed_ms.is_some_and(|start| clock_ms < start + GROW_MS)
}

/// How long a cell of a TUI arc is along the way: cells are about twice as
/// tall as wide, so a vertical step counts double (one speed all round).
fn cell_len(glyph: &str) -> f32 {
    match glyph {
        "┆" => 2.0,
        "╮" | "╯" => 1.5,
        _ => 1.0,
    }
}

/// The cell of a TUI arc (`ui_canvas::loop_path`) at arc fraction `s`.
pub fn cell_at(path: &[(i32, i32, &str)], s: f32) -> usize {
    let total: f32 = path.iter().map(|(_, _, g)| cell_len(g)).sum();
    let mut left = s.clamp(0.0, 1.0) * total;
    for (i, (_, _, glyph)) in path.iter().enumerate() {
        let len = cell_len(glyph);
        if left < len {
            return i;
        }
        left -= len;
    }
    path.len().saturating_sub(1)
}

/// Loop start and stop on this client: the arcs that grow in or fade out.
#[derive(Default)]
pub struct LoopState {
    /// Boxes that looped at the last canvas model (`None` before the first).
    known: Option<HashSet<String>>,
    /// Loops that started or stopped lately: box → (clock ms, stopped).
    pub changes: HashMap<String, (u64, bool)>,
    /// The TUI's dot cells at its last frame (it redraws when one moves).
    cells: u64,
}

impl App {
    /// The clock animations are drawn at (ms) and whether it stands still
    /// (the `freeze` hook holds tick × 80 ms; tests fix `visual.clock_ms`).
    pub fn anim_clock(&self) -> (u64, bool) {
        match (self.frozen_tick, self.visual.clock_ms) {
            (Some(tick), _) => (tick as u64 * 80, true),
            (None, Some(ms)) => (ms, true),
            (None, None) => (crate::paths::now_ms(), false),
        }
    }

    /// The lap offset of a box's loop: from its id, or none while the
    /// `freeze` hook holds animations (screenshots must not depend on ids).
    pub fn loop_seed(&self, id: &str) -> u32 {
        if self.frozen_tick.is_some() { 0 } else { seed(id) }
    }

    /// Notes which boxes loop now (all of them, in view or not) and starts a
    /// grow-in or fade-out for each change (GPU client, motion on, a running
    /// clock, the canvas settled after opening; otherwise arcs switch at once).
    pub fn sync_loops(&mut self, looping: HashSet<String>, clock_ms: u64, frozen: bool) {
        let animate = self.render_mode == RenderMode::Gpu
            && self.motion() == Motion::On
            && !frozen
            && self.visual.history_since().is_some_and(|since| since >= SETTLE);
        let state = &mut self.visual.loops;
        if let Some(known) = state.known.as_ref().filter(|_| animate) {
            for id in looping.difference(known) {
                state.changes.insert(id.clone(), (clock_ms, false));
            }
            for id in known.difference(&looping) {
                state.changes.insert(id.clone(), (clock_ms, true));
            }
        }
        if !animate {
            state.changes.clear();
        }
        state.changes.retain(|_, (start, _)| changing(clock_ms, Some(*start)));
        state.known = Some(looping);
    }

    /// The loop change of a box still running at the model's clock.
    pub fn loop_change(&self, id: &str) -> Option<(u64, bool)> {
        self.visual.loops.changes.get(id).copied()
    }

    /// TUI: whether a visible loop's lit dot moved to another cell since the
    /// last frame (the TUI redraws for loops only then, and never for loops
    /// out of view).
    pub fn loop_cells_moved(&mut self) -> bool {
        if self.render_mode != RenderMode::Cells || self.motion() != Motion::On {
            return false;
        }
        let (clock, _) = self.anim_clock();
        let area = self.layout.canvas;
        let mut key = 0xcbf2_9ce4_8422_2325u64;
        for node in &self.doc.nodes {
            let (x, y, w, h) = self.node_screen(node);
            if x + w + 3 < area.x as i32 || x > area.right() as i32 || y + h < area.y as i32 || y > area.bottom() as i32 || self.node_loop(node).is_none() {
                continue;
            }
            let path = crate::ui_canvas::loop_path(x, y, w, h);
            let t = lap(clock, self.loop_seed(&node.id));
            let step = if path.len() == 1 { (t * 4.0) as usize % 2 } else { cell_at(&path, along(t)) };
            key = (key ^ (seed(&node.id) as u64) ^ ((step as u64) << 32)).wrapping_mul(0x0100_0000_01b3);
        }
        let moved = key != self.visual.loops.cells;
        self.visual.loops.cells = key;
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dot_moves_continuously_whatever_the_frame_rate() {
        let s = seed("box");
        let at = |ms: u64| along(lap(ms, s));
        // The same instant gives the same place, however many frames were drawn before.
        let (mut at30, mut at60) = (Vec::new(), Vec::new());
        for frame in 0..90u64 {
            at30.push((frame * 1000 / 30, at(1_000_000 + frame * 1000 / 30)));
        }
        for frame in 0..180u64 {
            at60.push((frame * 1000 / 60, at(1_000_000 + frame * 1000 / 60)));
        }
        for (ms, pos) in &at30 {
            if let Some((_, other)) = at60.iter().find(|(m, _)| m == ms) {
                assert_eq!(pos, other, "{ms} ms");
            }
        }
        // Between frames 16 ms apart it moves a little, never jumps (laps wrap from 1 to 0).
        let max_step = (1.0 + EASE) * 16.0 / LAP_MS as f32 + 1e-4;
        let mut previous = at(5_000);
        let mut moved = 0.0;
        for ms in (5_016..5_000 + 3 * LAP_MS).step_by(16) {
            let now = at(ms);
            let step = (now - previous).rem_euclid(1.0);
            assert!(step <= max_step, "a jump of {step} at {ms} ms");
            moved += step;
            previous = now;
        }
        assert!((moved - 3.0).abs() < 0.02, "three laps in three lap times: {moved}");
    }

    #[test]
    fn the_lap_eases_round_the_bend_and_joins_the_next() {
        assert_eq!(along(0.0), 0.0);
        assert!((along(0.5) - 0.5).abs() < 1e-6, "the far side at half time");
        assert!((along(0.999_999) - 1.0).abs() < 1e-4 && along(1.0) == 0.0);
        let speed = |t: f32| (along(t + 0.001) - along(t)) / 0.001;
        assert!(speed(0.0) > 1.3 && speed(0.5) < 0.7, "fast out, slow round the bend: {} {}", speed(0.0), speed(0.5));
        assert!((speed(0.0) - speed(0.998)).abs() < 0.02, "the same speed across the lap boundary");
        // Monotonic: the dot never runs backwards.
        assert!((0..1000).all(|i| along(i as f32 / 1000.0) <= along((i + 1) as f32 / 1000.0) || i == 999));
        assert!(lap_pulse(0.0) == 1.0 && lap_pulse(0.1) > 0.0 && lap_pulse(0.3) == 0.0);
        // Neighbours are offset, the same box always the same.
        assert_ne!(lap(0, seed("a")), lap(0, seed("b")));
        assert_eq!(lap(123, seed("a")), lap(123 + LAP_MS, seed("a")));
    }

    #[test]
    fn arcs_grow_in_and_fade_out() {
        assert_eq!(grow(10_000, None, false), 1.0);
        assert_eq!(grow(10_000, Some(10_000), false), 0.0);
        assert!(grow(10_000 + GROW_MS / 2, Some(10_000), false) > 0.5, "eased out: most of it early");
        assert_eq!(grow(10_000 + GROW_MS, Some(10_000), false), 1.0);
        assert_eq!(grow(10_000, Some(10_000), true), 1.0);
        assert_eq!(grow(10_000 + GROW_MS, Some(10_000), true), 0.0);
        assert!(changing(10_100, Some(10_000)) && !changing(10_000 + GROW_MS, Some(10_000)));
    }

    #[test]
    fn tui_cells_keep_one_speed() {
        let path = crate::ui_canvas::loop_path(10, 5, 30, 4);
        // ┄ ┄ ╮ ┆ ┆ ╯ ┄ ◂: 1 1 1.5 2 2 1.5 1 1 = 11 units.
        assert_eq!(cell_at(&path, 0.0), 0);
        assert_eq!(cell_at(&path, 1.5 / 11.0), 1);
        assert_eq!(cell_at(&path, 4.0 / 11.0), 3, "a vertical cell holds the dot twice as long");
        assert_eq!(cell_at(&path, 5.0 / 11.0), 3);
        assert_eq!(cell_at(&path, 1.0), path.len() - 1);
    }

    fn looping_app(motion: Motion) -> App {
        let node: crate::doc::Node = serde_json::from_value(serde_json::json!({ "id": "s", "kind": "session", "name": "Loops", "x": 2, "y": 2 })).unwrap();
        let doc = crate::doc::Doc { nodes: vec![node], ..Default::default() };
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.layout.canvas = ratatui::layout::Rect::new(0, 1, 120, 40);
        app.render_mode = RenderMode::Gpu;
        app.view_scale = 1.0;
        app.visual.motion = Some(motion);
        app.visual.history_at = Some(std::time::Instant::now() - Duration::from_secs(10));
        app.pan_quiet_until = None;
        app.set_loop("s", 900_000);
        app
    }

    impl App {
        fn set_loop(&mut self, id: &str, every: u64) {
            let summary = crate::protocol::SessionSummary { id: id.into(), live: true, tracked: true, loop_ms: every, ..Default::default() };
            self.sessions.insert(id.into(), summary);
        }

        fn loop_models(&mut self) -> Vec<crate::view::LoopModel> {
            self.model().loops
        }

        fn model(&mut self) -> crate::view::CanvasModel {
            let area = self.layout.canvas;
            crate::ui_canvas::canvas_model(self, area)
        }
    }

    #[test]
    fn starting_and_stopping_a_loop_grows_its_arc_in_and_fades_it_out() {
        let mut app = looping_app(Motion::On);
        let steady = app.loop_models();
        assert_eq!(steady.len(), 1);
        assert_eq!((steady[0].changed_ms, steady[0].ending), (None, false), "a loop there when the canvas opened just shows");
        // It stops: the box loses its "↻ every", the arc fades out.
        app.set_loop("s", 0);
        let model = app.model();
        assert!(model.boxes[0].every.is_none());
        let fading = &model.loops[0];
        let at = fading.changed_ms.expect("a change");
        assert!(fading.ending && fading.grow_at(at) == 1.0 && fading.grow_at(at + GROW_MS) == 0.0);
        assert!(model.loops_busy(at + 100), "frames while it fades");
        // It loops again: the arc grows in from nothing.
        app.set_loop("s", 60_000);
        let growing = app.loop_models();
        let at = growing[0].changed_ms.expect("a change");
        assert!(!growing[0].ending && growing[0].grow_at(at) == 0.0 && growing[0].grow_at(at + GROW_MS) == 1.0);
        // With reduced motion loops switch at once.
        let mut app = looping_app(Motion::Reduced);
        app.loop_models();
        app.set_loop("s", 0);
        assert!(app.loop_models().is_empty(), "no fading arc");
        app.set_loop("s", 60_000);
        assert_eq!(app.loop_models()[0].changed_ms, None, "no growing arc");
        // Loops arriving while the canvas opens (the history just came in) show at once.
        let mut app = looping_app(Motion::On);
        app.visual.history_at = Some(std::time::Instant::now());
        app.loop_models();
        app.set_loop("s", 0);
        assert!(app.loop_models().is_empty());
    }

    #[test]
    fn the_gpu_client_animates_loops_itself_and_the_tui_redraws_only_for_visible_dots() {
        let mut app = looping_app(Motion::On);
        let model = app.model();
        assert!(model.loops_move() && !model.frozen, "the GPU client moves the dots on its own clock");
        assert!(!app.loop_cells_moved(), "no core redraws for loops in the GPU client");
        // Frozen (golden screenshots): drawn still.
        app.frozen_tick = Some(0);
        let model = app.model();
        assert!(model.frozen && !model.loops_move() && model.anim_clock() == 0);
        app.frozen_tick = None;
        // The TUI: a redraw when a visible dot changes cell, none while it stays.
        app.render_mode = RenderMode::Cells;
        app.visual.clock_ms = Some(0);
        assert!(app.loop_cells_moved(), "first frame");
        assert!(!app.loop_cells_moved(), "same cell");
        let path = crate::ui_canvas::loop_path(0, 0, 30, 4);
        let next = (1..LAP_MS).find(|ms| cell_at(&path, along(lap(*ms, seed("s")))) != cell_at(&path, along(lap(0, seed("s"))))).unwrap();
        app.visual.clock_ms = Some(next);
        assert!(app.loop_cells_moved(), "the dot moved on");
        // Out of view: no redraws for it.
        app.doc.nodes[0].x = 5_000;
        app.loop_cells_moved();
        for ms in (0..LAP_MS).step_by(40) {
            app.visual.clock_ms = Some(ms);
            assert!(!app.loop_cells_moved(), "{ms} ms");
        }
        // Reduced motion: still.
        app.doc.nodes[0].x = 2;
        app.visual.motion = Some(Motion::Reduced);
        assert!(!app.loop_cells_moved());
    }
}
