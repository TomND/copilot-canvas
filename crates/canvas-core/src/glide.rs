//! Gentle momentum after panning the canvas background: released while still
//! moving, the canvas keeps going in the drag's direction and slows to a stop
//! within a fraction of a second. Speed and travel are capped (at most about
//! eight columns or four rows), so a flick never sends the view far from its
//! boxes. Only background pans glide (never box, group or area drags); any new
//! key, click or wheel stops it at once; reduced motion and effects off turn it
//! off. Time-based, so it travels the same at any frame rate.

use std::time::{Duration, Instant};

use crate::app::{App, Drag};
use crate::view::{Motion, RenderMode};

/// The pointer's recent places while panning (screen cells).
#[derive(Clone, Debug, Default)]
pub struct PanTrack {
    samples: Vec<(Instant, (f32, f32))>,
}

/// A running glide: its starting velocity (screen cells per second, x and y)
/// and how far it has gone.
#[derive(Clone, Debug)]
pub struct Glide {
    start: Instant,
    velocity: (f32, f32),
    done: (f32, f32),
    /// Cells mode: world units owed but not yet whole.
    rest: (f32, f32),
}

/// The velocity is measured over the last stretch of the drag.
const WINDOW: Duration = Duration::from_millis(80);
/// Held still this long before release: no glide.
const STALE: Duration = Duration::from_millis(60);
/// Decay time constant (seconds): total travel is velocity × TAU.
const TAU: f32 = 0.16;
/// Slower flicks do not glide (columns per second; a row counts as two columns).
const MIN_SPEED: f32 = 12.0;
/// Faster flicks glide at this speed (at most 8 columns or 4 rows of travel).
const MAX_SPEED: f32 = 50.0;
/// Below this speed the glide ends.
const STOP_SPEED: f32 = 1.5;
/// A glide never lasts longer.
const MAX_TIME: f32 = 0.7;

/// Speed with rows weighted as two columns (cells are about twice as tall as wide).
fn speed((vx, vy): (f32, f32)) -> f32 {
    vx.hypot(vy * 2.0)
}

impl PanTrack {
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn push(&mut self, at: Instant, pointer: (f32, f32)) {
        self.samples.push((at, pointer));
        if let Some(&(last, _)) = self.samples.last() {
            self.samples.retain(|(t, _)| last.duration_since(*t) <= WINDOW * 2);
        }
    }

    /// The glide's starting velocity for a release at `now`, if it should glide.
    pub fn release_velocity(&self, now: Instant) -> Option<(f32, f32)> {
        let &(last_at, last) = self.samples.last()?;
        if now.saturating_duration_since(last_at) > STALE {
            return None;
        }
        let &(first_at, first) = self.samples.iter().find(|(t, _)| last_at.duration_since(*t) <= WINDOW)?;
        // A burst of events within a few milliseconds counts as 8 ms.
        let dt = last_at.duration_since(first_at).as_secs_f32().max(0.008);
        let v = ((last.0 - first.0) / dt, (last.1 - first.1) / dt);
        let s = speed(v);
        if !s.is_finite() || s < MIN_SPEED {
            return None;
        }
        let k = (MAX_SPEED / s).min(1.0);
        Some((v.0 * k, v.1 * k))
    }
}

impl Glide {
    pub fn new(start: Instant, velocity: (f32, f32)) -> Self {
        Glide { start, velocity, done: (0.0, 0.0), rest: (0.0, 0.0) }
    }

    /// How long it lasts (until its speed drops below [`STOP_SPEED`]).
    fn duration(&self) -> f32 {
        (TAU * (speed(self.velocity) / STOP_SPEED).max(1.0).ln()).min(MAX_TIME)
    }

    /// Distance travelled `t` seconds in (exponential slow-down).
    fn travelled(&self, t: f32) -> (f32, f32) {
        let f = TAU * (1.0 - (-t / TAU).exp());
        (self.velocity.0 * f, self.velocity.1 * f)
    }

    /// The whole distance it will travel (screen cells).
    pub fn total(&self) -> (f32, f32) {
        self.travelled(self.duration())
    }
}

impl App {
    /// A background pan follows the pointer: remembers where it was.
    pub(crate) fn track_pan(&mut self) {
        if matches!(self.drag, Some(Drag::Pan { .. })) {
            let pointer = self.pointer;
            self.pan_track.push(Instant::now(), pointer);
        }
    }

    /// Mouse up on a moved background pan: starts a glide when the pointer
    /// was still moving (and motion is on).
    pub(crate) fn release_pan(&mut self, now: Instant) {
        let velocity = self.pan_track.release_velocity(now);
        self.pan_track.clear();
        if self.motion() != Motion::On {
            return;
        }
        if let Some(v) = velocity {
            self.glide = Some(Glide::new(now, v));
            self.needs_draw = true;
        }
    }

    /// Stops a glide where it is (new navigation input).
    pub fn stop_glide(&mut self) {
        if self.glide.take().is_some() {
            self.needs_draw = true;
        }
    }

    /// Moves a running glide to where it is now; ends it when it has stopped.
    pub fn step_glide(&mut self) {
        self.step_glide_at(Instant::now());
    }

