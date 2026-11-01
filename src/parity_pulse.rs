//! The parity suite's check of the dot grid's pulse (row 47), on a canvas of
//! its own: one session working from its synthetic history (a tool running;
//! this process holds its in-use lock) in a small group, with empty canvas
//! around it. PNG captures at the top and the bottom of a breath, scheduled on
//! the animation clock this process shares with the GUI (each capture reports
//! the clock and the level it was drawn at, so the check never depends on the
//! time between requests), show the grid's dots measurably lighter at the top
//! and at their normal colour at the bottom, never beyond the pulse's range.
//! Reduced motion, effects off and *Settings › Dot grid pulse* off keep them
//! at their normal colour. While work runs only the grid layer is repainted
//! for it (at most 15 times a second); none once nothing works.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check, event, png_rgb, session_files};
use crate::client::doc::{Doc, Group, Node};
use crate::client::pulse::{AMPLITUDE, PERIOD_MS, breath, dot_rgb};
use crate::selftest::Ui;

const NAME: &str = "Pulse worker";

/// The pulse canvas: "Pulse worker" at 2,2 in "Pulse group".
fn canvas(home: &Path, work: &Path, id: &str) -> Result<PathBuf> {
    let mut doc = Doc::default();
    let mut node: Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": NAME, "x": 2, "y": 2 }))?;
    node.cwd = Some(work.display().to_string());
    node.reviewed_ms = Some(crate::paths::now_ms());
    doc.nodes.push(node);
    doc.groups.push(Group { id: "g-pulse".into(), name: "Pulse group".into(), x: 0, y: 0, w: 36, h: 8, color: 0, ..Default::default() });
    doc.view.x = -1;
    doc.view.y = -1;
    let path = home.join("pulse.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(
        rows,
        "47",
        "New: the dot grid breathes gently while an agent works (fades in and out; still with reduced motion, effects off or the setting off; no frames when nothing works)",
        "grid_pulse_while_working",
        || pulse(sb),
    );
}

/// A PNG capture with the clock and grid level it was drawn at.
struct Capture {
    clock: u64,
    level: f64,
    dots: Vec<(u32, u32)>,
    pixels: Vec<[u8; 3]>,
    width: usize,
}

impl Capture {
    /// A dot's colour: the brightest pixel of the 2 × 2 around its centre
    /// (its quad is centred on a whole pixel).
    fn dot(&self, (x, y): (u32, u32)) -> [u8; 3] {
        let at = |dx: u32, dy: u32| self.pixels.get(y.saturating_sub(dy) as usize * self.width + x.saturating_sub(dx) as usize).copied().unwrap_or_default();
        [at(0, 0), at(1, 0), at(0, 1), at(1, 1)].into_iter().max_by_key(|c| c.iter().map(|v| *v as u32).sum::<u32>()).unwrap_or_default()
    }
}

fn capture(gui: &Gui, dir: &Path, name: &str) -> Result<Capture> {
    let path = dir.join(format!("{name}.png"));
    let reply = gui.req(json!({ "cmd": "png", "path": path }))?;
    let grid = &reply["grid"];
    let dots = grid["dots"].as_array().context("the grid's dots")?.iter().filter_map(|d| Some((d[0].as_f64()? as u32, d[1].as_f64()? as u32))).collect();
    let (width, pixels) = png_rgb(&path)?;
    Ok(Capture { clock: reply["clock_ms"].as_u64().context("clock_ms")?, level: grid["level"].as_f64().context("the grid's level")?, dots, pixels, width })
}

/// Frames the GUI presented in `secs` seconds: (all besides those that only
/// moved loop dots (the sandbox's automations loop on their own), repaints
/// of the grid layer (in frames of its own or in loop frames), full frames).
fn frames_in(gui: &Gui, secs: f64) -> Result<(u64, u64, u64)> {
    frames_why(gui, secs).map(|(frames, _)| frames)
}

/// As [`frames_in`], with what made full frames besides the core's changes:
/// presents skipped (each retried in full) and redraws the window system
/// asked for.
/// Frames (not loops), grid paints, full frames; skipped presents, redraws asked.
type FramesWhy = ((u64, u64, u64), (u64, u64));

