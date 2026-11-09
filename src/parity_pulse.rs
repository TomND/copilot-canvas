//! The parity suite's check of the working glow (row 51), on a canvas of its
//! own: "Glow worker" working from its synthetic history (a tool running;
//! this process holds its in-use lock) and "Glow idle" far below it, with
//! empty canvas around both. PNG captures scheduled on the animation clock
//! this process shares with the GUI (each `png` reply carries the clock it
//! was drawn at and, for that instant, the glow's rings and sample dots with
//! their distance from the nearest box and the colour the model gives them,
//! so nothing depends on the time between requests) show the dots near the
//! working box brighter than far ones, exactly as the model says, and its
//! bright ring farther from the box a moment later. Reduced motion: a static
//! glow (two captures alike); effects off, *Settings › Working glow* off and
//! nothing working: the dots' normal colour. Only the glow's layer is
//! repainted for it while rings move; nothing once nothing works.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{Gui, Row, Sandbox, check, event, png_rgb, session_files};
use crate::client::doc::{Doc, Node};
use crate::client::glow::{BAND, PERIOD_MS, REACH, SPEED};
use crate::selftest::Ui;

const NAME: &str = "Glow worker";
/// The canvas background behind the dots (the GUI's theme BG).
const BACKGROUND: [u8; 3] = [13, 17, 23];
const IDLE: &str = "Glow idle";

/// The glow canvas: the worker at 24,12 and the idle box at 24,30 (out of
/// the worker's reach), the view at -1,-1.
fn canvas(home: &Path, work: &Path, id: &str, idle: &str) -> Result<PathBuf> {
    let mut doc = Doc::default();
    for (id, name, y) in [(id, NAME, 12), (idle, IDLE, 30)] {
        let mut node: Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": name, "x": 24, "y": y }))?;
        node.cwd = Some(work.display().to_string());
        node.reviewed_ms = Some(crate::paths::now_ms());
        doc.nodes.push(node);
    }
    doc.view.x = -1;
    doc.view.y = -1;
    let path = home.join("glow.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(
        rows,
        "51",
        "New: the dots around a working session glow in rings that travel outward from its box (static with reduced motion; none with effects off, the setting off or nothing working; only the glow's layer repainted for it)",
        "working_glow_from_working_boxes",
        || glow(sb),
    );
}

/// A sampled grid dot of a capture.
struct Dot {
    x: u32,
    y: u32,
    /// Distance from the nearest box's outline (world units) and that box.
    d: f64,
    worker: bool,
    /// The colour the model gives it at the capture's clock.
    model: [u8; 3],
}

/// A PNG capture with the clock it was drawn at and the glow then.
struct Capture {
    name: String,
    clock: u64,
    glow: Value,
    dots: Vec<Dot>,
    pixels: Vec<[u8; 3]>,
    width: usize,
}

fn luma(c: [u8; 3]) -> f64 {
    0.2126 * c[0] as f64 + 0.7152 * c[1] as f64 + 0.0722 * c[2] as f64
}

impl Capture {
    /// A dot's colour: the brightest pixel of the 2 × 2 around its centre
    /// (its quad is centred on a whole pixel).
    fn pixel(&self, dot: &Dot) -> [u8; 3] {
        let at = |dx: u32, dy: u32| self.pixels.get(dot.y.saturating_sub(dy) as usize * self.width + dot.x.saturating_sub(dx) as usize).copied().unwrap_or_default();
        [at(0, 0), at(1, 0), at(0, 1), at(1, 1)].into_iter().max_by_key(|c| c.iter().map(|v| *v as u32).sum::<u32>()).unwrap_or_default()
    }

    /// The youngest ring's front (world units from the outline), if one runs.
    fn front(&self) -> Option<f64> {
        self.glow["sources"][0]["rings"][0][0].as_f64()
    }