    pub(crate) fn step_glide_at(&mut self, now: Instant) {
        let Some(mut glide) = self.glide.take() else { return };
        let end = glide.duration();
        let t = now.saturating_duration_since(glide.start).as_secs_f32().min(end);
        let at = glide.travelled(t);
        let (dx, dy) = (at.0 - glide.done.0, at.1 - glide.done.1);
        glide.done = at;
        match self.render_mode {
            RenderMode::Gpu => self.pan_cells(dx, dy),
            RenderMode::Cells => {
                // Whole world units; the remainder waits for the next step.
                let (n, d) = self.scale();
                let k = d as f32 / n as f32;
                glide.rest.0 += dx * k;
                glide.rest.1 += dy * k;
                let (wx, wy) = (glide.rest.0.trunc(), glide.rest.1.trunc());
                glide.rest.0 -= wx;
                glide.rest.1 -= wy;
                if wx != 0.0 || wy != 0.0 {
                    self.doc.view.x -= wx as i32;
                    self.doc.view.y -= wy as i32;
                    self.mark_dirty();
                }
            }
        }
        self.needs_draw = true;
        if t < end {
            self.glide = Some(glide);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Modifiers, MouseButton, MouseEventKind};
    use crate::undo::fixture::{app, drag, key, mouse};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_flick_glides_in_its_direction_slows_down_and_stays_close() {
        let t0 = Instant::now();
        let mut track = PanTrack::default();
        for i in 0..5 {
            track.push(t0 + ms(i * 10), (10.0 + i as f32 * 2.0, 10.0));
        }
        let v = track.release_velocity(t0 + ms(45)).expect("glides");
        assert!(v.0 > 0.0 && v.1 == 0.0, "{v:?}");
        assert!(speed(v) <= MAX_SPEED + 0.01);
        let glide = Glide::new(t0, v);
        let total = glide.total();
        assert!(total.0 > 2.0 && total.0 <= 8.0 + 0.01, "modest travel: {total:?}");
        assert!(glide.duration() <= MAX_TIME);
        // Each later stretch covers less ground than the one before.
        let steps: Vec<f32> = (1..=5).map(|i| glide.travelled(i as f32 * 0.05).0 - glide.travelled((i - 1) as f32 * 0.05).0).collect();
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
        // Held still before release, or too slow: no glide.
        assert!(track.release_velocity(t0 + ms(40 + 61)).is_none());
        let mut slow = PanTrack::default();
        slow.push(t0, (10.0, 10.0));
        slow.push(t0 + ms(80), (10.5, 10.0));
        assert!(slow.release_velocity(t0 + ms(80)).is_none());
    }

    #[test]
    fn glides_end_at_the_same_place_at_any_frame_rate() {
        for gpu in [true, false] {
            let ends: Vec<(f32, f32)> = [1u64, 16, 40, 1000]
                .into_iter()
                .map(|every| {
                    let (mut app, _) = app(gpu);
                    let t0 = Instant::now();
                    app.glide = Some(Glide::new(t0, (-40.0, 10.0)));
                    let mut t = 0;
                    while app.glide.is_some() {
                        t += every;
                        app.step_glide_at(t0 + ms(t));
                    }
                    app.origin_f()
                })
                .collect();
            for end in &ends {
                assert!((end.0 - ends[0].0).abs() <= 1.0 && (end.1 - ends[0].1).abs() <= 1.0, "{ends:?}");
            }
            // Pointer moving left: the view's origin moves right.
            assert!(ends[0].0 > 0.0 && ends[0].1 < 0.0, "{ends:?}");
        }
    }

    #[test]
    fn only_background_pans_glide_and_new_input_stops_them() {
        let none = Modifiers::NONE;
        for gpu in [true, false] {
            let (mut app, _) = app(gpu);
            app.visual.motion = Some(Motion::On);
            // Empty canvas (right of every box, below the groups).
            drag(&mut app, MouseButton::Left, (150.0, 50.0), (130.0, 45.0), none);
            assert!(app.glide.is_some(), "a background flick glides");
            let origin = app.origin_f();
            app.step_glide_at(Instant::now() + ms(50));
            assert_ne!(app.origin_f(), origin, "it keeps moving after release");
            // A key, a click and the wheel stop it at once.
            key(&mut app, crate::input::KeyCode::Esc, none);
            assert!(app.glide.is_none());
            drag(&mut app, MouseButton::Middle, (150.0, 50.0), (130.0, 45.0), none);
            assert!(app.glide.is_some(), "middle-button pans glide too");
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), (150.0, 50.0), none);
            assert!(app.glide.is_none());
            mouse(&mut app, MouseEventKind::Up(MouseButton::Left), (150.0, 50.0), none);
            drag(&mut app, MouseButton::Left, (150.0, 50.0), (130.0, 45.0), none);
            mouse(&mut app, MouseEventKind::ScrollDown, (150.0, 50.0), none);
            assert!(app.glide.is_none());
            // Boxes, group titles and area selection never glide.
            let before = app.origin_f();
            drag(&mut app, MouseButton::Left, (4.0, 3.0), (30.0, 9.0), none);
            assert!(app.glide.is_none(), "box drags do not glide");
            drag(&mut app, MouseButton::Left, (3.0, 0.0), (20.0, 4.0), none);
            assert!(app.glide.is_none(), "group drags do not glide");
            drag(&mut app, MouseButton::Right, (150.0, 50.0), (120.0, 40.0), none);
            assert!(app.glide.is_none(), "area selection does not glide");
            assert_eq!(app.origin_f(), before);
            // Reduced motion and effects off: pans stop where released.
            for motion in [Motion::Reduced, Motion::Off] {
                app.visual.motion = Some(motion);
                drag(&mut app, MouseButton::Left, (150.0, 50.0), (130.0, 45.0), none);
                assert!(app.glide.is_none(), "{motion:?}");
            }
        }
    }
}
