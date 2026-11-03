//! The working glow: every session box at work releases energy into the
//! canvas's dot grid around it. Rings (wave fronts) of brighter dots leave the
//! box's outline, a new one every [`PERIOD_MS`], travel outward at [`SPEED`]
//! and fade out as they go (about [`TRAVEL`] world units: cells at 100 %); the
//! dots closest to the box keep a soft steady glow. Distances are measured
//! from the box's rectangle, not its centre (a row counts as [`ROW`] cells, so
//! the rings are round on screen), in world units, so the glow looks the same
//! at every zoom. Several working boxes combine by maximum. Every level is a
//! pure function of the animation clock: it never depends on the frame rate
//! and never restarts while you pan or zoom. Each box fades its glow in when it
//! starts working and out when it stops ([`Fade`]); its last rings travel on
//! and fade as they go.
//!
//! The GPU client draws the lit dots on a layer of their own over the grid,
//! computing only the dots within [`REACH`] of a working box, and repaints that
//! layer alone while rings move; the TUI steps the dots near working boxes
//! through a few colours on the frames its working spinners draw anyway, and
//! switches without fades. Reduced motion: a static soft glow, no rings, no
//! fades. Effects off, *Settings › Working glow* off and the `freeze` hook:
//! no glow.

use std::collections::HashMap;
use std::f32::consts::PI;

use ratatui::style::Color;

use crate::app::App;
use crate::doc::{Node, NodeKind};
use crate::paint::{FAINT, GLOW_LIT};
use crate::status::Status;
use crate::view::{Motion, RenderMode, WRect};
use crate::visual::VisualFrame;

/// A new ring leaves a working box this often (ms).
pub const PERIOD_MS: u64 = 1_400;
/// How fast a ring travels: world units per ms (10 cells a second at 100 %).
pub const SPEED: f32 = 0.01;
/// A ring's life (ms): it has faded out by then.
pub const LIFE_MS: u64 = 1_600;
/// How far a ring's outer edge travels from the outline in its life (world
/// units); its front, the brightest part, gets [`BAND`] less far.
pub const TRAVEL: f32 = SPEED * LIFE_MS as f32;
/// Half the width of a ring's band of lit dots (world units). A ring is born
/// just inside the box (its front at `-BAND`), so it grows out of the outline.
pub const BAND: f32 = 3.5;
/// The steady glow's reach (world units) and its level at the outline.
pub const NEAR: f32 = 8.0;
pub const NEAR_LEVEL: f32 = 0.3;
/// Reduced motion's static glow: its reach and its level at the outline.
pub const STILL: f32 = 14.0;
pub const STILL_LEVEL: f32 = 0.6;
/// Nothing of a box's glow shows farther than this from its outline.
pub const REACH: f32 = TRAVEL;
/// A row is about this many cells tall on screen.
pub const ROW: f32 = 2.0;
/// A box's whole fade in or out (ms).
pub const FADE_MS: u64 = 700;
/// A fully lit dot's radius in the GPU client: 1 + `GROW` times the normal.
pub const GROW: f32 = 0.5;
/// The TUI's lit colours besides the normal one.
pub const TUI_STEPS: u8 = 3;

/// Ease in and out (smoothstep).
fn ease(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

/// 1 at the outline, down to 0 at `reach` (quadratic).
fn falloff(d: f32, reach: f32) -> f32 {
    let p = (1.0 - d / reach).max(0.0);
    p * p
}

/// How far world point (`x`, `y`) is from `rect`'s outline (world units, a
/// row counting [`ROW`]; 0 inside).
pub fn distance(rect: WRect, x: f32, y: f32) -> f32 {
    let dx = (rect.x - x).max(x - rect.right()).max(0.0);
    let dy = (rect.y - y).max(y - rect.bottom()).max(0.0) * ROW;
    (dx * dx + dy * dy).sqrt()
}

/// How far out a ring's front is at `age_ms` (world units from the outline).
pub fn ring_front(age_ms: u64) -> f32 {
    SPEED * age_ms as f32 - BAND
}

/// A ring's strength where its front is (0..1): full until it leaves the
/// box, fading to nothing by the end of its life.
pub fn ring_strength(front: f32) -> f32 {
    let p = (front / (TRAVEL - BAND)).clamp(0.0, 1.0);
    1.0 - p * p
}

/// A box's glow fading in while it works and out once it stopped. A change
/// mid-way turns it around where it is, so nothing jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fade {
    /// It works: fading in (or fully in); else fading out (or fully out).
    pub on: bool,
    /// When it last turned (animation clock, ms) and its position then;
    /// `None`: settled.
    pub since: Option<(u64, f32)>,
}