    /// The worker's dots within `range` of its outline.
    fn worker(&self, range: std::ops::RangeInclusive<f64>) -> impl Iterator<Item = &Dot> {
        self.dots.iter().filter(move |d| d.worker && range.contains(&d.d))
    }

    /// The mean brightness of the worker's dots within `range` above `normal`.
    fn lift(&self, range: std::ops::RangeInclusive<f64>, normal: f64) -> f64 {
        let lifts: Vec<f64> = self.worker(range).map(|d| luma(self.pixel(d)) - normal).collect();
        lifts.iter().sum::<f64>() / lifts.len().max(1) as f64
    }

    /// The mean brightness above `normal` of the dots beyond every box's reach.
    fn far_lift(&self, normal: f64) -> f64 {
        let lifts: Vec<f64> = self.dots.iter().filter(|d| d.d > REACH as f64 + 2.0).map(|d| luma(self.pixel(d)) - normal).collect();
        lifts.iter().sum::<f64>() / lifts.len().max(1) as f64
    }

    /// Where the worker's glow is, as the distance its lit dots' brightness
    /// above `normal` is centred on.
    fn centre(&self, normal: f64) -> f64 {
        let (mut sum, mut weight) = (0.0, 0.0);
        for dot in self.worker(0.0..=REACH as f64) {
            let lift = (luma(self.pixel(dot)) - normal).max(0.0);
            sum += lift * dot.d;
            weight += lift;
        }
        if weight > 0.0 { sum / weight } else { 0.0 }
    }

    /// Every sampled dot is drawn in the colour the model gives it: no brighter, and no
    /// dimmer than that colour over the background at the capture's dot `coverage`
    /// (1.0 where a dot covers its centre pixel; lit dots are larger, so they may cover more).
    fn as_modelled(&self, coverage: f64) -> Result<()> {
        for dot in &self.dots {
            let px = self.pixel(dot);
            let low = |i: usize| BACKGROUND[i] as f64 + coverage * (dot.model[i] as f64 - BACKGROUND[i] as f64);
            ensure!(
                (0..3).all(|i| px[i] as i32 <= dot.model[i] as i32 + 6 && px[i] as f64 >= low(i) - 6.0),
                "{}: the dot at ({}, {}) is {px:?}, the model says {:?} (coverage {coverage:.2})",
                self.name,
                dot.x,
                dot.y,
                dot.model
            );
        }
        Ok(())
    }

    /// Every sampled dot has its normal colour.
    fn normal(&self, normal: [u8; 3], what: &str) -> Result<()> {
        for dot in &self.dots {
            let px = self.pixel(dot);
            ensure!((0..3).all(|i| (px[i] as i32 - normal[i] as i32).abs() <= 3), "{what}: the dot at ({}, {}), {:.1} from a box, is {px:?}, not the normal {normal:?}", dot.x, dot.y, dot.d);
        }
        Ok(())
    }
}

fn capture(gui: &Gui, dir: &Path, name: &str, worker: &str) -> Result<Capture> {
    let path = dir.join(format!("{name}.png"));
    let reply = gui.req(json!({ "cmd": "png", "path": path }))?;
    let glow = reply["glow"].clone();
    let dots = glow["dots"]
        .as_array()
        .context("the glow's sample dots")?
        .iter()
        .filter_map(|d| {
            let rgb = d["rgb"].as_array()?;
            Some(Dot {
                x: d["x"].as_f64()? as u32,
                y: d["y"].as_f64()? as u32,
                d: d["d"].as_f64()?,
                worker: d["box"] == worker,
                model: [rgb.first()?.as_u64()? as u8, rgb.get(1)?.as_u64()? as u8, rgb.get(2)?.as_u64()? as u8],
            })
        })
        .collect();
    let (width, pixels) = png_rgb(&path)?;
    Ok(Capture { name: name.to_owned(), clock: reply["clock_ms"].as_u64().context("clock_ms")?, glow, dots, pixels, width })
}