fn frames_why(gui: &Gui, secs: f64) -> Result<FramesWhy> {
    let counts = |p: &Value| {
        let n = |key: &str| p["perf"][key].as_u64().unwrap_or(0);
        [n("presented").saturating_sub(n("loop_frames")), n("grid_paints"), n("full_frames"), n("skipped"), n("redraw_requests")]
    };
    let before = counts(&gui.req(json!({ "cmd": "perf" }))?);
    std::thread::sleep(Duration::from_secs_f64(secs));
    let after = counts(&gui.req(json!({ "cmd": "perf" }))?);
    let d = |i: usize| after[i].saturating_sub(before[i]);
    Ok(((d(0), d(1), d(2)), (d(3), d(4))))
}

fn pulse_state(tree: &Value) -> &Value {
    &tree["canvas"]["grid_pulse"]
}

fn status(tree: &Value) -> Option<String> {
    tree["canvas"]["boxes"].as_array()?.iter().find(|b| b["name"] == NAME).and_then(|b| b["status"].as_str().map(str::to_owned))
}

/// The capture at the next `phase` of a breath (0: its bottom, 0.5: its
/// top) whose level `ok` accepts; a few tries, each aimed by the shared clock.
fn at_phase(gui: &Gui, dir: &Path, name: &str, phase: f64, ok: impl Fn(f64) -> bool) -> Result<Capture> {
    let mut seen = Vec::new();
    for attempt in 0..6 {
        let now = crate::paths::now_ms();
        let target = (phase * PERIOD_MS as f64) as u64;
        let wait = (target + PERIOD_MS - now % PERIOD_MS) % PERIOD_MS;
        std::thread::sleep(Duration::from_millis(wait.saturating_sub(25)));
        let c = capture(gui, dir, &format!("{name}-{attempt}"))?;
        if ok(c.level) {
            return Ok(c);
        }
        seen.push(format!("{:.3} at {} ms", c.level, c.clock % PERIOD_MS));
    }
    bail!("no capture near the {name} of a breath: {}", seen.join(", "))
}

/// The sampled dots of `c` against the normal colours of `base`, per dot:
/// (channel differences).
fn lifts(base: &Capture, c: &Capture) -> Result<Vec<[i32; 3]>> {
    anyhow::ensure!(c.dots == base.dots, "the view moved between captures ({} vs {} dots)", c.dots.len(), base.dots.len());
    Ok(base.dots.iter().map(|&d| {
        let (a, b) = (base.dot(d), c.dot(d));
        [b[0] as i32 - a[0] as i32, b[1] as i32 - a[1] as i32, b[2] as i32 - a[2] as i32]
    }).collect())
}

fn still(base: &Capture, c: &Capture, what: &str) -> Result<()> {
    anyhow::ensure!(c.level == 0.0, "{what}: the grid's level is {}", c.level);
    let moved = lifts(base, c)?.into_iter().filter(|l| l.iter().any(|v| *v != 0)).count();
    anyhow::ensure!(moved == 0, "{what}: {moved} dot(s) not at their normal colour");
    Ok(())
}

fn pulse(sb: &Sandbox) -> Result<String> {
    // Effects on whatever this machine says; the setting only from the sandbox.
    // SAFETY: set before the GUI starts; nothing else reads them concurrently.
    unsafe {
        std::env::set_var("COPILOT_CANVAS_EFFECTS", "on");
        std::env::remove_var("COPILOT_CANVAS_GRID_PULSE");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::paths::now_ms();
    let history = [
        event("session.start", now - 9000, json!({})),
        event("user.message", now - 8000, json!({ "content": "pulse" })),
        event("assistant.turn_start", now - 7000, json!({})),
        event("tool.execution_start", now - 6000, json!({ "toolCallId": "p1", "toolName": "powershell" })),
    ];
    session_files(&sb.instance.sessions_dir, &id, NAME, &sb.work, &history, true)?;
    let path = canvas(&sb.instance.home, &sb.work, &id)?;
    let dir = std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| sb.root.join("pulse"));
    std::fs::create_dir_all(&dir)?;
    let arg = path.display().to_string();
    let mut gui = sb.start(&["--canvas", &arg], "gui-pulse.log")?;
    let result = steps(sb, &mut gui, &id, &dir);
    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
    result
}