impl Fade {
    /// Fully in, no fade (the TUI, reduced motion).
    pub const SETTLED: Fade = Fade { on: true, since: None };

    /// Its position at `clock_ms` (0..1, linear in time).
    pub fn position(&self, clock_ms: u64) -> f32 {
        let Some((start, from)) = self.since else { return if self.on { 1.0 } else { 0.0 } };
        let step = clock_ms.saturating_sub(start) as f32 / FADE_MS as f32;
        if self.on { (from + step).min(1.0) } else { (from - step).max(0.0) }
    }

    /// How much of the glow shows at `clock_ms` (0..1, eased in and out).
    pub fn envelope(&self, clock_ms: u64) -> f32 {
        ease(self.position(clock_ms))
    }

    /// It works (`on`) or not from `clock_ms` on.
    pub fn turn(&mut self, on: bool, clock_ms: u64) {
        if on != self.on {
            *self = Fade { on, since: Some((clock_ms, self.position(clock_ms))) };
        }
    }

    /// When nothing of it shows any more (ms; `None` while it works): the
    /// end of the fade-out plus a ring's life, as its last rings travel on.
    pub fn over_at(&self) -> Option<u64> {
        match (self.on, self.since) {
            (true, _) => None,
            (false, None) => Some(0),
            (false, Some((start, from))) => Some(start + (from * FADE_MS as f32).ceil() as u64 + LIFE_MS),
        }
    }

    /// Something of it shows at `clock_ms`.
    pub fn alive(&self, clock_ms: u64) -> bool {
        self.over_at().is_none_or(|end| clock_ms < end)
    }
}

/// One working box (or one whose glow fades out).
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    /// Where the box is drawn (world units, with the drag offset).
    pub rect: WRect,
    pub fade: Fade,
    /// Offsets its rings from its neighbours' (ms into a period, from its id).
    pub offset: u64,
}

impl Source {
    /// How far world point (`x`, `y`) is from its outline.
    pub fn distance(&self, x: f32, y: f32) -> f32 {
        distance(self.rect, x, y)
    }

    /// Its rings at `clock_ms`, youngest first: (how far out the front is,
    /// world units; its strength 0..1, faded as it travels and by the box's
    /// fade: now while it works, else when the ring left the box, so its last
    /// rings travel on as they fade).
    pub fn rings(&self, clock_ms: u64) -> impl Iterator<Item = (f32, f32)> + '_ {
        let first = (clock_ms + self.offset) % PERIOD_MS;
        (0u64..).map(move |k| first + k * PERIOD_MS).take_while(|age| *age < LIFE_MS).filter(move |age| *age <= clock_ms).map(move |age| {
            let fade = if self.fade.on { self.fade.envelope(clock_ms) } else { self.fade.envelope(clock_ms - age) };
            let front = ring_front(age);
            (front, fade * ring_strength(front))
        })
    }

    /// Its glow at distance `d` from its outline at `clock_ms` (0..1): the
    /// rings and the steady glow near it, or (`moving` false) the static glow.
    pub fn level_at(&self, d: f32, clock_ms: u64, moving: bool) -> f32 {
        if d >= REACH {
            return 0.0;
        }
        let envelope = self.fade.envelope(clock_ms);
        if !moving {
            return STILL_LEVEL * falloff(d, STILL) * envelope;
        }
        let mut level = NEAR_LEVEL * falloff(d, NEAR) * envelope;
        for (front, strength) in self.rings(clock_ms) {
            let x = (d - front) / BAND;
            if x.abs() < 1.0 {
                level = level.max(strength * (0.5 + 0.5 * (PI * x).cos()));
            }
        }
        level
    }
}

/// The working glow of a frame: its sources within reach of the view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Glow {
    /// By id.
    pub sources: Vec<Source>,
    /// Rings move (motion on); else reduced motion's static glow.
    pub moving: bool,
}

impl Glow {
    /// The glow on the dot at world point (`x`, `y`) at `clock_ms`: 0 (its
    /// normal colour) … 1 ([`GLOW_LIT`], the GPU client's largest dot).
    pub fn level(&self, x: f32, y: f32, clock_ms: u64) -> f32 {
        self.sources.iter().map(|s| s.level_at(s.distance(x, y), clock_ms, self.moving)).fold(0.0, f32::max)
    }