/// The capture at the next moment the youngest ring's front is in `front`
/// (world units from the outline); a few tries, each aimed by the shared clock.
fn at_front(gui: &Gui, dir: &Path, name: &str, worker: &str, offset: u64, front: std::ops::RangeInclusive<f64>) -> Result<Capture> {
    let mut seen = Vec::new();
    let age = (((front.start() + front.end()) / 2.0 + BAND as f64) / SPEED as f64) as u64;
    for attempt in 0..6 {
        let now = crate::paths::now_ms();
        let wait = (age + PERIOD_MS - (now + offset) % PERIOD_MS) % PERIOD_MS;
        std::thread::sleep(Duration::from_millis(wait.saturating_sub(20)));
        let c = capture(gui, dir, &format!("{name}-{attempt}"), worker)?;
        if c.front().is_some_and(|f| front.contains(&f)) {
            return Ok(c);
        }
        seen.push(format!("{:?} at {} ms", c.front(), (c.clock + offset) % PERIOD_MS));
    }
    bail!("no capture with a ring {front:?} from the box: {}", seen.join(", "))
}

/// Frames the GUI presented in `secs` seconds: (all besides those that only
/// moved loop dots (the sandbox's automations loop on their own), repaints of
/// the glow layer (in frames of its own or riding on others), full frames).
fn frames_in(gui: &Gui, secs: f64) -> Result<(u64, u64, u64)> {
    frames_why(gui, secs).map(|(frames, _)| frames)
}

/// Frames (not loops), glow paints, full frames; skipped presents, redraws asked.
type FramesWhy = ((u64, u64, u64), (u64, u64));

/// As [`frames_in`], with what made full frames besides the core's changes:
/// presents skipped (each retried in full) and redraws the window system
/// asked for.
fn frames_why(gui: &Gui, secs: f64) -> Result<FramesWhy> {
    let counts = |p: &Value| {
        let n = |key: &str| p["perf"][key].as_u64().unwrap_or(0);
        [n("presented").saturating_sub(n("loop_frames")), n("glow_paints"), n("full_frames"), n("skipped"), n("redraw_requests")]
    };
    let before = counts(&gui.req(json!({ "cmd": "perf" }))?);
    std::thread::sleep(Duration::from_secs_f64(secs));
    let after = counts(&gui.req(json!({ "cmd": "perf" }))?);
    let d = |i: usize| after[i].saturating_sub(before[i]);
    Ok(((d(0), d(1), d(2)), (d(3), d(4))))
}

fn glow_state(tree: &Value) -> &Value {
    &tree["canvas"]["glow"]
}

fn status(tree: &Value, name: &str) -> Option<String> {
    tree["canvas"]["boxes"].as_array()?.iter().find(|b| b["name"] == name).and_then(|b| b["status"].as_str().map(str::to_owned))
}

/// The worker's glow is in, fully faded in, moving.
fn glowing(tree: &Value) -> bool {
    let g = glow_state(tree);
    g["moving"] == true && g["sources"].as_array().is_some_and(|s| s.len() == 1 && s[0]["on"] == true && s[0]["envelope"].as_f64().is_some_and(|e| e >= 0.999))
}