fn steps(sb: &Sandbox, gui: &mut Gui, id: &str, dir: &Path) -> Result<String> {
    gui.wait("the pulse canvas", 25, |t| t.contains("Pulse group"))?;
    gui.wait_tree("the session working, the grid breathing", 25, |t| status(t).as_deref() == Some("working") && pulse_state(t)["on"] == true)?;
    // Reduced motion and effects off: the dots' normal colour (the reference).
    gui.req(json!({ "cmd": "motion", "value": "reduced" }))?;
    gui.wait_tree("reduced motion: no pulse", 5, |t| pulse_state(t)["on"].is_null())?;
    let base = capture(gui, dir, "pulse-reduced-0")?;
    anyhow::ensure!(base.dots.len() >= 8, "only {} dots clear of the boxes", base.dots.len());
    let (_, grid_reduced, _) = frames_in(gui, PERIOD_MS as f64 / 2000.0)?;
    anyhow::ensure!(grid_reduced == 0, "{grid_reduced} grid repaints with reduced motion");
    still(&base, &capture(gui, dir, "pulse-reduced-1")?, "reduced motion, half a breath later")?;
    gui.req(json!({ "cmd": "motion", "value": "off" }))?;
    gui.wait_tree("effects off: no pulse", 5, |t| pulse_state(t)["on"].is_null())?;
    still(&base, &capture(gui, dir, "pulse-effects-off")?, "effects off")?;
    // Motion on: it fades in, then breathes with the clock.
    gui.req(json!({ "cmd": "motion", "value": "on" }))?;
    gui.wait_tree("faded in", 5, |t| pulse_state(t)["envelope"].as_f64().is_some_and(|e| e >= 0.999))?;
    let a = AMPLITUDE as f64;
    let top = at_phase(gui, dir, "pulse-top", 0.5, |l| l >= 0.85 * a)?;
    let bottom = at_phase(gui, dir, "pulse-bottom", 0.0, |l| l <= 0.15 * a)?;
    for c in [&top, &bottom] {
        let expected = a * breath(c.clock) as f64;
        anyhow::ensure!((c.level - expected).abs() < 0.005, "the level {:.4} is not the clock's ({expected:.4} at {} ms)", c.level, c.clock);
    }
    let apart = (top.clock as i64 - bottom.clock as i64).rem_euclid(PERIOD_MS as i64);
    anyhow::ensure!((apart - PERIOD_MS as i64 / 2).abs() <= PERIOD_MS as i64 * 3 / 10, "the captures are {apart} ms apart in the breath");
    // At the top: lighter and a little cooler, within the pulse's range; at the bottom: the normal colour.
    let range = {
        let (normal, lit) = (dot_rgb(0.0), dot_rgb(AMPLITUDE));
        [lit[0] - normal[0], lit[1] - normal[1], lit[2] - normal[2]].map(|v| v.ceil() as i32)
    };
    let up = lifts(&base, &top)?;
    for l in &up {
        anyhow::ensure!((0..3).all(|i| l[i] >= -1 && l[i] <= range[i] + 3), "a dot outside the pulse's range: {l:?} (at most {range:?})");
        anyhow::ensure!(l[2] >= l[0], "not cooler: {l:?}");
    }
    let mean = |v: &[[i32; 3]], i: usize| v.iter().map(|l| l[i] as f64).sum::<f64>() / v.len() as f64;
    let lift = [mean(&up, 0), mean(&up, 1), mean(&up, 2)];
    anyhow::ensure!(lift.iter().sum::<f64>() >= 25.0, "the top of a breath is not measurably lighter: {lift:?}");
    let down = lifts(&base, &bottom)?;
    anyhow::ensure!(down.iter().all(|l| l.iter().all(|v| v.abs() <= 8)), "the bottom of a breath is not the normal colour: {:?}", down.iter().find(|l| l.iter().any(|v| v.abs() > 8)));
    // While work runs only the grid layer is repainted for it, about 15 times a second at
    // most (when its colour changes); with the pulse off never, and no fewer full frames.
    let ((_, grid_on, full_on), (skipped_on, asked_on)) = frames_why(gui, 2.0)?;
    gui.req(json!({ "cmd": "grid_pulse", "value": "off" }))?;
    std::thread::sleep(Duration::from_millis(200));
    let ((_, grid_off, full_off), (skipped_off, asked_off)) = frames_why(gui, 2.0)?;
    gui.req(json!({ "cmd": "grid_pulse", "value": "on" }))?;
    anyhow::ensure!(
        (5..=44).contains(&grid_on) && grid_off == 0 && full_on <= full_off + 3,
        "in 2 s while a session works: {grid_on} grid repaints and {full_on} full frames with the pulse on, {grid_off} and {full_off} with it off (presents skipped {skipped_on} and {skipped_off}, redraws asked by the window system {asked_on} and {asked_off})"
    );
    // Settings › Dot grid pulse off (saved on this machine): still; on again.
    let toggle = |gui: &mut Gui, on: bool| -> Result<()> {
        gui.chord('8', &["alt"])?;
        gui.wait("the Settings view's Dot grid pulse row", 10, |t| t.contains("Dot grid pulse"))?;
        let at = gui.find("Dot grid pulse").context("the Dot grid pulse row")?;
        gui.click(at, &[])?;
        let want = format!("\"grid_pulse\": {on}");
        let settings = sb.instance.home.join("settings.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&settings).unwrap_or_default().contains(&want) {
            anyhow::ensure!(std::time::Instant::now() < deadline, "settings.json does not say {want}: {}", std::fs::read_to_string(&settings).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(100));
        }
        gui.chord('1', &["alt"])?;
        Ok(())
    };
    toggle(gui, false)?;
    gui.wait_tree("the setting off: no pulse", 5, |t| pulse_state(t)["on"].is_null() && status(t).as_deref() == Some("working"))?;
    std::thread::sleep(Duration::from_millis(300));
    still(&base, &capture(gui, dir, "pulse-setting-off")?, "the setting off")?;
    toggle(gui, true)?;
    gui.wait_tree("the setting on again: breathing", 5, |t| pulse_state(t)["on"] == true)?;
    // Nothing works (an Esc in the agent): the pulse fades out, then no frames at all.
    let events = sb.instance.sessions_dir.join(id).join("events.jsonl");
    let mut text = std::fs::read_to_string(&events)?;
    text.push_str(&(event("abort", crate::paths::now_ms(), json!({})) + "\n"));
    std::fs::write(&events, text)?;
    let mut fading = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let tree = gui.tree()?;
        fading |= pulse_state(&tree)["on"] == false;
        if status(&tree).as_deref() == Some("idle") && pulse_state(&tree)["on"].is_null() {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "the pulse did not end: {} ({:?})", pulse_state(&tree), status(&tree));
        std::thread::sleep(Duration::from_millis(60));
    }
    std::thread::sleep(Duration::from_millis(500));
    // Only the window's housekeeping (the service's statistics in the status bar, about one frame in 2 s).
    let (idle_frames, idle_grid, _) = frames_in(gui, 2.0)?;
    anyhow::ensure!(idle_frames <= 3 && idle_grid == 0, "{idle_frames} frames ({idle_grid} grid repaints) in 2 s with nothing working");
    still(&base, &capture(gui, dir, "pulse-nothing-works")?, "nothing working")?;
    Ok(format!(
        "{} dots: top of a breath +{:.0}/+{:.0}/+{:.0} (level {:.3} at {} ms), bottom ±{} (level {:.3}, {apart} ms apart in the breath); while working in 2 s: {grid_on} grid repaints, {full_on} full frames ({grid_off} and {full_off} with the pulse off); still with reduced motion, effects off and the setting off (saved); fade-out seen: {fading}, then {idle_frames} frames in 2 s",
        base.dots.len(),
        lift[0],
        lift[1],
        lift[2],
        top.level,
        top.clock % PERIOD_MS,
        down.iter().flat_map(|l| l.iter().map(|v| v.abs())).max().unwrap_or(0),
        bottom.level,
    ))
}