    /// Its dots change at `clock_ms`: rings move, a box fades in or out.
    pub fn busy(&self, clock_ms: u64) -> bool {
        self.moving && self.sources.iter().any(|s| s.fade.alive(clock_ms))
    }

    /// Where a box's glow can show (world units).
    pub fn reach(rect: WRect) -> WRect {
        WRect::new(rect.x - REACH, rect.y - REACH / ROW, rect.w + 2.0 * REACH, rect.h + 2.0 * REACH / ROW)
    }
}

fn overlaps(a: WRect, b: WRect) -> bool {
    a.x <= b.right() && b.x <= a.right() && a.y <= b.bottom() && b.y <= a.bottom()
}

/// A box's ring offset (ms into a period) from its id.
pub fn offset(id: &str) -> u64 {
    (crate::loops::seed(id) as u64).wrapping_mul(2_654_435_761) % PERIOD_MS
}

fn rgb(color: Color) -> [f32; 3] {
    match color {
        Color::Rgb(r, g, b) => [r as f32, g as f32, b as f32],
        _ => [0.0; 3],
    }
}

/// A dot's colour at `level` (0: [`FAINT`], its normal colour; 1:
/// [`GLOW_LIT`]), unrounded.
pub fn dot_rgb(level: f32) -> [f32; 3] {
    let (a, b) = (rgb(FAINT), rgb(GLOW_LIT));
    let t = level.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// A dot's colour at `level`.
pub fn dot_color(level: f32) -> Color {
    let [r, g, b] = dot_rgb(level).map(|c| c.round() as u8);
    Color::Rgb(r, g, b)
}

/// The TUI's step for `level` (0: the normal colour … [`TUI_STEPS`]).
pub fn step(level: f32) -> u8 {
    (level.clamp(0.0, 1.0) * TUI_STEPS as f32).round() as u8
}

/// The TUI's dot colour at `level`: the nearest of its few steps.
pub fn tui_color(level: f32) -> Color {
    dot_color(step(level) as f32 / TUI_STEPS as f32)
}

impl App {
    /// *Settings › Working glow* (this machine; on unless switched off).
    pub fn working_glow_setting(&self) -> bool {
        self.visual.grid_pulse.unwrap_or(true)
    }

    /// The working glow at `clock_ms`, its fades brought up to date (every
    /// box, in view or not): a source for each working session box (a session
    /// whose subagents run is working; boxes in a deck or hidden have none, a
    /// deck's session covers it) and each box whose glow still fades out,
    /// drawn at `rect`, within reach of `visible`. `None` when nothing glows
    /// there: nothing works, effects off, the setting off or the `freeze`
    /// hook. The GPU client with motion on fades; the TUI and reduced motion
    /// switch at once.
    pub fn working_glow(&mut self, clock_ms: u64, frame: &VisualFrame, rect: &dyn Fn(&Node) -> WRect, visible: WRect) -> Option<Glow> {
        let motion = self.motion();
        if self.frozen_tick.is_some() || motion == Motion::Off || !self.working_glow_setting() {
            self.visual.glow.clear();
            return None;
        }
        let working: Vec<&Node> = self.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && !frame.skip(&n.id) && self.node_status(n) == Status::Working).collect();
        let fading = motion == Motion::On && self.render_mode == RenderMode::Gpu;
        let mut sources: Vec<Source> = if fading {
            let ids: Vec<String> = working.iter().map(|n| n.id.clone()).collect();
            let fades = &mut self.visual.glow;
            for id in &ids {
                fades.entry(id.clone()).or_default().turn(true, clock_ms);
            }
            for (id, fade) in fades.iter_mut() {
                if !ids.contains(id) {
                    fade.turn(false, clock_ms);
                }
            }
            fades.retain(|_, fade| fade.alive(clock_ms));
            let doc = &self.doc;
            fades
                .iter()
                .filter_map(|(id, fade)| {
                    let node = doc.node(id).filter(|n| !frame.skip(&n.id))?;
                    Some(Source { id: id.clone(), rect: rect(node), fade: *fade, offset: offset(id) })
                })
                .collect()
        } else {
            // Reduced motion forgets the fades (back on, they start afresh).
            if motion != Motion::On {
                self.visual.glow.clear();
            }
            working.iter().map(|n| Source { id: n.id.clone(), rect: rect(n), fade: Fade::SETTLED, offset: offset(&n.id) }).collect()
        };
        sources.retain(|s| overlaps(Glow::reach(s.rect), visible));
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        (!sources.is_empty()).then_some(Glow { sources, moving: motion == Motion::On })
    }
}