fn glow(sb: &Sandbox) -> Result<String> {
    // Effects on whatever this machine says; the setting only from the sandbox.
    // SAFETY: set before the GUI starts; nothing else reads them concurrently.
    unsafe {
        std::env::set_var("ATELIER_EFFECTS", "on");
        std::env::remove_var("ATELIER_GRID_PULSE");
    }
    let (id, idle) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
    let now = crate::paths::now_ms();
    let history = [
        event("session.start", now - 9000, json!({})),
        event("user.message", now - 8000, json!({ "content": "glow" })),
        event("assistant.turn_start", now - 7000, json!({})),
        event("tool.execution_start", now - 6000, json!({ "toolCallId": "g1", "toolName": "powershell" })),
    ];
    session_files(&sb.instance.sessions_dir, &id, NAME, &sb.work, &history, true)?;
    let quiet = [event("session.start", now - 9000, json!({})), event("user.message", now - 8000, json!({ "content": "rest" })), event("session.idle", now - 7000, json!({}))];
    session_files(&sb.instance.sessions_dir, &idle, IDLE, &sb.work, &quiet, true)?;
    let path = canvas(&sb.instance.home, &sb.work, &id, &idle)?;
    let dir = std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| sb.root.join("glow"));
    std::fs::create_dir_all(&dir)?;
    let arg = path.display().to_string();
    let mut gui = sb.start(&["--canvas", &arg], "gui-glow.log")?;
    let result = steps(sb, &mut gui, &id, &dir);
    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
    result
}

