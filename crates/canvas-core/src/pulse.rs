//! The dot grid's breathing while agents work: the canvas's background dots
//! slowly brighten a little towards a light, cool tone ([`GRID_LIT`]) and
//! return to their normal colour while at least one session of the canvas is
//! working (the toolbar's working count; a session whose subagents run is
//! working). The level is a pure function of the animation clock, so it never
//! depends on the frame rate and never restarts while you pan or zoom or work
//! moves from one session to another; it fades in when work starts and out
//! when the last working session stops, turning around mid-way without a jump.
//!
//! The GPU client repaints only its grid layer for it, in frames of its own at
//! most 15 times a second and only when the dots' colour changes
//! (`CanvasModel::grid_busy`: while work runs and during the fade-out); the
//! TUI steps through a few colours on the frames its working spinners draw
//! anyway and, having no frames for a fade-out, switches back at once.
//! Reduced motion, effects off and *Settings › Dot grid pulse* off keep the
//! dots still; the `freeze` hook holds them at their normal colour.

use ratatui::style::Color;

use crate::app::App;
use crate::doc::NodeKind;
use crate::paint::{FAINT, GRID_LIT};
use crate::status::Status;
use crate::view::{Motion, RenderMode};

/// One breath (ms).
pub const PERIOD_MS: u64 = 3_000;
/// A whole fade in or out (ms).
pub const FADE_MS: u64 = 800;
/// How far the dots go from their normal colour towards [`GRID_LIT`] at the
/// top of a breath (about 40 % brighter).
pub const AMPLITUDE: f32 = 0.35;
/// The TUI's lit colours besides the normal one.
pub const TUI_STEPS: u8 = 3;

/// The breath at `clock_ms` (0..1): a raised cosine, 0 (the normal colour)
/// at the start of every period and 1 halfway through.
pub fn breath(clock_ms: u64) -> f32 {
    let t = (clock_ms % PERIOD_MS) as f32 / PERIOD_MS as f32;
    0.5 - 0.5 * (std::f32::consts::TAU * t).cos()
}

/// Ease in and out (smoothstep).
fn ease(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

/// The pulse's fade: in while work runs, out once it stopped. A change
/// mid-way turns it around where it is, so the level never jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fade {
    /// Work runs: fading in (or fully in); else fading out (or fully out).
    pub on: bool,
    /// When it last turned (animation clock, ms) and its position then;
    /// `None`: settled.
    pub since: Option<(u64, f32)>,
}

impl Fade {
    /// Its position at `clock_ms` (0..1, linear in time).
    pub fn position(&self, clock_ms: u64) -> f32 {
        let Some((start, from)) = self.since else { return if self.on { 1.0 } else { 0.0 } };
        let step = clock_ms.saturating_sub(start) as f32 / FADE_MS as f32;
        if self.on { (from + step).min(1.0) } else { (from - step).max(0.0) }
    }

    /// How much of the pulse shows at `clock_ms` (0..1, eased in and out).
    pub fn envelope(&self, clock_ms: u64) -> f32 {
        ease(self.position(clock_ms))
    }

    /// Work runs (`on`) or not from `clock_ms` on.
    pub fn turn(&mut self, on: bool, clock_ms: u64) {
        if on != self.on {
            *self = Fade { on, since: Some((clock_ms, self.position(clock_ms))) };
        }
    }

    /// Still fading in or out at `clock_ms`.
    pub fn changing(&self, clock_ms: u64) -> bool {
        let p = self.position(clock_ms);
        if self.on { p < 1.0 } else { p > 0.0 }
    }

    /// Fading out at `clock_ms`.
    pub fn fading_out(&self, clock_ms: u64) -> bool {
        !self.on && self.changing(clock_ms)
    }

    /// The grid's level at `clock_ms`: how far the dots are from their
    /// normal colour towards [`GRID_LIT`] (0..[`AMPLITUDE`]).
    pub fn level(&self, clock_ms: u64) -> f32 {
        AMPLITUDE * self.envelope(clock_ms) * breath(clock_ms)
    }
}

fn rgb(color: Color) -> [f32; 3] {
    match color {
        Color::Rgb(r, g, b) => [r as f32, g as f32, b as f32],
        _ => [0.0; 3],
    }
}