/// The fades of the boxes that glow, by id (`VisualState::glow`).
pub type Fades = HashMap<String, Fade>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A 30 × 4 box at the origin; rings born at every multiple of the period.
    fn source(fade: Fade) -> Source {
        Source { id: "s".into(), rect: WRect::new(0.0, 0.0, 30.0, 4.0), fade, offset: 0 }
    }

    fn glow(sources: Vec<Source>) -> Glow {
        Glow { sources, moving: true }
    }

    /// A clock far from zero at the birth of a ring (offset 0).
    const T0: u64 = 1_000 * PERIOD_MS;

    /// Where the glow peaks (distance from the outline, 0.1 steps) and how bright.
    fn peak(s: &Source, clock_ms: u64) -> (f32, f32) {
        (0..=200).map(|i| i as f32 * 0.1).map(|d| (d, s.level_at(d, clock_ms, true))).fold((0.0, 0.0), |best, p| if p.1 > best.1 { p } else { best })
    }

    #[test]
    fn a_ring_leaves_the_outline_and_travels_outward_fading_as_it_goes() {
        let s = source(Fade::SETTLED);
        // Each ring's front moves out with the clock, at SPEED, from just inside the box.
        let fronts: Vec<f32> = (0..14).map(|i| s.rings(T0 + i * 100).next().unwrap().0).collect();
        assert!((fronts[0] + BAND).abs() < 1e-4 && fronts.windows(2).all(|w| (w[1] - w[0] - 1.0).abs() < 1e-3), "{fronts:?}");
        // The brightest dots are farther from the box the older the ring, and dimmer.
        let peaks: Vec<(f32, f32)> = [450, 650, 850, 1050, 1250].iter().map(|age| peak(&s, T0 + age)).collect();
        for w in peaks.windows(2) {
            assert!(w[1].0 > w[0].0 + 1.5, "outward: {peaks:?}");
            assert!(w[1].1 < w[0].1, "fading as it goes: {peaks:?}");
        }
        // About 10 to 16 cells: still lit 10 cells out, gone beyond the reach.
        assert!(peaks[3].0 >= 7.0 && peaks[3].1 > 0.2 && (10.0..=16.0).contains(&REACH), "{peaks:?}");
        // Near the box a passing ring is clearly brighter than the steady glow.
        assert!(peaks[0].1 > 0.85 && peaks[0].1 > 3.0 * s.level_at(peaks[0].0, T0 + PERIOD_MS / 2 + 600, true), "{peaks:?}");
        // A new ring every period: the pattern repeats.
        for d in [1.0, 3.0, 6.5, 11.0] {
            for t in [0, 300, 999] {
                assert!((s.level_at(d, T0 + t, true) - s.level_at(d, T0 + t + 7 * PERIOD_MS, true)).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn it_is_bounded_zero_far_away_and_brighter_near_the_box_than_far() {
        let s = source(Fade::SETTLED);
        let g = glow(vec![s.clone()]);
        let mut sum = [0.0f32; 4];
        for t in (0..PERIOD_MS).step_by(10) {
            for (i, d) in [3.0, 8.0, 13.0, REACH].into_iter().enumerate() {
                let level = s.level_at(d, T0 + t, true);
                assert!((0.0..=1.0).contains(&level), "bounded: {level}");
                sum[i] += level;
            }
            // Far away, in any direction: nothing.
            for (x, y) in [(-REACH - 0.1, 2.0), (30.0 + REACH + 1.0, 2.0), (15.0, -REACH / ROW - 0.1), (15.0, 4.0 + REACH / ROW + 0.5), (100.0, 100.0)] {
                assert_eq!(g.level(x, y, T0 + t), 0.0, "({x}, {y})");
            }
        }
        assert!(sum[0] > 2.0 * sum[1] && sum[1] > sum[2] && sum[2] > 0.0 && sum[3] == 0.0, "brighter near the box: {sum:?}");
        // Measured from the outline, not the centre: the same 3 cells out on every side
        // (a row counts as two cells), whatever the box's size.
        for t in [T0 + 200, T0 + 600] {
            let at = |x: f32, y: f32| g.level(x, y, t);
            let right = at(33.0, 2.0);
            assert!(right > 0.0 && (at(-3.0, 2.0) - right).abs() < 1e-6 && (at(15.0, -1.5) - right).abs() < 1e-6 && (at(2.0, 5.5) - right).abs() < 1e-6);
        }
        // Nothing works: no sources, nothing anywhere.
        assert_eq!(glow(Vec::new()).level(31.0, 2.0, T0), 0.0);
        assert!(!glow(Vec::new()).busy(T0));
        // The colours: the normal dot at 0, the light working blue at the top, 2.5 to 3 times as bright.
        let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        assert_eq!(dot_color(0.0), FAINT);
        assert_eq!(dot_color(1.0), GLOW_LIT);
        let gain = luma(dot_rgb(1.0)) / luma(dot_rgb(0.0));
        assert!((2.5..=3.0).contains(&gain), "{gain}");
        assert_eq!(dot_color(7.0), GLOW_LIT, "never beyond the top");
    }

    #[test]
    fn several_boxes_combine_by_the_brightest() {
        let a = source(Fade::SETTLED);
        let b = Source { id: "b".into(), rect: WRect::new(40.0, 0.0, 30.0, 4.0), fade: Fade::SETTLED, offset: 500 };
        let both = glow(vec![a.clone(), b.clone()]);
        for t in (0..2 * PERIOD_MS).step_by(37) {
            for x in [-5.0, 31.0, 35.0, 38.0, 39.5, 72.0] {
                let (la, lb) = (glow(vec![a.clone()]).level(x, 2.0, T0 + t), glow(vec![b.clone()]).level(x, 2.0, T0 + t));
                let level = both.level(x, 2.0, T0 + t);
                assert_eq!(level, la.max(lb), "max, never a sum");
                assert!(level <= 1.0);
            }
        }
    }

    /// Levels at `points` over time while a box works from `start` to `stop`,
    /// with frames every `frame` ms (the fade turns at the frame that sees it).
    fn run(frame: u64, start: u64, stop: u64, until: u64) -> Vec<(u64, Vec<f32>)> {
        let mut fade = Fade::default();
        let mut out = Vec::new();
        let mut t = 0;
        while t <= until {
            let working = t >= start && t < stop;
            let at = if working && !fade.on { start } else if !working && fade.on { stop } else { t };
            fade.turn(working, T0 + at);
            let s = Source { fade, ..source(fade) };
            out.push((t, [0.5, 2.0, 4.0, 7.5, 12.0].iter().map(|d| s.level_at(*d, T0 + t, true)).collect()));
            t += frame;
        }
        out
    }

    #[test]
    fn the_same_at_any_frame_rate_and_continuous() {
        let (a, b, c) = (run(33, 1_000, 6_100, 9_000), run(16, 1_000, 6_100, 9_000), run(40, 1_000, 6_100, 9_000));
        let mut compared = 0;
        for (t, levels) in &b {
            for other in [&a, &c] {
                if let Some((_, l)) = other.iter().find(|(u, _)| u == t) {
                    compared += 1;
                    for (x, y) in levels.iter().zip(l) {
                        assert!((x - y).abs() < 1e-6, "{t} ms: {levels:?} vs {l:?}");
                    }
                }
            }
        }
        assert!(compared > 100);
        // No jumps: at most a ring's slope plus a fade's per 16 ms frame.
        let max = (0.5 * PI / BAND * SPEED + 1.6 / FADE_MS as f32) * 16.0;
        for pair in b.windows(2) {
            for (x, y) in pair[0].1.iter().zip(&pair[1].1) {
                assert!((x - y).abs() <= max, "a jump at {} ms: {x} -> {y}", pair[1].0);
            }
        }
        assert!(b.iter().any(|(_, l)| l[2] > 0.8), "it glows");
    }

    #[test]
    fn it_fades_in_when_a_box_starts_and_its_last_rings_travel_on_when_it_stops() {
        let start = T0 + 300;
        let mut fade = Fade::default();
        assert!(!fade.alive(start) && source(fade).level_at(2.0, start, true) == 0.0, "nothing before it works");
        fade.turn(true, start);
        let at = |fade: Fade, d: f32, t: u64| source(fade).level_at(d, t, true);
        assert_eq!(at(fade, 2.0, start), 0.0, "from nothing");
        assert!(at(fade, 2.0, start + FADE_MS / 4) < 0.3, "eased in");
        assert_eq!(fade.envelope(start + FADE_MS), 1.0);
        assert!(fade.alive(start + 100 * PERIOD_MS) && fade.over_at().is_none());
        // It stops as a ring passes 4 cells out: that ring travels on and fades, no new one comes.
        let stop = T0 + 10 * PERIOD_MS + 750;
        fade.turn(false, stop);
        let s = source(fade);
        assert!(s.level_at(4.0, stop, true) > 0.6, "no snap");
        let (near, far) = (peak(&s, stop + 300), peak(&s, stop + 600));
        assert!(far.0 > near.0 && far.1 > 0.1, "the last ring keeps going: {near:?} {far:?}");
        let ring_age = |front: f32| ((front + BAND) / SPEED).round() as u64;
        for t in (stop + FADE_MS..stop + FADE_MS + LIFE_MS).step_by(50) {
            assert!(s.rings(t).all(|(front, strength)| ring_age(front) + 1 >= t - stop - FADE_MS || strength == 0.0), "no new ring after the fade-out");
        }
        let end = fade.over_at().unwrap();
        assert_eq!(end, stop + FADE_MS + LIFE_MS);
        assert!(fade.alive(end - 1) && !fade.alive(end));
        assert!((0..=20).all(|i| source(fade).level_at(i as f32, end + i * 37, true) == 0.0), "then nothing");
        // Turned around mid-way: from where it was.
        let mut fade = Fade::default();
        fade.turn(true, 0);
        let half = fade.envelope(FADE_MS / 2);
        fade.turn(false, FADE_MS / 2);
        assert_eq!(fade.envelope(FADE_MS / 2), half);
        // Working on is no change.
        let mut on = Fade::SETTLED;
        on.turn(true, 5);
        assert_eq!(on, Fade::SETTLED);
    }

    #[test]
    fn reduced_motion_is_a_static_glow_near_the_box() {
        let s = source(Fade::SETTLED);
        let g = Glow { sources: vec![s.clone()], moving: false };
        let levels: Vec<f32> = [1.0, 4.0, 8.0, 12.0, STILL, REACH].iter().map(|d| s.level_at(*d, T0, false)).collect();
        assert!(levels.windows(2).all(|w| w[0] >= w[1]) && levels[0] > 0.4 && levels[1] > 0.25 && levels[4] == 0.0, "{levels:?}");
        for t in (0..3 * PERIOD_MS).step_by(53) {
            assert_eq!(g.level(31.0, 2.0, T0 + t), g.level(31.0, 2.0, T0), "never moves");
        }
        assert!(!g.busy(T0), "no frames for it");
        assert!(glow(vec![s]).busy(T0), "rings move");
    }

    #[test]
    fn the_tui_steps_through_a_few_colours() {
        assert_eq!((step(0.0), step(0.1), step(0.5), step(1.0), step(3.0)), (0, 0, 2, TUI_STEPS, TUI_STEPS));
        assert_eq!(tui_color(0.0), FAINT);
        assert_eq!(tui_color(1.0), GLOW_LIT);
        let colours: std::collections::HashSet<String> = (0..=100).map(|i| format!("{:?}", tui_color(i as f32 / 100.0))).collect();
        assert_eq!(colours.len(), TUI_STEPS as usize + 1);
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::protocol::{Activity, AgentStatus, AgentSummary, SessionSummary};

    /// A canvas with "s" (30 x 4 at 10, 10) and "t" far to the right (at 200, 10), 120 x 40 cells at 100 %.
    fn app(render_mode: RenderMode) -> App {
        let node = |id: &str, x: i32| -> crate::doc::Node { serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": 10 })).unwrap() };
        let doc = crate::doc::Doc { nodes: vec![node("s", 10), node("t", 200)], ..Default::default() };
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.layout.canvas = ratatui::layout::Rect::new(0, 1, 120, 40);
        app.render_mode = render_mode;
        app.view_scale = 1.0;
        app.visual.motion = Some(Motion::On);
        app.pan_quiet_until = None;
        app
    }

    fn work(app: &mut App, id: &str, activity: Activity) {
        app.sessions.insert(id.into(), SessionSummary { id: id.into(), live: true, tracked: true, activity, ..Default::default() });
    }

    fn model(app: &mut App, clock_ms: u64) -> crate::view::CanvasModel {
        app.visual.clock_ms = Some(clock_ms);
        let area = app.layout.canvas;
        crate::ui_canvas::canvas_model(app, area)
    }

    const T0: u64 = 500 * PERIOD_MS;

    #[test]
    fn the_model_carries_a_source_for_each_working_box_in_reach() {
        let mut app = app(RenderMode::Gpu);
        assert!(model(&mut app, T0).glow.is_none(), "nothing works");
        work(&mut app, "s", Activity::Working);
        work(&mut app, "t", Activity::Idle);
        let m = model(&mut app, T0);
        let glow = m.glow.as_ref().expect("a glow");
        assert_eq!(glow.sources.len(), 1);
        assert!(glow.moving && glow.sources[0].id == "s" && glow.sources[0].rect == WRect::new(10.0, 10.0, 30.0, 4.0));
        assert!(m.glow_busy(T0) && m.glow_level(42.0, 12.0, T0) == 0.0, "it fades in from nothing");
        let later = T0 + 3 * PERIOD_MS;
        let m = model(&mut app, later);
        let near = (0..PERIOD_MS).step_by(20).map(|t| m.glow_level(43.0, 12.0, later + t)).fold(0.0, f32::max);
        assert!(near > 0.8 && m.glow_level(100.0, 30.0, later) == 0.0, "near {near}");
        // A box off screen but within reach lights the visible dots near it; farther away, none.
        app.doc.view.x = 45;
        let m = model(&mut app, later);
        assert!(m.glow.as_ref().is_some_and(|g| g.sources.len() == 1), "the box is 5 cells left of the view");
        app.doc.view.x = 70;
        assert!(model(&mut app, later).glow.is_none(), "out of reach of the view");
        app.doc.view.x = 0;
        // Needs you and failed have their own styling: no glow; a session whose subagent runs glows.
        let mut waiting = SessionSummary { id: "s".into(), live: true, tracked: true, activity: Activity::Working, pending: 1, ..Default::default() };
        app.sessions.insert("s".into(), waiting.clone());
        let m = model(&mut app, later + 10);
        assert!(m.glow.as_ref().is_some_and(|g| !g.sources[0].fade.on), "needs you: it fades out");
        waiting.pending = 0;
        waiting.activity = Activity::Idle;
        waiting.agents.push(AgentSummary { key: "a".into(), name: "Explorer".into(), description: String::new(), status: AgentStatus::Working, finished_ms: None });
        app.sessions.insert("s".into(), waiting);
        let m = model(&mut app, later + 20);
        assert!(m.glow.as_ref().is_some_and(|g| g.sources[0].fade.on), "a subagent runs");
        // Two working boxes in view: two sources.
        work(&mut app, "s", Activity::Working);
        app.doc.nodes[1].x = 60;
        work(&mut app, "t", Activity::Working);
        assert_eq!(model(&mut app, later + 40).glow.map(|g| g.sources.len()), Some(2));
    }

    #[test]
    fn a_box_that_stops_fades_out_then_nothing_and_no_frames() {
        let mut app = app(RenderMode::Gpu);
        work(&mut app, "s", Activity::Working);
        model(&mut app, T0);
        let peak = T0 + 2 * PERIOD_MS;
        assert!(model(&mut app, peak).glow_busy(peak));
        work(&mut app, "s", Activity::Idle);
        let stop = peak + 40;
        let m = model(&mut app, stop);
        let glow = m.glow.as_ref().expect("fading out");
        assert!(!glow.sources[0].fade.on && m.glow_busy(stop) && m.glow_busy(stop + FADE_MS + LIFE_MS - 1));
        assert!(!m.glow_busy(stop + FADE_MS + LIFE_MS), "then no frames, even before the next model");
        assert!(model(&mut app, stop + FADE_MS + LIFE_MS).glow.is_none() && app.visual.glow.is_empty(), "forgotten");
        assert!(!model(&mut app, stop + 10 * LIFE_MS).dump().contains("glow"));
    }

    #[test]
    fn reduced_motion_effects_off_the_setting_and_the_freeze_hook() {
        type Setup = Box<dyn Fn(&mut App)>;
        let cases: [(&str, Setup, bool); 4] = [
            ("reduced motion", Box::new(|app: &mut App| app.visual.motion = Some(Motion::Reduced)), true),
            ("effects off", Box::new(|app: &mut App| app.visual.motion = Some(Motion::Off)), false),
            ("the setting off", Box::new(|app: &mut App| app.visual.grid_pulse = Some(false)), false),
            ("the freeze hook", Box::new(|app: &mut App| app.frozen_tick = Some(3)), false),
        ];
        for (what, setup, still) in cases {
            let mut app = app(RenderMode::Gpu);
            work(&mut app, "s", Activity::Working);
            model(&mut app, T0);
            assert!(model(&mut app, T0 + PERIOD_MS).glow.is_some(), "{what}: glowing before");
            setup(&mut app);
            let m = model(&mut app, T0 + 2 * PERIOD_MS);
            if still {
                let glow = m.glow.as_ref().unwrap_or_else(|| panic!("{what}: a static glow"));
                assert!(!glow.moving && !m.glow_busy(m.clock_ms) && glow.sources[0].fade == Fade::SETTLED, "{what}: at once, no fade, no frames");
                let near = m.glow_level(42.0, 12.0, m.clock_ms);
                assert!(near > 0.3 && m.glow_level(70.0, 12.0, m.clock_ms) == 0.0 && near == m.glow_level(42.0, 12.0, m.clock_ms + 777), "{what}: {near}");
            } else {
                assert!(m.glow.is_none() && app.visual.glow.is_empty(), "{what}: none, at once");
            }
        }
    }

    #[test]
    fn the_tui_lights_the_dots_near_a_working_box_in_steps_and_switches_at_once() {
        let mut app = app(RenderMode::Cells);
        app.layout.canvas = ratatui::layout::Rect::new(0, 0, 120, 40);
        let draw = |app: &mut App, clock: u64| {
            app.visual.clock_ms = Some(clock);
            let area = ratatui::layout::Rect::new(0, 0, 122, 42);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            let canvas = app.layout.canvas;
            crate::ui_canvas::draw_canvas(app, &mut buf, canvas);
            // The grid's dots and their colours by distance from the box (10..40 x 10..14 at 100 %).
            let mut dots = Vec::new();
            for y in canvas.y..canvas.bottom() {
                for x in canvas.x..canvas.right() {
                    if buf[(x, y)].symbol() == "·" {
                        let (wx, wy) = app.to_world_f(x as f32, y as f32);
                        let d = distance(WRect::new(10.0, 10.0, 30.0, 4.0), wx, wy);
                        // Outside the box (its status line has a "·" too).
                        if d > 0.0 {
                            dots.push((d, buf[(x, y)].fg));
                        }
                    }
                }
            }
            dots
        };
        let lit = |dots: &[(f32, Color)]| dots.iter().filter(|(_, c)| *c != FAINT).map(|(d, _)| *d).collect::<Vec<f32>>();
        assert!(lit(&draw(&mut app, T0)).is_empty(), "nothing works");
        work(&mut app, "s", Activity::Working);
        // At once (no fade in the TUI), in steps, only near the box.
        let mut colours = std::collections::HashSet::new();
        let mut fronts = Vec::new();
        for t in (0..PERIOD_MS).step_by(80) {
            let dots = draw(&mut app, T0 + t);
            assert!(dots.iter().all(|(d, c)| *d < REACH || *c == FAINT), "only near the box");
            colours.extend(dots.iter().map(|(_, c)| format!("{c:?}")));
            let brightest = dots.iter().filter(|(_, c)| *c == tui_color(1.0) || *c == tui_color(2.0 / 3.0)).map(|(d, _)| *d).fold(0.0, f32::max);
            fronts.push(brightest);
        }
        assert!(colours.len() >= 3 && colours.len() <= TUI_STEPS as usize + 1, "a few steps: {colours:?}");
        assert!(fronts.iter().any(|d| *d > 5.0) && fronts.iter().any(|d| *d > 0.0 && *d <= 5.0), "rings near and farther out: {fronts:?}");
        work(&mut app, "s", Activity::Idle);
        assert!(lit(&draw(&mut app, T0 + PERIOD_MS + 40)).is_empty(), "back at once when it stops");
    }

    #[test]
    fn settings_switch_it_off_and_on_beside_effects_and_remember_it_on_this_machine() {
        use crate::visual::VisualSetting;
        let home = std::env::temp_dir().join(format!("cc-glow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let mut app = app(RenderMode::Gpu);
        app.instance = Some(crate::paths::Instance::new(Some("glow".into()), Some(home.clone()), Some(home.join("sessions"))).unwrap());
        let rows = |app: &App| app.visual_setting_rows().into_iter().map(|r| (r.label, r.right)).collect::<Vec<_>>();
        let labels: Vec<String> = rows(&app).into_iter().map(|r| r.0).collect();
        assert_eq!(labels[labels.len() - 2..], ["Effects".to_owned(), "Working glow".to_owned()], "next to Effects");
        assert_eq!(rows(&app).last().map(|r| r.1.as_str()), Some("on"), "on by default");
        app.visual_setting(VisualSetting::WorkingGlow);
        assert!(!app.working_glow_setting() && rows(&app).last().is_some_and(|r| r.1 == "off"));
        assert_eq!(crate::settings::Settings::load(&home).grid_pulse, Some(false), "kept in settings.json under its old key");
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("Working glow off")));
        app.visual_setting(VisualSetting::WorkingGlow);
        assert!(app.working_glow_setting() && crate::settings::Settings::load(&home).grid_pulse == Some(true));
        let _ = std::fs::remove_dir_all(home);
    }
}