fn steps(sb: &Sandbox, gui: &mut Gui, id: &str, dir: &Path) -> Result<String> {
    gui.wait("the glow canvas", 25, |t| t.contains(NAME) && t.contains(IDLE))?;
    // At 100 % scale, as on CI's Windows display, wherever Windows runs it: the dots are smallest there.
    if cfg!(windows) {
        gui.req(json!({ "cmd": "dpi", "scale": 1.0 }))?;
        gui.send("f")?;
    }
    let tree = gui.wait_tree("the worker working, its glow faded in", 25, |t| status(t, NAME).as_deref() == Some("working") && status(t, IDLE).as_deref() == Some("idle") && glowing(t))?;
    let offset = glow_state(&tree)["sources"][0]["offset"].as_u64().context("the ring offset")?;
    ensure!(offset == crate::client::glow::offset(id), "the rings' offset is the box's");
    // Effects off: the dots' normal colour (the reference), no glow.
    gui.req(json!({ "cmd": "motion", "value": "off" }))?;
    gui.wait_tree("effects off: no glow", 5, |t| glow_state(t)["active"] == false)?;
    let off = capture(gui, dir, "glow-effects-off", id)?;
    let near = off.worker(0.0..=6.0).count();
    let far = off.dots.iter().filter(|d| d.d > REACH as f64 + 2.0).count();
    ensure!(near >= 6 && far >= 6, "{near} dots within 6 of the worker and {far} far from every box sampled ({} in all)", off.dots.len());
    let normal = off.pixel(off.dots.last().context("a far dot")?);
    let faint = crate::client::glow::dot_rgb(0.0).map(|c| c as u8);
    // A small dot covers its centre pixels only partly at some scales (on CI's 100 % display
    // [47, 53, 62] for [56, 62, 72]): the capture's own normal dot is the reference, and it must
    // lie between the background and the model's colour.
    let bg = luma(BACKGROUND);
    ensure!(luma(normal) > bg + 8.0 && (0..3).all(|i| normal[i] as i32 <= faint[i] as i32 + 6), "the normal dot is {normal:?}, not a dot of {faint:?} over the background {BACKGROUND:?}");
    let coverage = ((luma(normal) - bg) / (luma(faint) - bg)).clamp(0.0, 1.0);
    off.normal(normal, "effects off")?;
    let base = luma(normal);
    // Motion on: rings leave the box. A capture as one passes the nearest dots and one as it is
    // at least 3 cells farther out (the same ring, about half a second later).
    gui.req(json!({ "cmd": "motion", "value": "on" }))?;
    gui.wait_tree("motion on: the glow faded in", 5, glowing)?;
    let a = at_front(gui, dir, "glow-ring-near", id, offset, 3.0..=5.5)?;
    let fa = a.front().unwrap_or_default();
    let b = at_front(gui, dir, "glow-ring-far", id, offset, (fa + 3.0).max(7.0)..=10.5)?;
    for c in [&a, &b] {
        c.as_modelled(coverage)?;
    }
    let (a_near, a_far) = (a.lift(0.0..=6.0, base), a.far_lift(base));
    ensure!(a_near >= 25.0 * coverage && a_far.abs() <= 2.0, "near the box the dots are not clearly brighter than far away: +{a_near:.1} vs {a_far:+.1}");
    let peak = a.worker(0.0..=6.0).map(|d| luma(a.pixel(d)) / base).fold(0.0, f64::max);
    let (ca, cb) = (a.centre(base), b.centre(base));
    let (a_inner, b_inner, a_outer, b_outer) = (a.lift(0.0..=5.0, base), b.lift(0.0..=5.0, base), a.lift(7.0..=11.0, base), b.lift(7.0..=11.0, base));
    ensure!(
        cb > ca + 1.0 && b_outer > a_outer + 10.0 * coverage && b_inner < a_inner - 20.0 * coverage,
        "the bright ring did not move out: centred {ca:.1} then {cb:.1} from the box ({} ms apart, fronts {fa:.1} and {:.1}; 7-11 out +{a_outer:.1} then +{b_outer:.1}; within 5 +{a_inner:.1} then +{b_inner:.1})",
        b.clock.abs_diff(a.clock),
        b.front().unwrap_or_default()
    );
    // The idle box has no glow: the dots near it keep their colour.
    let idle: Vec<&Dot> = a.dots.iter().filter(|d| !d.worker && d.d <= 6.0).collect();
    ensure!(idle.iter().all(|d| luma(a.pixel(d)) - base <= 2.0), "a dot near the idle box is lit");
    // While rings move only the glow's layer is repainted for it (30 a second at most);
    // with the glow off never, and no fewer full frames.
    let ((_, glow_on, full_on), (skipped_on, asked_on)) = frames_why(gui, 2.0)?;
    gui.req(json!({ "cmd": "grid_pulse", "value": "off" }))?;
    std::thread::sleep(Duration::from_millis(200));
    let ((_, glow_off, full_off), (skipped_off, asked_off)) = frames_why(gui, 2.0)?;
    gui.req(json!({ "cmd": "grid_pulse", "value": "on" }))?;
    // Were glow repaints full frames, each would add one. The working session's spinner
    // draws full frames on the core's tick, and a glow repaint can delay a tick enough to
    // show a few more spinner steps (half the glow repaints, at least 3, is the margin).
    // With a software renderer (WARP on CI's Windows runners, llvmpipe on Linux's) every
    // present takes tens of ms, so the spinner's redraw stays pending into the next 40 ms
    // housekeeping tick and the spinners redraw at their own 12.5 a second: bounded by that.
    let software = gui.req(json!({ "cmd": "size" }))?["software"] == true;
    let full_ok = full_on <= full_off + (glow_on / 2).max(3) || (software && full_on <= 2 * 13 + 4);
    ensure!(
        (10..=100).contains(&glow_on) && glow_off == 0 && full_ok,
        "in 2 s while the worker works: {glow_on} glow repaints and {full_on} full frames with the glow on, {glow_off} and {full_off} with it off{} (presents skipped {skipped_on} and {skipped_off}, redraws asked by the window system {asked_on} and {asked_off})",
        if software { " (software renderer)" } else { "" }
    );
    // Reduced motion: a static glow, the same a moment later, near brighter than far, no frames for it.
    gui.req(json!({ "cmd": "motion", "value": "reduced" }))?;
    gui.wait_tree("reduced motion: a static glow", 5, |t| glow_state(t)["moving"] == false)?;
    std::thread::sleep(Duration::from_millis(150));
    let r1 = capture(gui, dir, "glow-reduced-0", id)?;
    let (_, glow_reduced, _) = frames_in(gui, 0.7)?;
    let r2 = capture(gui, dir, "glow-reduced-1", id)?;
    r1.as_modelled(coverage)?;
    ensure!(r1.dots.iter().zip(&r2.dots).all(|(x, y)| (x.x, x.y) == (y.x, y.y) && r1.pixel(x) == r2.pixel(y)), "the reduced-motion glow changed in {} ms", r2.clock - r1.clock);
    let (r_near, r_far) = (r1.lift(0.0..=6.0, base), r1.far_lift(base));
    ensure!(r_near >= 15.0 * coverage && r_far.abs() <= 2.0 && glow_reduced == 0, "reduced motion: +{r_near:.1} near, {r_far:+.1} far, {glow_reduced} glow repaints");
    gui.req(json!({ "cmd": "motion", "value": "on" }))?;
    gui.wait_tree("motion on again", 5, glowing)?;
    // Settings › Working glow off (saved on this machine): normal dots; on again.
    let toggle = |gui: &mut Gui, on: bool| -> Result<()> {
        gui.chord('8', &["alt"])?;
        gui.wait("the Settings view's Working glow row", 10, |t| t.contains("Working glow"))?;
        let at = gui.find("Working glow").context("the Working glow row")?;
        gui.click(at, &[])?;
        let want = format!("\"grid_pulse\": {on}");
        let settings = sb.instance.home.join("settings.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&settings).unwrap_or_default().contains(&want) {
            ensure!(Instant::now() < deadline, "settings.json does not say {want}: {}", std::fs::read_to_string(&settings).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(100));
        }
        gui.chord('1', &["alt"])?;
        Ok(())
    };
    toggle(gui, false)?;
    gui.wait_tree("the setting off: no glow", 5, |t| glow_state(t)["active"] == false && status(t, NAME).as_deref() == Some("working"))?;
    std::thread::sleep(Duration::from_millis(300));
    capture(gui, dir, "glow-setting-off", id)?.normal(normal, "the setting off")?;
    toggle(gui, true)?;
    gui.wait_tree("the setting on again: glowing", 5, glowing)?;
    // Nothing works (an Esc in the agent): the glow fades out, its last ring travels on, then no frames at all.
    let events = sb.instance.sessions_dir.join(id).join("events.jsonl");
    let mut text = std::fs::read_to_string(&events)?;
    text.push_str(&(event("abort", crate::paths::now_ms(), json!({})) + "\n"));
    std::fs::write(&events, text)?;
    let mut fading = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let tree = gui.tree()?;
        fading |= glow_state(&tree)["sources"][0]["on"] == false;
        if status(&tree, NAME).as_deref() == Some("idle") && glow_state(&tree)["busy"] == false && glow_state(&tree)["lit"] == 0 {
            break;
        }
        ensure!(Instant::now() < deadline, "the glow did not end: {} ({:?})", glow_state(&tree), status(&tree, NAME));
        std::thread::sleep(Duration::from_millis(60));
    }
    std::thread::sleep(Duration::from_millis(500));
    // Only the window's housekeeping (the service's statistics in the status bar, about one frame in 2 s).
    let (idle_frames, idle_glow, _) = frames_in(gui, 2.0)?;
    ensure!(idle_frames <= 3 && idle_glow == 0, "{idle_frames} frames ({idle_glow} glow repaints) in 2 s with nothing working");
    capture(gui, dir, "glow-nothing-works", id)?.normal(normal, "nothing working")?;
    Ok(format!(
        "{} dots sampled; a ring {:.1} out: +{a_near:.0} near the worker (up to {peak:.2}x the normal dot), {a_far:+.0} far; {} ms later {:.1} out: centred {ca:.1} then {cb:.1} from the box; as modelled; idle box unlit; in 2 s while working: {glow_on} glow repaints, {full_on} full frames ({glow_off} and {full_off} with the glow off); reduced motion: still, +{r_near:.0} near; normal with effects off and the setting off (saved); fade-out seen: {fading}, then {idle_frames} frames in 2 s",
        a.dots.len(),
        a.front().unwrap_or_default(),
        b.clock.abs_diff(a.clock),
        b.front().unwrap_or_default(),
    ))
}