/// The dots' colour at `level` (0: [`FAINT`], their normal colour), unrounded.
pub fn dot_rgb(level: f32) -> [f32; 3] {
    let (a, b) = (rgb(FAINT), rgb(GRID_LIT));
    let t = level.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// The dots' colour at `level`.
pub fn dot_color(level: f32) -> Color {
    let [r, g, b] = dot_rgb(level).map(|c| c.round() as u8);
    Color::Rgb(r, g, b)
}

/// The TUI's step for `level` (0: the normal colour … [`TUI_STEPS`]).
pub fn step(level: f32) -> u8 {
    ((level / AMPLITUDE).clamp(0.0, 1.0) * TUI_STEPS as f32).round() as u8
}

/// The TUI's dot colour at `level`: the nearest of its few steps.
pub fn tui_color(level: f32) -> Color {
    dot_color(step(level) as f32 / TUI_STEPS as f32 * AMPLITUDE)
}

impl App {
    /// *Settings › Dot grid pulse* (this machine; on unless switched off).
    pub fn grid_pulse_setting(&self) -> bool {
        self.visual.grid_pulse.unwrap_or(true)
    }

    /// Some session of this canvas is working (the toolbar's working count).
    pub fn any_working(&self) -> bool {
        self.doc.nodes.iter().any(|n| n.kind == NodeKind::Session && self.node_status(n) == Status::Working)
    }

    /// The dot grid's pulse at `clock_ms`, brought up to date; `None` when
    /// it shows nothing: nothing works (and a fade-out is over), reduced
    /// motion, effects off, the setting off, or the `freeze` hook (the dots
    /// hold their normal colour).
    pub fn grid_pulse(&mut self, clock_ms: u64) -> Option<Fade> {
        if self.frozen_tick.is_some() {
            return None;
        }
        let allowed = self.motion() == Motion::On && self.grid_pulse_setting();
        let working = allowed && self.any_working();
        // The TUI draws only on its spinner's frames: no fade-out, the dots return at once.
        if !working && (!allowed || self.render_mode == RenderMode::Cells) {
            self.visual.pulse = Fade::default();
            return None;
        }
        self.visual.pulse.turn(working, clock_ms);
        let fade = self.visual.pulse;
        (fade.on || fade.changing(clock_ms)).then_some(fade)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fade that turned on at `start` (and off at `stop`), sampled every
    /// `frame` ms: the level at each sample.
    fn run(start: u64, stop: Option<u64>, frame: u64, until: u64) -> Vec<(u64, f32)> {
        let mut fade = Fade::default();
        let mut out = Vec::new();
        let mut t = 0;
        while t <= until {
            let working = t >= start && stop.is_none_or(|s| t < s);
            // A change is noticed at the frame where it is due (the status update).
            let at = if working && !fade.on { start } else if !working && fade.on { stop.unwrap_or(t) } else { t };
            fade.turn(working, at);
            out.push((t, fade.level(t)));
            t += frame;
        }
        out
    }

    #[test]
    fn a_breath_is_small_slow_and_returns_to_the_normal_colour() {
        let on = Fade { on: true, since: None };
        let levels: Vec<f32> = (0..=PERIOD_MS).step_by(10).map(|ms| on.level(1_000_000 * PERIOD_MS + ms)).collect();
        assert!(levels.iter().all(|l| (0.0..=AMPLITUDE).contains(l)), "bounded");
        assert_eq!(levels[0], 0.0, "the normal colour at the start of a period");
        assert!((on.level(PERIOD_MS / 2) - AMPLITUDE).abs() < 1e-6, "the top halfway");
        assert!((on.level(1_234) - on.level(1_234 + PERIOD_MS)).abs() < 1e-6 && (on.level(77) - on.level(77 + 5 * PERIOD_MS)).abs() < 1e-6, "periodic");
        // The colour: a little lighter and a little cooler at the top, never more.
        let (normal, top) = (dot_rgb(0.0), dot_rgb(AMPLITUDE));
        assert_eq!(dot_color(0.0), FAINT);
        let lift: Vec<f32> = (0..3).map(|i| top[i] - normal[i]).collect();
        assert!(lift.iter().all(|d| *d > 10.0 && *d < 40.0), "{lift:?}");
        assert!(lift[2] > lift[0], "towards blue: {lift:?}");
        let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let gain = luma(top) / luma(normal) - 1.0;
        assert!((0.3..=0.45).contains(&gain), "about +40 %: {gain}");
        let muted = rgb(crate::paint::MUTED);
        assert!(luma(top) < luma(muted) - 20.0, "well below the secondary text");
    }

    #[test]
    fn the_level_moves_smoothly_and_the_same_at_any_frame_rate() {
        // Work from 1 s to 7.3 s, sampled at 30 and 60 frames/s (and 12.5).
        let at = |frame: u64| run(1_000, Some(7_300), frame, 10_000);
        let (a, b, c) = (at(33), at(16), at(80));
        for (t, level) in &a {
            for other in [&b, &c] {
                if let Some((_, l)) = other.iter().find(|(u, _)| u == t) {
                    assert!((level - l).abs() < 1e-6, "{t} ms: {level} vs {l}");
                }
            }
        }
        // Continuous: under 2 % of the amplitude's worth per 16 ms frame, in and out.
        let max = AMPLITUDE * (std::f32::consts::PI / PERIOD_MS as f32 + 1.5 / FADE_MS as f32) * 16.0 + 1e-4;
        for pair in b.windows(2) {
            assert!((pair[1].1 - pair[0].1).abs() <= max, "a jump at {} ms: {} → {}", pair[1].0, pair[0].1, pair[1].1);
        }
        assert!(b.iter().any(|(_, l)| *l > 0.9 * AMPLITUDE), "it breathes");
    }

    #[test]
    fn it_fades_in_when_work_starts_and_out_when_it_stops() {
        let mut fade = Fade::default();
        assert!(!fade.changing(0) && fade.level(PERIOD_MS / 2) == 0.0, "nothing before work");
        // Starts at the top of a breath: no snap, it grows over the fade.
        let start = 10 * PERIOD_MS + PERIOD_MS / 2;
        fade.turn(true, start);
        assert_eq!(fade.level(start), 0.0);
        assert!(fade.level(start + FADE_MS / 4) < 0.25 * AMPLITUDE, "eased in");
        assert!(fade.changing(start + FADE_MS / 2) && !fade.changing(start + FADE_MS));
        assert_eq!(fade.envelope(start + FADE_MS), 1.0);
        // Stops: out over the same time, then nothing.
        let stop = start + 3 * PERIOD_MS;
        fade.turn(false, stop);
        assert!((fade.level(stop) - AMPLITUDE).abs() < 1e-5, "still at the top when work stops");
        assert!(fade.fading_out(stop + 100) && fade.level(stop + FADE_MS / 2) > 0.0);
        assert_eq!(fade.level(stop + FADE_MS), 0.0);
        assert!(!fade.fading_out(stop + FADE_MS));
        // Turned around mid-way: from where it was, never a jump.
        let mut fade = Fade::default();
        fade.turn(true, 0);
        let half = fade.envelope(FADE_MS / 2);
        fade.turn(false, FADE_MS / 2);
        assert_eq!(fade.envelope(FADE_MS / 2), half);
        fade.turn(true, FADE_MS / 2 + 100);
        assert!((fade.envelope(FADE_MS / 2 + 100) - Fade { on: false, since: Some((FADE_MS / 2, 0.5)) }.envelope(FADE_MS / 2 + 100)).abs() < 1e-6);
        // A switch between two working sessions is no change at all.
        let before = fade;
        fade.turn(true, FADE_MS * 3);
        assert_eq!(fade, before);
    }

    #[test]
    fn the_tui_steps_through_a_few_colours() {
        assert_eq!((step(0.0), step(AMPLITUDE * 0.1), step(AMPLITUDE * 0.5), step(AMPLITUDE)), (0, 0, 2, TUI_STEPS));
        assert_eq!(tui_color(0.0), FAINT);
        assert_eq!(tui_color(AMPLITUDE), dot_color(AMPLITUDE));
        let colours: std::collections::HashSet<String> = (0..=100).map(|i| format!("{:?}", tui_color(AMPLITUDE * i as f32 / 100.0))).collect();
        assert_eq!(colours.len(), TUI_STEPS as usize + 1);
    }

    fn app(render_mode: RenderMode) -> App {
        let node: crate::doc::Node = serde_json::from_value(serde_json::json!({ "id": "s", "kind": "session", "name": "Working", "x": 2, "y": 2 })).unwrap();
        let doc = crate::doc::Doc { nodes: vec![node], ..Default::default() };
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

    fn work(app: &mut App, activity: crate::protocol::Activity) {
        let summary = crate::protocol::SessionSummary { id: "s".into(), live: true, tracked: true, activity, ..Default::default() };
        app.sessions.insert("s".into(), summary);
    }

    fn model(app: &mut App, clock_ms: u64) -> crate::view::CanvasModel {
        app.visual.clock_ms = Some(clock_ms);
        let area = app.layout.canvas;
        crate::ui_canvas::canvas_model(app, area)
    }

    #[test]
    fn the_view_model_carries_the_grid_level_while_a_session_works() {
        use crate::protocol::{Activity, AgentStatus, AgentSummary};
        let mut app = app(RenderMode::Gpu);
        let t0 = 100 * PERIOD_MS;
        assert!(model(&mut app, t0).pulse.is_none(), "nothing works");
        work(&mut app, Activity::Working);
        let first = model(&mut app, t0);
        assert!(first.pulse.is_some_and(|f| f.on) && first.grid_level(t0) == 0.0, "it fades in from nothing");
        // Faded in: the level follows the clock, whatever the frames in between.
        let peak = t0 + 2 * PERIOD_MS + PERIOD_MS / 2;
        let m = model(&mut app, peak);
        assert!((m.grid_level(peak) - AMPLITUDE).abs() < 1e-5 && m.grid_level(peak + PERIOD_MS / 2) == 0.0);
        assert!(m.dump().contains("grid pulse on=true envelope=1 level=0.35"), "{}", m.dump());
        // A session whose subagent still runs is working too.
        let mut summary = crate::protocol::SessionSummary { id: "s".into(), live: true, tracked: true, activity: Activity::Idle, ..Default::default() };
        summary.agents.push(AgentSummary { key: "a".into(), name: "Explorer".into(), description: String::new(), status: AgentStatus::Working, finished_ms: None });
        app.sessions.insert("s".into(), summary);
        assert!(app.any_working() && model(&mut app, peak + 40).pulse.is_some_and(|f| f.on));
        // It stops: the level fades out from where it is, then nothing (and no more frames).
        work(&mut app, Activity::Idle);
        let stop = peak + 80;
        let fading = model(&mut app, stop);
        let fade = fading.pulse.expect("fading out");
        assert!(!fade.on && fade.fading_out(stop + 100) && fading.grid_level(stop) > 0.9 * AMPLITUDE);
        assert!(model(&mut app, stop + FADE_MS).pulse.is_none(), "over");
        assert!(!model(&mut app, stop + FADE_MS).dump().contains("grid pulse"));
    }

    #[test]
    fn reduced_motion_effects_off_the_setting_and_the_freeze_hook_keep_the_dots_still() {
        use crate::protocol::Activity;
        let peak = 200 * PERIOD_MS + PERIOD_MS / 2;
        for (what, setup) in [
            ("reduced motion", Box::new(|app: &mut App| app.visual.motion = Some(Motion::Reduced)) as Box<dyn Fn(&mut App)>),
            ("effects off", Box::new(|app: &mut App| app.visual.motion = Some(Motion::Off))),
            ("the setting off", Box::new(|app: &mut App| app.visual.grid_pulse = Some(false))),
            ("the freeze hook", Box::new(|app: &mut App| app.frozen_tick = Some(3))),
        ] {
            let mut app = app(RenderMode::Gpu);
            work(&mut app, Activity::Working);
            model(&mut app, peak - 5 * PERIOD_MS);
            assert!(model(&mut app, peak).grid_level(peak) > 0.0, "{what}: pulsing before");
            setup(&mut app);
            let m = model(&mut app, peak + PERIOD_MS);
            assert!(m.pulse.is_none() && m.grid_level(peak + PERIOD_MS) == 0.0, "{what}: at once, no fade");
            assert_eq!(pulse_color(&m), FAINT, "{what}");
        }
    }

    fn pulse_color(model: &crate::view::CanvasModel) -> Color {
        dot_color(model.grid_level(model.clock_ms))
    }

    #[test]
    fn the_tui_dots_step_on_spinner_frames_and_return_at_once() {
        use crate::protocol::Activity;
        let mut app = app(RenderMode::Cells);
        work(&mut app, Activity::Working);
        let draw = |app: &mut App, clock: u64| {
            app.visual.clock_ms = Some(clock);
            let area = ratatui::layout::Rect::new(0, 0, 122, 42);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            let canvas = app.layout.canvas;
            crate::ui_canvas::draw_canvas(app, &mut buf, canvas);
            // The colour of the grid's dots (the cells drawing "·" away from the box).
            let dots: std::collections::HashSet<String> = (canvas.y..canvas.bottom())
                .flat_map(|y| (60..canvas.right()).map(move |x| (x, y)))
                .filter(|&(x, y)| buf[(x, y)].symbol() == "·")
                .map(|(x, y)| format!("{:?}", buf[(x, y)].fg))
                .collect();
            assert_eq!(dots.len(), 1, "one colour for the whole grid: {dots:?}");
            dots.into_iter().next().unwrap()
        };
        let t0 = 300 * PERIOD_MS;
        assert_eq!(draw(&mut app, t0), format!("{FAINT:?}"));
        let peak = t0 + PERIOD_MS + PERIOD_MS / 2;
        assert_eq!(draw(&mut app, peak), format!("{:?}", tui_color(AMPLITUDE)), "the top step");
        work(&mut app, Activity::Idle);
        assert_eq!(draw(&mut app, peak + 40), format!("{FAINT:?}"), "back at once when work stops");
        assert!(app.grid_pulse(peak + 80).is_none(), "no fade-out: the TUI would have no frames for it");
    }

    #[test]
    fn the_gpu_client_repaints_the_grid_only_while_it_shows() {
        use crate::protocol::Activity;
        let mut app = app(RenderMode::Gpu);
        let t0 = 400 * PERIOD_MS;
        assert!(!model(&mut app, t0).grid_busy(t0), "nothing works: no grid frames");
        work(&mut app, Activity::Working);
        let m = model(&mut app, t0);
        assert!(m.grid_busy(t0) && m.grid_busy(t0 + 10 * PERIOD_MS), "while work runs");
        work(&mut app, Activity::Idle);
        let stop = t0 + 2 * PERIOD_MS + 40;
        let m = model(&mut app, stop);
        assert!(m.grid_busy(stop) && m.grid_busy(stop + FADE_MS / 2), "the fade-out");
        assert!(!m.grid_busy(stop + FADE_MS) && !m.grid_busy(stop + 10 * FADE_MS), "then none, even before the next model");
        assert!(!model(&mut app, stop + FADE_MS).grid_busy(stop + FADE_MS));
    }

    #[test]
    fn settings_switch_it_off_and_on_beside_effects_and_remember_it_on_this_machine() {
        use crate::visual::VisualSetting;
        let home = std::env::temp_dir().join(format!("cc-pulse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let mut app = app(RenderMode::Gpu);
        app.instance = Some(crate::paths::Instance::new(Some("pulse".into()), Some(home.clone()), Some(home.join("sessions"))).unwrap());
        let rows = |app: &App| app.visual_setting_rows().into_iter().map(|r| (r.label, r.right)).collect::<Vec<_>>();
        let labels: Vec<String> = rows(&app).into_iter().map(|r| r.0).collect();
        assert_eq!(labels[labels.len() - 2..], ["Effects".to_owned(), "Dot grid pulse".to_owned()], "next to Effects");
        assert_eq!(rows(&app).last().map(|r| r.1.as_str()), Some("on"), "on by default");
        app.visual_setting(VisualSetting::GridPulse);
        assert!(!app.grid_pulse_setting() && rows(&app).last().is_some_and(|r| r.1 == "off"));
        let stored = crate::settings::Settings::load(&home);
        assert_eq!(stored.grid_pulse, Some(false), "kept in settings.json, where Effects is");
        assert!(app.notice.as_ref().is_some_and(|n| n.text.contains("Dot grid pulse off")));
        app.visual_setting(VisualSetting::GridPulse);
        assert!(app.grid_pulse_setting() && crate::settings::Settings::load(&home).grid_pulse == Some(true));
        let _ = std::fs::remove_dir_all(home);
    }
}
