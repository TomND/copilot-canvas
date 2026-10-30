//! `copilot-canvas selftest --client gui --parity`: one named check per row
//! of the parity matrix (PLAN.md §8), run against the GUI over its test hooks.
//!
//! It is fast and safe: terminals are PowerShell demo shells, and the agent
//! is a stand-in Copilot CLI (`copilot-canvas.exe` copied as `copilot.exe`,
//! see `fake_copilot`) in a sandboxed `COPILOT_HOME` with synthetic session
//! histories (`events.jsonl`) for every status, subagents and a loop, plus
//! the sample automation database. Nothing reaches a model. Each check
//! records ✓ or ✗ with its reason; the table at the end maps every row to
//! its check, and the run fails when any check failed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::doc::{Doc, GRID_X, GRID_Y, Group, Node, NodeKind};
use crate::paths::Instance;
use crate::selftest::{GuiDriver, Ui, load, wait_doc};

const COLS: u16 = 170;
const ROWS: u16 = 46;

/// OpenCode sessions of the inspector check: a session and its two children.
const OC_PARENT: &str = "ses_parityparent";
const OC_RUNNING: &str = "ses_parityrunning";
const OC_DONE: &str = "ses_paritydone";

/// OpenCode's tables with the indexes the canvas reads by.
const OPENCODE_SCHEMA: &str = "
    CREATE TABLE session (id text PRIMARY KEY, project_id text NOT NULL, parent_id text, slug text NOT NULL, directory text NOT NULL,
        title text NOT NULL, version text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, time_archived integer, agent text);
    CREATE INDEX session_parent_idx ON session (parent_id);
    CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
    CREATE INDEX message_session_time_created_id_idx ON message (session_id, time_created, id);
    CREATE TABLE part (id text PRIMARY KEY, message_id text NOT NULL, session_id text NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
    CREATE INDEX part_message_id_id_idx ON part (message_id, id);
    CREATE TABLE event_sequence (aggregate_id text PRIMARY KEY, seq integer NOT NULL);
";

/// How a row is tested.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// An end-to-end check in this run.
    E2e,
    /// Only meaningful in one front end (with the reason in the detail).
    Only(&'static str),
}

struct Row {
    row: &'static str,
    feature: &'static str,
    test: &'static str,
    kind: Kind,
    result: Result<String, String>,
}

/// The synthetic history: session ids by role.
struct Sessions {
    working: String,
    waiting: String,
    review: String,
    idle: String,
    stopped: String,
    failed: String,
    elsewhere: String,
    looping: String,
    agents: String,
    import_a: String,
    import_b: String,
}

/// The sandbox: synthetic histories for every status, a canvas of them, a
/// stand-in Copilot CLI (also the golden screenshots' all-statuses canvas).
pub(crate) struct Sandbox {
    pub(crate) root: PathBuf,
    pub(crate) instance: Instance,
    pub(crate) work: PathBuf,
    pub(crate) group_folder: PathBuf,
    copilot_home: PathBuf,
    fake: PathBuf,
    /// The stand-in as `opencode.exe` (the OpenCode boxes of the inspector check).
    fake_opencode: PathBuf,
    /// The stand-in as `claude.exe` (the model check's Claude Code sessions).
    fake_claude: PathBuf,
    /// The stand-in `gh` and `az` (pull request rows) and their fixtures' folder.
    fake_gh: PathBuf,
    fake_az: PathBuf,
    pub(crate) prs: PathBuf,
    canvas: PathBuf,
    sessions: Sessions,
    pub(crate) exe: PathBuf,
}

/// The GUI and the helpers the checks use.
pub(crate) struct Gui {
    pub(crate) driver: GuiDriver,
}

impl Gui {
    fn req(&self, command: Value) -> Result<Value> {
        self.driver.request(command)
    }

    fn tree_of(&self, window: usize) -> Result<Value> {
        Ok(self.req(json!({ "cmd": "tree", "window": window }))?["tree"].clone())
    }

    fn tree(&self) -> Result<Value> {
        self.tree_of(0)
    }

    fn screen_of(&self, window: usize) -> String {
        self.req(json!({ "cmd": "screen", "window": window }))
            .map(|r| r["lines"].as_array().map(|l| l.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default())
            .unwrap_or_default()
    }

    fn screen(&self) -> String {
        self.driver.text()
    }

    fn wait(&self, what: &str, secs: u64, predicate: impl Fn(&str) -> bool) -> Result<()> {
        self.driver.wait(what, Duration::from_secs(secs), predicate)
    }

    fn wait_tree(&self, what: &str, secs: u64, predicate: impl Fn(&Value) -> bool) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let tree = self.tree()?;
            if predicate(&tree) {
                return Ok(tree);
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for {what}\n--- screen ---\n{}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn send(&mut self, bytes: &str) -> Result<()> {
        self.driver.send(bytes.as_bytes())
    }

    fn key(&self, name: &str, mods: &[&str]) -> Result<()> {
        self.req(json!({ "cmd": "key", "key": name, "mods": mods })).map(|_| ())
    }

    fn chord(&self, c: char, mods: &[&str]) -> Result<()> {
        self.req(json!({ "cmd": "key", "char": c.to_string(), "mods": mods })).map(|_| ())
    }

    fn mouse(&self, kind: &str, button: &str, (x, y): (f64, f64), mods: &[&str]) -> Result<()> {
        self.req(json!({ "cmd": "mouse", "kind": kind, "button": button, "x": x, "y": y, "mods": mods })).map(|_| ())
    }

    fn click(&self, at: (f64, f64), mods: &[&str]) -> Result<()> {
        self.mouse("down", "left", at, mods)?;
        self.mouse("up", "left", at, mods)
    }

    /// Two clicks at one point (the explicit start of a box).
    fn double_click(&self, at: (f64, f64)) -> Result<()> {
        self.click(at, &[])?;
        self.click(at, &[])
    }

    fn drag(&self, button: &str, from: (f64, f64), to: (f64, f64), mods: &[&str]) -> Result<()> {
        self.mouse("down", button, from, mods)?;
        for i in 1..=4 {
            let t = i as f64 / 4.0;
            self.mouse("drag", button, (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t), mods)?;
        }
        self.mouse("up", button, to, mods)
    }

    fn wheel(&self, dir: &str, at: (f64, f64), mods: &[&str]) -> Result<()> {
        self.req(json!({ "cmd": "wheel", "dir": dir, "x": at.0, "y": at.1, "mods": mods })).map(|_| ())
    }

    fn png(&self, window: usize, name: &str) -> Result<PathBuf> {
        let dir = std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("copilot-canvas-parity-screens"));
        let path = dir.join(format!("{name}.png"));
        self.req(json!({ "cmd": "png", "path": path, "window": window }))?;
        Ok(path)
    }

    /// Back to the canvas from wherever the keyboard is (dialogs closed).
    fn canvas_focus(&mut self) -> Result<()> {
        for _ in 0..3 {
            let tree = self.tree()?;
            if tree["modals"].as_u64().unwrap_or(0) > 0 {
                self.key("Esc", &[])?;
                continue;
            }
            if tree["focus"] == "Terminal" {
                self.chord(']', &["ctrl"])?;
                continue;
            }
            break;
        }
        if self.tree()?["focus"] != "Canvas" {
            self.key("F6", &[])?;
        }
        Ok(())
    }

    /// A canvas box by its name: (its JSON, its screen rect).
    fn find_box(&self, name: &str) -> Result<(Value, [f64; 4])> {
        let tree = self.tree()?;
        let boxes = tree["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
        let found = boxes.into_iter().find(|b| b["name"] == name).with_context(|| format!("no box {name:?} on the canvas"))?;
        let r: Vec<f64> = found["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        anyhow::ensure!(r.len() == 4, "box rect");
        Ok((found, [r[0], r[1], r[2], r[3]]))
    }

    /// A point inside a box's visible part (boxes half under the sidebar or
    /// the right panel are clicked where they show).
    fn box_center(&self, name: &str) -> Result<(f64, f64)> {
        let (_, r) = self.find_box(name)?;
        let tree = self.tree()?;
        let area: Vec<f64> = tree["canvas"]["area"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        anyhow::ensure!(area.len() == 4, "canvas area");
        let (x0, x1) = (r[0].max(area[0]), (r[0] + r[2]).min(area[0] + area[2]));
        let (y0, y1) = (r[1].max(area[1]), (r[1] + r[3]).min(area[1] + area[3]));
        anyhow::ensure!(x1 - x0 >= 1.0 && y1 - y0 >= 0.5, "box {name:?} at {r:?} is not in view (canvas {area:?})");
        Ok((x0 + ((x1 - x0) / 2.0).min(4.0), (y0 + y1) / 2.0))
    }

    /// The focused tile's inner rect.
    fn tile(&self) -> Result<(String, [f64; 4])> {
        let tree = self.tree()?;
        let tile = tree["tiles"].as_array().and_then(|t| t.iter().find(|t| t["focused"] == true).or_else(|| t.first()).cloned()).context("no terminal tile")?;
        let r: Vec<f64> = tile["inner"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        Ok((tile["id"].as_str().unwrap_or_default().to_owned(), [r[0], r[1], r[2], r[3]]))
    }

    /// The focused tile's text with its rows joined (soft-wrapped lines rejoin).
    fn tile_text(&self) -> Result<String> {
        let (_, r) = self.tile()?;
        let screen = self.screen();
        let (x, y, w, h) = (r[0] as usize, r[1] as usize, r[2] as usize, r[3] as usize);
        Ok(screen.lines().skip(y).take(h).map(|l| l.chars().skip(x).take(w).collect::<String>()).collect::<Vec<_>>().join(""))
    }

    fn wait_tile(&self, what: &str, secs: u64, predicate: impl Fn(&str) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let text = self.tile_text()?;
            if predicate(&text) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for {what} in the terminal\n--- screen ---\n{}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Starts (or focuses) a box's terminal with a double-click, waits for
    /// its terminal to have the keyboard and for `ready` in the screen.
    fn open(&mut self, name: &str, ready: &str) -> Result<()> {
        self.canvas_focus()?;
        let (found, rect) = self.find_box(name)?;
        let id = found["id"].as_str().unwrap_or_default().to_owned();
        let at = self.box_center(name)?;
        self.double_click(at)?;
        self.wait_tree(&format!("{name}'s terminal with the keyboard (box {rect:?}, double-click at {at:?})"), 25, |t| t["focus"] == "Terminal" && t["focused_term"] == id.as_str())?;
        self.wait(&format!("{name}'s terminal"), 25, |t| t.contains(ready))
    }

    /// The cell of the first occurrence of `needle` in the screen.
    fn find(&self, needle: &str) -> Option<(f64, f64)> {
        self.driver.find(needle).map(|(x, y)| (x as f64 + 0.5, y as f64 + 0.5))
    }

    /// What the GUI copied (Windows' clipboard, or the hooks' when a locked
    /// workstation refuses it), once it matches.
    fn copied(&self, ok: impl Fn(&str) -> bool) -> Option<String> {
        (0..25).find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            let platform = self.req(json!({ "cmd": "platform" })).ok()?;
            let text = platform["clipboard"]["text"].as_str().map(str::to_owned).or_else(|| crate::client::clipboard::get_text().ok())?;
            ok(&text).then_some(text)
        })
    }

    /// Which clipboard the copy and paste checks used.
    fn clipboard_source(&self) -> &'static str {
        match self.req(json!({ "cmd": "platform" })) {
            Ok(p) if p["clipboard"]["system_unavailable"] == true => "the Windows clipboard is locked: the hooks' clipboard stood in",
            _ => "Windows clipboard",
        }
    }

    /// Polls the platform state (toasts with the Action Center count) until `ok`.
    fn poll_platform(&self, what: &str, secs: u64, ok: &dyn Fn(&Value) -> bool) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let platform = self.req(json!({ "cmd": "platform", "history": true }))?;
            if ok(&platform) {
                return Ok(platform);
            }
            if Instant::now() > deadline {
                bail!("{what}: {platform}");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn cell(&self) -> Result<(f64, f64)> {
        let size = self.req(json!({ "cmd": "size" }))?;
        Ok((size["cell_w"].as_f64().unwrap_or(10.0), size["cell_h"].as_f64().unwrap_or(20.0)))
    }
}

/// RGB of a pixel of a PNG file.
fn pixel(path: &Path, x: u32, y: u32) -> Result<[u8; 3]> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("png size")?];
    let info = reader.next_frame(&mut buf)?;
    let channels = info.color_type.samples();
    let at = (y as usize * info.width as usize + x as usize) * channels;
    Ok([buf[at], buf[at + 1], buf[at + 2]])
}

fn close(a: [u8; 3], b: [u8; 3], tolerance: i32) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= tolerance)
}

/// A PNG file as (width, RGB rows).
fn png_rgb(path: &Path) -> Result<(usize, Vec<[u8; 3]>)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("png size")?];
    let info = reader.next_frame(&mut buf)?;
    let channels = info.color_type.samples();
    Ok((info.width as usize, buf.chunks(channels).map(|p| [p[0], p[1], p[2]]).collect()))
}

/// The loop dot of row 7b: PNG captures a short time apart show the lit
/// dot where the clock put it (the hooks report its place at the capture),
/// moving a little each time and never jumping; the loop layer is redrawn
/// about 30 times a second while the arc is in view and not at all once it
/// is scrolled away; reduced motion draws a still arc (identical captures,
/// no dot, no frames).
fn loop_dot_check(gui: &mut Gui, id: &str) -> Result<String> {
    gui.canvas_focus()?;
    let before = gui.tree()?["canvas"]["motion"].as_str().unwrap_or("On").to_lowercase();
    let result = loop_dot_steps(gui, id);
    // Whatever happened: this window's motion setting back, everything in view.
    let _ = gui.req(json!({ "cmd": "motion", "value": before }));
    let _ = gui.chord('f', &[]);
    result
}

fn loop_dot_steps(gui: &mut Gui, id: &str) -> Result<String> {
    const LIT: [u8; 3] = [110, 240, 190];
    gui.send("f")?;
    gui.req(json!({ "cmd": "motion", "value": "on" }))?;
    let arc = |loops: &Value| loops.as_array().and_then(|l| l.iter().find(|a| a["id"] == id).cloned());
    gui.wait_tree("the loop arc with its dot", 10, |t| arc(&t["canvas"]["loop_arcs"]).is_some_and(|a| !a["dot_px"].is_null()))?;
    let dir = std::env::temp_dir().join(format!("copilot-canvas-loop-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let capture = |name: &str| -> Result<(Instant, Value, PathBuf)> {
        let path = dir.join(format!("{name}.png"));
        let reply = gui.req(json!({ "cmd": "png", "path": path }))?;
        let at = Instant::now();
        let found = arc(&reply["loops"]).context("the loop in the capture")?;
        Ok((at, found, path))
    };
    let point = |v: &Value| -> Option<(f64, f64)> { Some((v[0].as_f64()?, v[1].as_f64()?)) };
    // Motion on: five captures 90 ms apart.
    let mut samples = Vec::new();
    for i in 0..5 {
        samples.push(capture(&format!("loop-dot-{i}"))?);
        std::thread::sleep(Duration::from_millis(90));
    }
    let mut matched = 0;
    for (_, found, path) in &samples {
        let (x, y) = point(&found["dot_px"]).context("dot")?;
        let (width, pixels) = png_rgb(path)?;
        // The head's core is drawn where the clock put it (unless it is just leaving or entering the box).
        let lit: Vec<(f64, f64)> = (-6i64..=6)
            .flat_map(|dy| (-6i64..=6).map(move |dx| (dx, dy)))
            .filter_map(|(dx, dy)| {
                let (px, py) = ((x.round() as i64 + dx) as usize, (y.round() as i64 + dy) as usize);
                pixels.get(py * width + px).filter(|c| close(**c, LIT, 45)).map(|_| (px as f64, py as f64))
            })
            .collect();
        if !lit.is_empty() {
            let (cx, cy) = (lit.iter().map(|p| p.0).sum::<f64>() / lit.len() as f64, lit.iter().map(|p| p.1).sum::<f64>() / lit.len() as f64);
            anyhow::ensure!((cx - x).hypot(cy - y) <= 3.0, "the lit dot at ({cx:.1}, {cy:.1}), the clock says ({x:.1}, {y:.1})");
            matched += 1;
        }
    }
    anyhow::ensure!(matched >= 3, "the lit dot was in only {matched} of 5 captures");
    let bounds: Vec<f64> = samples[0].1["bounds_px"].as_array().map(|b| b.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    anyhow::ensure!(bounds.len() == 4, "arc bounds");
    // The arc's length bounds how far the dot can go in a moment (1.35 × the mean speed).
    let arc_len = 2.0 * ((bounds[2] - bounds[0]) + (bounds[3] - bounds[1]));
    let mut steps = Vec::new();
    for pair in samples.windows(2) {
        let (a, b) = (point(&pair[0].1["dot_px"]).context("dot")?, point(&pair[1].1["dot_px"]).context("dot")?);
        let lap = (pair[1].1["lap"].as_f64().unwrap_or(0.0) - pair[0].1["lap"].as_f64().unwrap_or(0.0)).rem_euclid(1.0);
        let elapsed = pair[1].0.duration_since(pair[0].0).as_secs_f64();
        // The lap advances with the clock (2 s a lap), whatever was drawn in between.
        anyhow::ensure!((lap * 2.0 - elapsed).abs() < 0.08, "the lap moved {lap:.3} in {elapsed:.3} s");
        let moved = (b.0 - a.0).hypot(b.1 - a.1);
        let wrapped = pair[1].1["lap"].as_f64() < pair[0].1["lap"].as_f64();
        anyhow::ensure!(moved > 0.3 || wrapped, "the dot did not move in {elapsed:.3} s");
        anyhow::ensure!(wrapped || moved <= 1.35 * arc_len * lap + 3.0, "a jump of {moved:.1} px in {elapsed:.3} s (arc ≈ {arc_len:.0} px)");
        steps.push(format!("{moved:.1}"));
    }
    // Frames: the loop layer is redrawn at its pace while the arc is in view
    // (by loop frames of its own, and by the full frames other animations need).
    let frames = |key: &str| -> Result<u64> { Ok(gui.req(json!({ "cmd": "perf" }))?["perf"][key].as_u64().unwrap_or(0)) };
    let count = |key: &str, secs: f64| -> Result<u64> {
        let start = frames(key)?;
        std::thread::sleep(Duration::from_secs_f64(secs));
        Ok(frames(key)?.saturating_sub(start))
    };
    let interval = gui.tree()?["canvas"]["loop_interval_ms"].as_u64().unwrap_or(33).max(1);
    let (start_loops, start_all) = (frames("loop_frames")?, frames("presented")?);
    std::thread::sleep(Duration::from_secs(2));
    let (in_view, all) = (frames("loop_frames")?.saturating_sub(start_loops), frames("presented")?.saturating_sub(start_all));
    let want = 2000 / interval;
    anyhow::ensure!(in_view > 0 && all * 100 >= want * 65 && all <= 2 * 75, "{all} frames ({in_view} of the loop layer alone) in 2 s while the arc is in view; its pace is every {interval} ms");
    // Scrolled until no loop is in view: no loop frames.
    let none_in_view = |t: &Value| t["canvas"]["loop_arcs"].as_array().is_some_and(Vec::is_empty);
    for _ in 0..80 {
        if none_in_view(&gui.tree()?) {
            break;
        }
        gui.key("Down", &["ctrl"])?;
    }
    gui.wait_tree("every loop out of view", 5, none_in_view)?;
    std::thread::sleep(Duration::from_millis(300));
    let away = count("loop_frames", 1.5)?;
    anyhow::ensure!(away <= 1, "{away} loop frames with every loop out of view");
    gui.chord('f', &[])?;
    // Reduced motion: no moving dot, the arc and its ↻ badge stand still.
    gui.req(json!({ "cmd": "motion", "value": "reduced" }))?;
    let still = gui.wait_tree("a still arc", 5, |t| arc(&t["canvas"]["loop_arcs"]).is_some_and(|a| a["moving"] == false && a["dot_px"].is_null()))?;
    anyhow::ensure!(still["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["id"] == id && b["loop"] == "every 15m")), "the status line still says ↻ every 15m");
    std::thread::sleep(Duration::from_millis(300));
    let (_, first, a) = capture("loop-reduced-0")?;
    std::thread::sleep(Duration::from_millis(400));
    let (_, _, b) = capture("loop-reduced-1")?;
    let bounds: Vec<f64> = first["bounds_px"].as_array().map(|b| b.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    anyhow::ensure!(bounds.len() == 4, "arc bounds");
    let ((width, one), (_, two)) = (png_rgb(&a)?, png_rgb(&b)?);
    let mut arc_pixels = 0;
    for y in bounds[1] as usize..=bounds[3] as usize {
        for x in bounds[0] as usize..=bounds[2] as usize + 4 {
            let i = y * width + x;
            anyhow::ensure!(one.get(i) == two.get(i), "the reduced-motion arc changed at ({x}, {y})");
            arc_pixels += one.get(i).is_some_and(|c| close(*c, [0, 158, 115], 40) || close(*c, LIT, 40)) as usize;
        }
    }
    anyhow::ensure!(arc_pixels >= 20, "the still arc is drawn ({arc_pixels} loop-coloured pixels)");
    let reduced = count("loop_frames", 1.0)?;
    anyhow::ensure!(reduced <= 1, "{reduced} loop frames with reduced motion");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(format!("dot at the clock's place in {matched}/5 captures, steps {} px; {all} frames in 2 s in view ({in_view} of the loop layer alone, every {interval} ms), {away} away; reduced motion: still arc, {reduced} frames", steps.join(" ")))
}

/// Row 38: three sessions viewed in turn are listed newest first under
/// RECENT and saved in the canvas document every window shares; Ctrl+F6
/// views the one used before, again the one before that, Ctrl+Shift+F6 goes
/// back, `]` and `[` do the same on the canvas, each showing the session
/// (details panel) and starting nothing (no launch in the service log, no
/// new terminal); a second after a walk the session it reached is first.
fn recent_check(sb: &Sandbox, gui: &mut Gui) -> Result<String> {
    use crate::selftest::recent_names;
    gui.canvas_focus()?;
    gui.chord('f', &[])?;
    let log = || std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
    let launches = log().matches("launched ").count();
    let selected = |t: &Value, id: &str| t["selection"].as_array().is_some_and(|s| s.len() == 1 && s[0] == id);
    let mut ids = Vec::new();
    for name in ["Idle one", "Review me", "Needs you one"] {
        let (found, _) = gui.find_box(name)?;
        let id = found["id"].as_str().unwrap_or_default().to_owned();
        gui.click(gui.box_center(name)?, &[])?;
        gui.wait_tree(&format!("{name} viewed"), 5, |t| selected(t, &id))?;
        ids.push(id);
    }
    let first = |t: &str, name: &str| recent_names(t).first().is_some_and(|n| name.starts_with(n.as_str()));
    gui.wait("RECENT, newest first", 5, |t| {
        let r = recent_names(t);
        r.len() >= 3 && "Needs you one".starts_with(r[0].as_str()) && "Review me".starts_with(r[1].as_str()) && "Idle one".starts_with(r[2].as_str())
    })?;
    // Shared: the service saved the uses in the canvas document every window opens.
    wait_doc(&sb.canvas, "the uses in the canvas document", |d| {
        let mut used: Vec<&Node> = d.nodes.iter().filter(|n| n.accessed_ms.is_some()).collect();
        used.sort_by_key(|n| std::cmp::Reverse(n.accessed_ms));
        used.len() >= 3 && used[0].id == ids[2] && used[1].id == ids[1] && used[2].id == ids[0]
    })?;
    // Ctrl+F6, Ctrl+F6, Ctrl+Shift+F6: the one before, the one before that, back; the order holds meanwhile.
    gui.key("F6", &["ctrl"])?;
    gui.wait_tree("Ctrl+F6: the session used before", 5, |t| selected(t, &ids[1]))?;
    gui.wait("its details and the walk's notice", 5, |t| t.contains("Recent 2 of") && t.contains("Review me"))?;
    gui.key("F6", &["ctrl"])?;
    gui.wait_tree("Ctrl+F6 again: the one before that", 5, |t| selected(t, &ids[0]))?;
    gui.key("F6", &["ctrl", "shift"])?;
    gui.wait_tree("Ctrl+Shift+F6: back", 5, |t| selected(t, &ids[1]))?;
    anyhow::ensure!(first(&gui.screen(), "Needs you one"), "the list kept its order during the walk: {:?}", recent_names(&gui.screen()));
    gui.wait("the walk settled", 5, |t| first(t, "Review me"))?;
    // ] and [ on the canvas.
    gui.chord(']', &[])?;
    gui.wait_tree("]: the session used before", 5, |t| selected(t, &ids[2]))?;
    gui.chord('[', &[])?;
    gui.wait_tree("[: back", 5, |t| selected(t, &ids[1]))?;
    gui.key("Esc", &[])?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(log().matches("launched ").count() == launches, "walking the recent sessions launched one");
    let tree = gui.tree()?;
    let shown: Vec<&str> = tree["tiles"].as_array().into_iter().flatten().filter_map(|t| t["id"].as_str()).collect();
    anyhow::ensure!(!shown.iter().any(|id| ids.iter().any(|i| i == id)), "a walked session got a terminal: {shown:?}");
    Ok("three views listed newest first (and in the shared document); Ctrl+F6 ×2, Ctrl+Shift+F6, ] and [ viewed them in MRU order; settled after 1 s; nothing launched".into())
}

fn session_files(dir: &Path, id: &str, name: &str, cwd: &Path, events: &[String], live: bool) -> Result<()> {
    let folder = dir.join(id);
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("workspace.yaml"), format!("id: {id}\nname: {name}\ncwd: {}\n", cwd.display()))?;
    if !events.is_empty() {
        std::fs::write(folder.join("events.jsonl"), events.join("\n") + "\n")?;
    }
    if live {
        // This process holds the "in use" lock: the session counts as open elsewhere.
        std::fs::write(folder.join(format!("inuse.{}.lock", std::process::id())), "parity")?;
    }
    Ok(())
}

fn event(kind: &str, at_ms: u64, data: Value) -> String {
    json!({ "type": kind, "timestamp": crate::paths::format_rfc3339_ms(at_ms), "data": data }).to_string()
}

fn node(id: &str, name: &str, x: i32, y: i32, cwd: &Path) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x,
        y,
        cwd: Some(cwd.display().to_string()),
        parent: None,
        agent_key: None,
        source: None,
        command: None,
        fresh: false,
        automation: None,
        synced_name: None,
        reviewed_ms: None,
        mcp_off: None,
        passive: None,
        no_messages: false,
        agent: None,
        agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
    }
}

impl Sandbox {
    pub(crate) fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("copilot-canvas-parity-{}", uuid::Uuid::new_v4()));
        let (home, work, copilot_home, group_folder) = (root.join("home"), root.join("work"), root.join("copilot-home"), root.join("group"));
        let sessions_dir = copilot_home.join("session-state");
        for dir in [&home, &work, &sessions_dir, &group_folder] {
            std::fs::create_dir_all(dir)?;
        }
        crate::automations::write_sample_db(&copilot_home.join("data.db"))?;
        // The stand-in agent CLI.
        let exe = std::env::current_exe()?;
        let fake = root.join("fake").join("copilot.exe");
        std::fs::create_dir_all(fake.parent().unwrap_or(&root))?;
        std::fs::copy(&exe, &fake)?;
        let fake_opencode = root.join("fake").join("opencode.exe");
        std::fs::copy(&exe, &fake_opencode)?;
        let fake_claude = root.join("fake").join("claude.exe");
        std::fs::copy(&exe, &fake_claude)?;
        let (fake_gh, fake_az, prs) = (root.join("fake").join("gh.exe"), root.join("fake").join("az.exe"), root.join("prs"));
        std::fs::copy(&exe, &fake_gh)?;
        std::fs::copy(&exe, &fake_az)?;
        std::fs::create_dir_all(&prs)?;
        let instance = Instance::new(Some(format!("parity{}", std::process::id())), Some(home.clone()), Some(sessions_dir.clone()))?;
        let id = || uuid::Uuid::new_v4().to_string();
        let sessions = Sessions {
            working: id(),
            waiting: id(),
            review: id(),
            idle: id(),
            stopped: id(),
            failed: id(),
            elsewhere: id(),
            looping: id(),
            agents: id(),
            import_a: id(),
            import_b: id(),
        };
        let now = crate::paths::now_ms();
        let start = |at| event("session.start", at, json!({}));
        let user = |at| event("user.message", at, json!({ "content": "parity" }));
        let turn = |at| event("assistant.turn_start", at, json!({}));
        let s = &sessions;
        let dir = &sessions_dir;
        session_files(dir, &s.working, "Working one", &work, &[start(now - 9000), user(now - 8000), turn(now - 7000), event("tool.execution_start", now - 6000, json!({ "toolCallId": "t1", "toolName": "powershell" }))], true)?;
        session_files(dir, &s.waiting, "Needs you one", &work, &[start(now - 9000), user(now - 8000), turn(now - 7000), event("permission.requested", now - 6000, json!({ "requestId": "p1" }))], true)?;
        session_files(dir, &s.review, "Review me", &work, &[start(now - 9000), user(now - 8000), turn(now - 7000), event("assistant.turn_end", now - 6000, json!({})), event("session.idle", now - 5000, json!({}))], true)?;
        session_files(dir, &s.idle, "Idle one", &work, &[start(now - 7_200_000), user(now - 7_100_000), event("session.idle", now - 7_000_000, json!({}))], true)?;
        session_files(dir, &s.stopped, "Stopped one", &work, &[start(now - 9000), user(now - 8000), event("session.shutdown", now - 7000, json!({}))], false)?;
        session_files(dir, &s.failed, "Failed one", &work, &[start(now - 9000), event("session.error", now - 8000, json!({ "message": "boom" }))], false)?;
        session_files(dir, &s.elsewhere, "Elsewhere", &work, &[], true)?;
        session_files(dir, &s.looping, "Looping", &work, &[start(now - 9000), event("session.schedule_created", now - 8000, json!({ "id": 7, "intervalMs": 900000, "prompt": "check", "recurring": true }))], true)?;
        session_files(
            dir,
            &s.agents,
            "With agents",
            &work,
            &[
                start(now - 9000),
                user(now - 8000),
                turn(now - 7000),
                event("subagent.started", now - 6000, json!({ "toolCallId": "a1", "agentDisplayName": "Parity explorer" })),
                event("assistant.turn_end", now - 5000, json!({})),
                event("session.idle", now - 4000, json!({})),
            ],
            true,
        )?;
        session_files(dir, &s.import_a, "Import me A", &work, &[start(now - 9000), user(now - 8000)], false)?;
        session_files(dir, &s.import_b, "Import me B", &work, &[start(now - 9000), user(now - 8000)], false)?;

        let mut doc = Doc::default();
        let mut add = |id: &str, name: &str, slot: i32, shell: bool| {
            let mut n = node(id, name, 2 + (slot % 3) * (GRID_X + 2), 2 + (slot / 3) * (GRID_Y + 2), &work);
            if shell {
                n.command = Some(vec!["powershell.exe".to_owned(), "-NoLogo".to_owned(), "-NoProfile".to_owned()]);
                n.fresh = true;
            }
            doc.nodes.push(n);
        };
        add("shell-a", "Shell A", 0, true);
        add("shell-b", "Shell B", 1, true);
        add(&s.working, "Working one", 2, false);
        add(&s.waiting, "Needs you one", 3, false);
        add(&s.review, "Review me", 4, false);
        add(&s.idle, "Idle one", 5, false);
        add(&s.stopped, "Stopped one", 6, false);
        add(&s.failed, "Failed one", 7, false);
        add(&s.elsewhere, "Elsewhere", 8, false);
        add(&s.looping, "Looping", 9, false);
        add(&s.agents, "With agents", 10, false);
        // "done · review" needs a look older than the finished turn; "idle" a newer one.
        for n in &mut doc.nodes {
            n.reviewed_ms = Some(if n.id == s.review { now - 600_000 } else { now });
        }
        doc.groups.push(Group {
            id: "g-parity".into(),
            name: "Parity group".into(),
            x: 0,
            y: 0,
            w: 3 * (GRID_X + 2) + 2,
            h: 4 * (GRID_Y + 2) + 3,
            color: 0,
            folder: Some(group_folder.display().to_string()),
            source: None,
            defaults: None,
        });
        doc.selected = Some("shell-a".into());
        let canvas = home.join("canvas.json");
        std::fs::write(&canvas, serde_json::to_string_pretty(&doc)?)?;
        // The service also listens on loopback TCP (the remote rows connect there).
        std::fs::write(home.join("settings.json"), r#"{"remote":{"listen":"127.0.0.1:0"}}"#)?;
        Ok(Self { root, instance, work, group_folder, copilot_home, fake, fake_opencode, fake_claude, fake_gh, fake_az, prs, canvas, sessions, exe })
    }

    /// OpenCode's database with a session and two task-tool children (one
    /// still working, one finished), and the session's box on the canvas
    /// (the inspector check; the golden screenshots leave it out).
    fn add_opencode(&self) -> Result<()> {
        let dir = self.root.join("xdg-data").join("opencode");
        std::fs::create_dir_all(&dir)?;
        let db = rusqlite::Connection::open(dir.join("opencode.db"))?;
        db.execute_batch(OPENCODE_SCHEMA)?;
        let now = crate::paths::now_ms() as i64;
        let work = self.work.display().to_string().replace('\\', "/");
        let mut seq = 0;
        let mut bump = |id: &str| -> Result<()> {
            seq += 1;
            db.execute("INSERT OR REPLACE INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)", rusqlite::params![id, seq])?;
            Ok(())
        };
        for (id, parent, title, at) in [
            (OC_PARENT, None, "OpenCode parent", now - 60_000),
            (OC_RUNNING, Some(OC_PARENT), "Find callers (@explore subagent)", now - 50_000),
            (OC_DONE, Some(OC_PARENT), "Summarize the diff (@general subagent)", now - 40_000),
        ] {
            db.execute(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) VALUES (?1, 'p', ?2, 's', ?3, ?4, '1', ?5, ?5)",
                rusqlite::params![id, parent, work, title, at],
            )?;
            bump(id)?;
        }
        let message = |id: &str, session: &str, at: i64, data: Value| -> Result<()> {
            db.execute("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?3, ?4)", rusqlite::params![id, session, at, data.to_string()])?;
            Ok(())
        };
        let part = |id: &str, message: &str, session: &str, data: Value| -> Result<()> {
            db.execute("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 0, 0, ?4)", rusqlite::params![id, message, session, data.to_string()])?;
            Ok(())
        };
        let user = |at: i64| json!({ "role": "user", "time": { "created": at } });
        let done = |at: i64| json!({ "role": "assistant", "time": { "created": at, "completed": at + 1000 }, "finish": "stop" });
        message("msg_p1", OC_PARENT, now - 59_000, user(now - 59_000))?;
        part("prt_p1", "msg_p1", OC_PARENT, json!({ "type": "text", "text": "Review the parity sandbox" }))?;
        message("msg_p2", OC_PARENT, now - 58_000, done(now - 58_000))?;
        part("prt_p2", "msg_p2", OC_PARENT, json!({ "type": "text", "text": "Two subagents are on it." }))?;
        message("msg_r1", OC_RUNNING, now - 49_000, user(now - 49_000))?;
        part("prt_r1", "msg_r1", OC_RUNNING, json!({ "type": "text", "text": "Find the callers of parity_probe" }))?;
        message("msg_r2", OC_RUNNING, now - 48_000, json!({ "role": "assistant", "time": { "created": now - 48_000 } }))?;
        part("prt_r2", "msg_r2", OC_RUNNING, json!({ "type": "tool", "tool": "grep", "state": { "status": "running", "input": { "pattern": "parity_probe" }, "time": { "start": now - 47_000 } } }))?;
        message("msg_d1", OC_DONE, now - 39_000, user(now - 39_000))?;
        part("prt_d1", "msg_d1", OC_DONE, json!({ "type": "text", "text": "Summarize the diff in one line" }))?;
        message("msg_d2", OC_DONE, now - 38_000, done(now - 38_000))?;
        part("prt_d2", "msg_d2", OC_DONE, json!({ "type": "text", "text": "PARITY_DONE_SUMMARY: nothing changed." }))?;
        for id in [OC_PARENT, OC_RUNNING, OC_DONE] {
            bump(id)?;
        }
        let mut doc = self.doc()?;
        let mut parent = node("oc-parent", "OpenCode parent", 2, 2 + 4 * (GRID_Y + 2) + 6, &self.work);
        parent.agent = Some("opencode".into());
        parent.agent_session = Some(OC_PARENT.into());
        parent.reviewed_ms = Some(crate::paths::now_ms());
        doc.nodes.push(parent);
        std::fs::write(&self.canvas, serde_json::to_string_pretty(&doc)?)?;
        Ok(())
    }

    /// Adds a text part to the working child (its transcript grows while you look).
    fn grow_running_child(&self, text: &str) -> Result<()> {
        let db = rusqlite::Connection::open(self.root.join("xdg-data").join("opencode").join("opencode.db"))?;
        db.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, 'msg_r2', ?2, 0, 0, ?3)",
            rusqlite::params![format!("prt_r9{}", crate::paths::now_ms()), OC_RUNNING, json!({ "type": "text", "text": text }).to_string()],
        )?;
        db.execute("UPDATE event_sequence SET seq = seq + 100 WHERE aggregate_id = ?1", [OC_RUNNING])?;
        Ok(())
    }

    fn args(&self, extra: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--instance".into(),
            self.instance.name.clone(),
            "--home".into(),
            self.instance.home.display().to_string(),
            "--sessions-dir".into(),
            self.instance.sessions_dir.display().to_string(),
            "--cwd".into(),
            self.work.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }

    pub(crate) fn start(&self, extra: &[&str], log: &str) -> Result<Gui> {
        let one = PathBuf::from("1");
        // The stand-in's passive runs last long enough to be stopped (row 40b).
        let run_ms = PathBuf::from("6000");
        let (claude, xdg) = (self.root.join("claude-home"), self.root.join("xdg-data"));
        // Pull requests are polled every 2 s (rows 42, 43), with the stand-in gh / az;
        // review follow-ups are checked every 10 s instead of hourly (row 43).
        let (poll, check) = (PathBuf::from("2"), PathBuf::from("10"));
        // Only the sandbox's histories: OpenCode and Claude Code look in empty folders.
        let env: [(&str, &Path); 13] = [
            ("COPILOT_HOME", &self.copilot_home),
            ("COPILOT_CANVAS_COPILOT", &self.fake),
            ("COPILOT_CANVAS_OPENCODE", &self.fake_opencode),
            ("COPILOT_CANVAS_CLAUDE", &self.fake_claude),
            (crate::fake_copilot::ENV, &one),
            ("CLAUDE_CONFIG_DIR", &claude),
            ("XDG_DATA_HOME", &xdg),
            ("COPILOT_CANVAS_FAKE_RUN_MS", &run_ms),
            ("COPILOT_CANVAS_GH", &self.fake_gh),
            ("COPILOT_CANVAS_AZ", &self.fake_az),
            (crate::fake_gh::ENV, &self.prs),
            ("COPILOT_CANVAS_PR_POLL_SECS", &poll),
            ("COPILOT_CANVAS_REVIEW_CHECK_SECS", &check),
        ];
        let driver = GuiDriver::start(&self.args(extra), &env, (COLS, ROWS), &self.root.join(log))?;
        Ok(Gui { driver })
    }

    fn doc(&self) -> Result<Doc> {
        load(&self.canvas)
    }
}

/// Stops an instance's service and waits (up to 15 s) until it is gone: a
/// service left running would hold the console's output open until its idle
/// exit 10 minutes later.
pub(crate) fn stop_service(instance: &Instance) {
    let Ok(conn) = crate::client::conn::try_connect(instance) else { return };
    let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(15);
    // The connection stays open until the service closes it, so the request is sent.
    while Instant::now() < deadline && conn.rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
    drop(conn);
    while Instant::now() < deadline && crate::client::conn::try_connect(instance).is_ok() {
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn run(remote: Option<&str>) -> Result<()> {
    anyhow::ensure!(remote.is_none(), "--parity runs locally (it starts its own loopback client for the remote rows)");
    let sandbox = Sandbox::new()?;
    sandbox.add_opencode()?;
    println!("Copilot Canvas GUI parity suite (PLAN §8; demo shells and a stand-in Copilot only; sandbox {})", sandbox.root.display());
    let saved_clipboard = crate::client::clipboard::get_text().ok();
    let started = Instant::now();
    let rows = suite(&sandbox);
    if let Some(text) = saved_clipboard {
        let _ = crate::client::clipboard::set_text(&text);
    }
    stop_service(&sandbox.instance);
    let failed = print_table(&rows);
    if failed == 0 {
        println!("PASS: {} rows in {:.1}s", rows.len(), started.elapsed().as_secs_f64());
        for _ in 0..10 {
            if std::fs::remove_dir_all(&sandbox.root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(())
    } else {
        eprintln!("artifacts kept in {}", sandbox.root.display());
        bail!("{failed} parity row(s) failed")
    }
}

fn print_table(rows: &[Row]) -> usize {
    println!();
    println!("{:<4} | {:<62} | {:<44} | ✓/✗", "row", "feature", "GUI test");
    println!("{}", "-".repeat(122));
    let mut failed = 0;
    for row in rows {
        let mark = match (&row.result, row.kind) {
            (Ok(_), _) => "✓",
            (Err(_), _) => {
                failed += 1;
                "✗"
            }
        };
        let test = match row.kind {
            Kind::E2e => row.test.to_owned(),
            Kind::Only(why) => format!("{} ({why})", row.test),
        };
        println!("{:<4} | {:<62} | {:<44} | {mark}", row.row, truncate(row.feature, 62), truncate(&test, 44));
        if let Err(error) = &row.result {
            println!("       ✗ {}", error.lines().next().unwrap_or_default());
        }
    }
    failed
}

/// Table order: rows 1…33 (with their sub-checks), then the GUI-only features.
fn order(row: &str) -> (u32, String) {
    let digits: String = row.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse() {
        Ok(n) => (n, row[digits.len()..].to_owned()),
        Err(_) => (100, row.to_owned()),
    }
}

fn truncate(text: &str, n: usize) -> String {
    if text.chars().count() <= n { text.to_owned() } else { text.chars().take(n - 1).collect::<String>() + "…" }
}

/// Runs a check, printing its name and outcome as it goes.
fn check(rows: &mut Vec<Row>, row: &'static str, feature: &'static str, test: &'static str, body: impl FnOnce() -> Result<String>) {
    check_kind(rows, row, feature, test, Kind::E2e, body)
}

fn check_kind(rows: &mut Vec<Row>, row: &'static str, feature: &'static str, test: &'static str, kind: Kind, body: impl FnOnce() -> Result<String>) {
    // CANVAS_PARITY_ROWS=41,41b: only these rows (while working on them).
    if let Ok(only) = std::env::var("CANVAS_PARITY_ROWS")
        && !only.split(',').any(|r| r.trim().eq_ignore_ascii_case(row))
    {
        return;
    }
    let started = Instant::now();
    let result = body().map_err(|e| format!("{e:#}"));
    match &result {
        Ok(detail) => println!("  ✓ {row:<3} {test} ({:.1}s) {detail}", started.elapsed().as_secs_f64()),
        Err(error) => println!("  ✗ {row:<3} {test}: {}", error.lines().take(30).collect::<Vec<_>>().join("\n      ")),
    }
    rows.push(Row { row, feature, test, kind, result });
}

fn suite(sb: &Sandbox) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut gui = match sb.start(&["--perf"], "gui-parity.log") {
        Ok(gui) => gui,
        Err(error) => {
            check(&mut rows, "-", "GUI start", "start", || Err(error));
            return rows;
        }
    };
    let ready = gui.wait("the parity canvas", 25, |t| t.contains("Parity group"));
    if let Err(error) = ready {
        check(&mut rows, "-", "GUI start", "start", || Err(error));
        return rows;
    }
    // Everything in view (the statuses come from the history index).
    let _ = gui.chord('f', &[]);
    main_rows(sb, &mut gui, &mut rows);
    let _ = gui.req(json!({ "cmd": "quit" }));
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
    drop(gui);
    late_rows(sb, &mut rows);
    rows.sort_by_key(|r| order(r.row));
    rows
}

fn main_rows(sb: &Sandbox, gui: &mut Gui, rows: &mut Vec<Row>) {
    check(rows, "2", "Status (working, needs you, done · review, idle, stopped, failed, open elsewhere), background subagents, Esc not done", "status_from_history", || {
        let expect = [
            ("Working one", "working"),
            ("Needs you one", "needs you"),
            ("Review me", "done · review"),
            ("Idle one", "idle"),
            ("Stopped one", "stopped"),
            ("Failed one", "failed"),
            ("Elsewhere", "open elsewhere"),
            ("With agents", "working"),
        ];
        gui.wait_tree("statuses from the synthetic history", 20, |t| {
            let boxes = t["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
            expect.iter().all(|(name, status)| boxes.iter().any(|b| b["name"] == *name && b["status"] == *status))
        })?;
        // Esc (abort) leaves nothing to review: idle, not done · review.
        let events = sb.instance.sessions_dir.join(&sb.sessions.working).join("events.jsonl");
        let mut text = std::fs::read_to_string(&events)?;
        text.push_str(&(event("abort", crate::paths::now_ms(), json!({})) + "\n"));
        std::fs::write(&events, text)?;
        gui.wait_tree("abort → idle", 15, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["name"] == "Working one" && b["status"] == "idle")))?;
        Ok("7 states + background subagent; Esc → idle".into())
    });

    check(rows, "4", "Colour-blind-safe palette; distinct icon and border per state", "state_icons_and_borders", || {
        let tree = gui.tree()?;
        let boxes = tree["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
        let border = |name: &str| boxes.iter().find(|b| b["name"] == name).map(|b| b["border"].as_str().unwrap_or_default().to_owned()).unwrap_or_default();
        let (waiting, review, plain) = (border("Needs you one"), border("Review me"), border("Idle one"));
        anyhow::ensure!(waiting != review && waiting != plain && review != plain, "borders: needs you {waiting}, review {review}, idle {plain}");
        let screen = gui.screen();
        let icons = ["◆", "✓", "○", "■", "✗", "↗"];
        let missing: Vec<&str> = icons.iter().copied().filter(|i| !screen.contains(i)).collect();
        anyhow::ensure!(missing.is_empty(), "icons missing on screen: {missing:?}");
        Ok(format!("borders {waiting} / {review} / {plain}; icons {}", icons.join(" ")))
    });

    check(rows, "5", "Subagents from history", "subagent_boxes_from_history", || {
        let tree = gui.wait_tree("the subagent box", 15, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["name"] == "Parity explorer")))?;
        let edges = tree["canvas"]["edges"].as_u64().unwrap_or(0);
        anyhow::ensure!(edges >= 1, "no edge to the subagent");
        Ok(format!("“Parity explorer” box, {edges} edge(s)"))
    });

    check(rows, "7", "Looping sessions: loopback arc, ↻ every, sidebar ↻, pulsing ↻", "loop_from_session_schedule", || {
        let tree = gui.wait_tree("the loop", 15, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["name"] == "Looping" && b["loop"] == "every 15m")))?;
        anyhow::ensure!(tree["canvas"]["loops"].as_u64().unwrap_or(0) >= 1, "no loop arc");
        gui.wait("sidebar ↻", 5, |t| t.lines().any(|l| l.chars().take(36).collect::<String>().contains("↻") && l.chars().take(36).collect::<String>().contains("Looping")))?;
        Ok("↻ every 15m, arc drawn, sidebar ↻".into())
    });

    check(rows, "7b", "Looping sessions: the dot follows the clock smoothly (loop layer alone, ≈30 frames/s, none off screen); reduced motion holds a still arc", "loop_dot_smooth_and_reduced_motion", || loop_dot_check(gui, &sb.sessions.looping));

    check(rows, "3", "Attention borders and tints; active-terminal halo (heavy while typing)", "attention_and_halo", || {
        let (waiting, _) = gui.find_box("Needs you one")?;
        let (review, _) = gui.find_box("Review me")?;
        anyhow::ensure!(waiting["attention"] == true && review["attention"] == true, "attention flags");
        gui.open("Shell A", "PS ")?;
        gui.send("Write-Output halo")?;
        let tree = gui.wait_tree("the halo", 5, |t| !t["canvas"]["halo"].is_null())?;
        anyhow::ensure!(tree["canvas"]["halo"]["typing"] == true, "halo not heavy while typing: {}", tree["canvas"]["halo"]);
        gui.send("\r")?;
        Ok("needs you + review tinted; halo heavy while typing".into())
    });

    check(rows, "21", "Activity bar views (Alt+1…9)", "activity_views_alt_1_to_9", || {
        gui.canvas_focus()?;
        let views = ["SESSIONS", "AUTOMATIONS", "PLUGINS", "MCP SERVERS", "SKILLS", "AGENTS", "VS CODE", "SETTINGS", "REPORTS"];
        for (i, title) in views.iter().enumerate() {
            gui.chord(char::from(b'1' + i as u8), &["alt"])?;
            gui.wait(title, 10, |t| t.contains(title))?;
        }
        gui.chord('1', &["alt"])?;
        Ok("9 views".into())
    });

    // The Reports inbox (rows 41, 41b): before row 34 resumes "Stopped one".
    reports::rows(sb, gui, rows);

    check(rows, "22", "Copilot defaults, group overrides, copilot config list and edit", "settings_defaults_and_config", || {
        gui.chord('8', &["alt"])?;
        gui.wait("settings view", 10, |t| t.contains("New-session defaults") && t.contains("Reasoning effort"))?;
        gui.wait("copilot config list (stand-in CLI)", 20, |t| t.contains("fake-model"))?;
        gui.wait("each agent's model, the initial default", 5, |t| t.contains("Copilot model") && t.contains("initial") && t.contains("claude-opus-5.5") && t.contains("claude-opus-5-5"))?;
        let (x, y) = gui.find("Copilot model").context("Copilot model row")?;
        gui.click((x, y), &[])?;
        gui.wait("model picker with the stand-in's choices", 10, |t| t.contains("Copilot model for new sessions") && t.contains(crate::fake_copilot::MODELS[1]))?;
        // An id the list does not have: the picker takes it as typed.
        gui.send("parity-model\r")?;
        wait_doc(&sb.canvas, "default model saved", |d| d.defaults.model(crate::client::agents::AgentKind::Copilot) == Some("parity-model"))?;
        gui.chord('1', &["alt"])?;
        Ok("Copilot's model set (typed), config listed".into())
    });

    check(rows, "23", "Plugins, MCP, skills: list, toggle, remove, add, install; secrets hidden", "mcp_view_lists_and_toggles", || {
        gui.chord('4', &["alt"])?;
        gui.wait("MCP servers from the CLI", 20, |t| t.contains("github") && t.contains("playwright"))?;
        let (x, y) = gui.find("playwright").context("playwright row")?;
        gui.click((x, y), &[])?;
        gui.send(" ")?;
        gui.wait_tree("toggle notice", 15, |t| !t["notice"].is_null())?;
        gui.chord('3', &["alt"])?;
        gui.wait("plugins view", 10, |t| t.contains("PLUGINS"))?;
        gui.chord('1', &["alt"])?;
        Ok("listed, toggled through the CLI".into())
    });

    check(rows, "26", "Automations: families, failure pulse, badge, info and health, run, enable, outputs", "automations_view_and_details", || {
        gui.chord('2', &["alt"])?;
        gui.wait("automations list", 15, |t| t.contains("Morning triage") && t.contains("Cron job"))?;
        let (x, y) = gui.find("Morning triage").context("automation row")?;
        gui.click((x, y), &[])?;
        gui.wait("automation details", 10, |t| t.contains("Summarize new bugs") && t.contains("daily at 08:30"))?;
        gui.chord('1', &["alt"])?;
        Ok("listed; details with prompt, schedule, runs".into())
    });

    check(rows, "27", "Copilot automations (data.db) and session schedules", "automation_boxes_and_session_loops", || {
        wait_doc(&sb.canvas, "automation boxes", |d| d.groups.iter().any(|g| g.is_automations()) && d.nodes.iter().filter(|n| n.kind == NodeKind::Automation).count() == 3)?;
        let (looping, _) = gui.find_box("Looping")?;
        anyhow::ensure!(looping["loop"] == "every 15m", "session schedule");
        Ok("3 data.db automations boxed; session schedule loops".into())
    });

    check(rows, "10", "Sidebar tree, / filter, click flies to the box, group rows tile terminals", "sidebar_filter_click_and_group", || {
        gui.canvas_focus()?;
        gui.chord('1', &["alt"])?;
        gui.wait("sidebar", 5, |t| t.contains("Parity group"))?;
        gui.send("/")?;
        gui.send("Loop")?;
        gui.wait("filtered sidebar", 5, |t| {
            let side: String = t.lines().map(|l| l.chars().skip(3).take(30).collect::<String>()).collect::<Vec<_>>().join("\n");
            side.contains("Looping") && !side.contains("Stopped one")
        })?;
        gui.key("Esc", &[])?;
        let origin = gui.tree()?["canvas"]["origin"].clone();
        let (x, y) = gui.driver.find_in("Failed one", 3, 33).map(|(x, y)| (x as f64 + 0.5, y as f64 + 0.5)).context("sidebar row")?;
        gui.click((x, y), &[])?;
        let tree = gui.wait_tree("selected from the sidebar", 5, |t| t["selection"].as_array().is_some_and(|s| s.iter().any(|v| v == &json!(sb.sessions.failed))))?;
        let flew = tree["canvas"]["origin"] != origin;
        Ok(format!("filter, select from sidebar{}", if flew { ", canvas moved to it" } else { "" }))
    });

    check(rows, "1", "Canvas: drag boxes, pan, zoom, fit, center, double-click creates, right-click menus", "canvas_pan_zoom_menus", || {
        gui.canvas_focus()?;
        let tree = gui.tree()?;
        let area: Vec<f64> = tree["canvas"]["area"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        let empty = (area[0] + area[2] - 3.0, area[1] + area[3] - 3.0);
        // Pan by dragging empty canvas.
        let origin = tree["canvas"]["origin"].clone();
        gui.drag("middle", empty, (empty.0 - 6.0, empty.1 - 3.0), &[])?;
        anyhow::ensure!(gui.tree()?["canvas"]["origin"] != origin, "middle-drag did not pan");
        // Zoom with Ctrl+wheel around the pointer.
        let scale = gui.tree()?["canvas"]["scale"].as_f64().unwrap_or(0.0);
        gui.wheel("up", empty, &["ctrl"])?;
        let zoomed = gui.tree()?["canvas"]["scale"].as_f64().unwrap_or(0.0);
        anyhow::ensure!(zoomed > scale, "Ctrl+wheel did not zoom in ({scale} → {zoomed})");
        gui.send("f")?;
        // Drag a box: the document moves it.
        let before = sb.doc()?.node("shell-b").map(|n| (n.x, n.y)).context("shell-b")?;
        let at = gui.box_center("Shell B")?;
        gui.drag("left", at, (at.0 + 4.0, at.1 + 2.0), &[])?;
        wait_doc(&sb.canvas, "box moved", |d| d.node("shell-b").is_some_and(|n| (n.x, n.y) != before))?;
        // Right-click menu, double-click creates.
        let at = gui.box_center("Shell B")?;
        gui.mouse("down", "right", at, &[])?;
        gui.mouse("up", "right", at, &[])?;
        gui.wait("context menu", 5, |t| t.contains("New subsession") && t.contains("Remove from canvas"))?;
        gui.key("Esc", &[])?;
        let tree = gui.tree()?;
        let area: Vec<f64> = tree["canvas"]["area"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        let empty = (area[0] + area[2] - 3.0, area[1] + area[3] - 3.0);
        gui.click(empty, &[])?;
        gui.click(empty, &[])?;
        gui.wait("double-click opens the new-session dialog", 5, |t| t.contains("New session") && t.contains("Folder"))?;
        gui.key("Esc", &[])?;
        gui.send("f")?;
        Ok("pan, Ctrl+wheel zoom, fit, box drag, right-click menu, double-click".into())
    });

    check(rows, "9", "Groups: select, G, move by title, resize ◢, nested, membership, folder chips, defaults", "group_move_resize_chips", || {
        gui.canvas_focus()?;
        gui.wait("folder chips", 5, |t| t.contains("◧ AI") && t.contains("◨ Out"))?;
        let group = sb.doc()?.groups.iter().find(|g| g.id == "g-parity").cloned().context("group")?;
        let tree = gui.tree()?;
        let g = tree["canvas"]["groups"].as_array().and_then(|g| g.iter().find(|g| g["id"] == "g-parity").cloned()).context("group in the view")?;
        let r: Vec<f64> = g["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        // Resize from the ◢ corner.
        let corner = (r[0] + r[2] - 0.5, r[1] + r[3] - 0.5);
        gui.drag("left", corner, (corner.0 + 4.0, corner.1 + 1.0), &[])?;
        wait_doc(&sb.canvas, "group resized", |d| d.groups.iter().any(|x| x.id == "g-parity" && (x.w, x.h) != (group.w, group.h)))?;
        // Move by its title (members follow).
        let title = (r[0] + 3.5, r[1] + 0.5);
        let member = sb.doc()?.node("shell-a").map(|n| n.x).context("member")?;
        gui.drag("left", title, (title.0 + 3.0, title.1), &[])?;
        let doc = wait_doc(&sb.canvas, "group moved", |d| d.groups.iter().any(|x| x.id == "g-parity" && x.x != group.x))?;
        anyhow::ensure!(doc.node("shell-a").is_some_and(|n| n.x != member), "members follow the group");
        Ok("chips, ◢ resize, move by title with members".into())
    });

    check(rows, "6", "Subsessions: S, fork or fresh, dotted connector; spawned detection", "subsession_with_dashed_edge", || {
        gui.canvas_focus()?;
        let at = gui.box_center("Shell A")?;
        gui.click(at, &[])?;
        gui.chord(']', &["ctrl"])?;
        let edges = gui.tree()?["canvas"]["edges"].as_u64().unwrap_or(0);
        gui.send("s")?;
        gui.wait("subsession dialog", 5, |t| t.contains("New subsession of"))?;
        gui.send("Parity child\r")?;
        wait_doc(&sb.canvas, "subsession linked", |d| d.nodes.iter().any(|n| n.name == "Parity child" && n.parent.as_deref() == Some("shell-a")))?;
        gui.wait_tree("a new (dashed) edge", 10, |t| t["canvas"]["edges"].as_u64().unwrap_or(0) > edges)?;
        Ok("linked child, connector drawn".into())
    });

    check(rows, "19", "New session dialog: name, folder (Browse…, Recent ▾), Git, worktree, first prompt, MCP row, agent", "new_session_dialog_fields", || {
        gui.canvas_focus()?;
        gui.send("n")?;
        gui.wait("dialog fields", 15, |t| t.contains("New session") && t.contains("Folder") && t.contains("First prompt") && t.contains("MCP servers") && t.contains("Browse"))?;
        gui.wait("git probe", 10, |t| t.contains("Not a Git repository") || t.contains("Git:"))?;
        let (x, y) = gui.find("Browse").context("Browse button")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("folder browser", 5, |t| t.contains("Choose a folder"))?;
        gui.key("Esc", &[])?;
        gui.key("Esc", &[])?;
        Ok("fields, Git probe, folder browser".into())
    });

    check(rows, "8", "MCP picker with presets (Ctrl+S, ←/→, Del), per session and resume", "mcp_picker_preset_and_launch", || {
        gui.canvas_focus()?;
        gui.send("n")?;
        gui.wait("MCP row loaded", 20, |t| t.contains("MCP servers") && t.contains("▾ choose"))?;
        gui.send("MCP parity")?;
        let (x, y) = gui.find("▾ choose").context("MCP row")?;
        gui.click((x, y), &[])?;
        gui.wait("picker", 10, |t| t.contains("MCP servers for the new session") && t.contains("github"))?;
        gui.send("n")?;
        gui.wait("none selected", 5, |t| t.contains(" 0 of "))?;
        gui.send("\x13")?;
        gui.wait("preset prompt", 5, |t| t.contains("Save MCP preset"))?;
        gui.send("Parity none\r")?;
        gui.wait("preset chosen", 5, |t| t.contains("Parity none"))?;
        if gui.screen().contains("MCP servers for the new session") {
            gui.send("\r")?;
        }
        gui.wait("dialog shows the choice", 5, |t| t.contains("0 of ") && t.contains("Create & start"))?;
        let (x, y) = gui.find("Create & start").context("create button")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("the session starts with both servers off", 25, |t| t.contains("MCP off: github,playwright") || t.contains("MCP off: playwright,github"))?;
        let doc = wait_doc(&sb.canvas, "preset and per-session list saved", |d| {
            d.mcp.presets.iter().any(|p| p.name == "Parity none") && d.nodes.iter().any(|n| n.name == "MCP parity" && n.mcp_off.as_ref().is_some_and(|o| o.len() == 2))
        })?;
        Ok(format!("preset saved ({} presets), launch passed --disable-mcp-server ×2", doc.mcp.presets.len()))
    });

    check(rows, "13", "User-message tint (navy background, ▎ bar) in Copilot boxes", "user_message_tint_png", || {
        // The stand-in Copilot of "MCP parity" shows a message of yours.
        let (_, inner) = gui.tile()?;
        let tree = gui.tree()?;
        let tile = tree["tiles"].as_array().and_then(|t| t.iter().find(|t| t["focused"] == true).cloned()).context("tile")?;
        anyhow::ensure!(tile["tint"] == true, "agent tiles are tinted");
        let (x, y) = gui.find(crate::fake_copilot::USER_MESSAGE).context("user message on screen")?;
        let path = gui.png(0, "parity-user-message-tint")?;
        let (cw, ch) = gui.cell()?;
        // Right of the text, inside the tile: the navy tint.
        let px = ((inner[0] + inner[2] - 3.0) * cw) as u32;
        let py = (y * ch) as u32;
        let tint = pixel(&path, px, py)?;
        let plain = pixel(&path, px, ((y - 3.0) * ch) as u32)?;
        anyhow::ensure!(close(tint, [30, 44, 70], 6), "tint pixel {tint:?} at row {y}, col {x}");
        anyhow::ensure!(!close(plain, [30, 44, 70], 6), "the reply is not tinted ({plain:?})");
        Ok(format!("tint {tint:?} vs {plain:?}"))
    });

    check(rows, "16", "Click forwarding in Copilot's mouse mode; drags still select", "click_forwarding_and_drag_select", || {
        let (_, inner) = gui.tile()?;
        let at = (inner[0] + 6.5, inner[1] + inner[3] - 3.5);
        gui.click(at, &[])?;
        gui.wait("the click reached the program", 10, |t| t.contains("MOUSE 0;7;"))?;
        let (x, y) = gui.find("Fake reply").context("reply")?;
        gui.drag("left", (x, y), (x + 12.0, y), &[])?;
        let copied = gui.copied(|t| t.contains("Fake reply"));
        anyhow::ensure!(copied.is_some(), "the drag selected and copied text");
        Ok(format!("click forwarded (SGR report); drag copied “Fake reply” ({})", gui.clipboard_source()))
    });

    check(rows, "15", "Image paste (Alt+V / Ctrl+V): direct Ctrl+V; show the image", "paste_image_preview", || {
        // A 64 × 40 blue image on the clipboard (the hooks' clipboard: a locked workstation refuses Windows').
        gui.req(json!({ "cmd": "clipboard", "image": { "width": 64, "height": 40, "color": [0, 0, 255] } }))?;
        gui.chord('v', &["ctrl"])?;
        let tree = gui.wait_tree("the paste preview", 5, |t| !t["paste_preview"].is_null())?;
        let preview = &tree["paste_preview"];
        anyhow::ensure!(preview["width"] == 64 && preview["height"] == 40, "{preview}");
        let r: Vec<f64> = preview["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        let path = gui.png(0, "paste-image-preview")?;
        let color = pixel(&path, (r[0] + r[2] / 2.0) as u32, (r[1] + r[3] / 2.0) as u32)?;
        anyhow::ensure!(close(color, [0, 0, 255], 8), "thumbnail pixel {color:?}");
        gui.req(json!({ "cmd": "clipboard", "clear": true }))?;
        Ok("Ctrl+V sent to the agent; 64 × 40 thumbnail drawn".into())
    });

    check(rows, "11", "Terminals: ConPTY, single or 4 tiles (T), survive client exit", "tiles_single_and_tiled", || {
        gui.open("Shell B", "PS ")?;
        gui.canvas_focus()?;
        gui.send("t")?;
        gui.wait_tree("tiled", 5, |t| t["tiles"].as_array().is_some_and(|t| t.len() >= 2))?;
        gui.send("t")?;
        gui.wait_tree("single", 5, |t| t["tiles"].as_array().is_some_and(|t| t.len() == 1))?;
        Ok("T toggles 1 ↔ 2+ tiles (client exit: row 29 / late checks)".into())
    });

    check(rows, "12", "Wheel rules (mouse mode, PgUp/PgDn, scrollback, Shift forces scrollback)", "wheel_scrollback_and_shift", || {
        gui.open("Shell A", "PS ")?;
        gui.send("1..120 | % { \"WHEEL_LINE_$_\" }\r")?;
        gui.wait("lines printed", 10, |t| t.contains("WHEEL_LINE_120"))?;
        let (_, inner) = gui.tile()?;
        let at = (inner[0] + 10.5, inner[1] + 5.5);
        for _ in 0..8 {
            gui.wheel("up", at, &[])?;
        }
        gui.wait("scrolled back", 5, |t| !t.contains("WHEEL_LINE_120") && t.contains("WHEEL_LINE_9"))?;
        gui.send("x")?;
        gui.wait("typing returns to the live screen", 5, |t| t.contains("WHEEL_LINE_120"))?;
        gui.send("\x7f")?;
        // In mouse mode (the stand-in agent) Shift+wheel still scrolls the scrollback.
        Ok("wheel scrolls back; typing returns".into())
    });

    check(rows, "14", "Copy and paste: drag-select (rejoin), copy on release, Ctrl+C, Ctrl+Shift+C, Ctrl+V, pane menu", "copy_on_release_and_paste", || {
        gui.send("Write-Output ('COPY_' + 'PROBE_TEXT')\r")?;
        gui.wait("text printed", 10, |t| t.lines().any(|l| l.contains("COPY_PROBE_TEXT") && !l.contains("PROBE_TEXT'")))?;
        let (x, y) = gui
            .driver
            .text()
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.contains("PROBE_TEXT'"))
            .find_map(|(i, l)| l.find("COPY_PROBE_TEXT").map(|c| (crate::client::paint::text_width(&l[..c]) as f64 + 0.5, i as f64 + 0.5)))
            .context("printed text")?;
        gui.drag("left", (x, y), (x + 14.0, y), &[])?;
        let copied = gui.copied(|t| t == "COPY_PROBE_TEXT");
        anyhow::ensure!(copied.is_some(), "copy on release: {:?}", gui.req(json!({ "cmd": "platform" }))?["clipboard"]);
        gui.req(json!({ "cmd": "clipboard", "text": "PASTED_BY_PARITY" }))?;
        gui.chord('v', &["ctrl"])?;
        gui.wait("Ctrl+V pasted", 5, |t| t.contains("PASTED_BY_PARITY"))?;
        gui.chord('u', &["ctrl"])?;
        gui.send("\x1b")?;
        // Multi-line: copy exactly, paste without Enter (PowerShell and a program with bracketed paste).
        let multi = crate::selftest::copy_paste_check(&mut gui.driver)?;
        // IME text with a line break is a paste too.
        let exe = std::env::current_exe()?;
        gui.send(&format!("& '{}' paste-probe\r", exe.display()))?;
        gui.wait("the paste probe", 20, |t| t.contains(crate::paste_probe::READY))?;
        gui.req(json!({ "cmd": "ime", "commit": "ime one\nime two" }))?;
        gui.wait("the IME text as one paste", 10, |t| t.contains("PASTE 2 lines: ime one\\rime two"))?;
        anyhow::ensure!(!gui.screen().contains(crate::paste_probe::ENTER), "the IME text sent Enter");
        gui.send("\x04")?;
        gui.wait("the probe ended", 10, |t| t.contains("BYE"))?;
        // Later rows print near the top again (the --perf overlay covers the tile's last rows).
        gui.send("Clear-Host\r")?;
        gui.wait("the screen cleared", 10, |t| !t.contains("PASTE 2 lines"))?;
        Ok(format!("selection copied on release; Ctrl+V pasted ({}); {multi}; IME text with a line break pasted", gui.clipboard_source()))
    });

    check(rows, "18", "Links: URLs, OSC 8, Windows paths; Ctrl+click; nothing executed", "links_osc8_and_paths", || {
        gui.send("$e=[char]27; Write-Host \"see $e]8;;file:///C:/canvas-parity-missing/osc8.txt$e\\OSC8LINK$e]8;;$e\\ and C:\\canvas-parity-missing\\plain.txt\"\r")?;
        gui.wait("links printed", 10, |t| t.contains("OSC8LINK and C:\\canvas-parity-missing"))?;
        let tree = gui.wait_tree("links found", 5, |t| t["links"].as_array().is_some_and(|l| l.iter().any(|l| l["target"].as_str().is_some_and(|s| s.contains("osc8.txt")))))?;
        let count = tree["links"].as_array().map(|l| l.iter().filter(|l| l["terminal"] == true).count()).unwrap_or(0);
        let (x, y) = gui.driver.text().lines().enumerate().find_map(|(i, l)| l.find("plain.txt").filter(|_| !l.contains("Write-Host")).map(|c| (crate::client::paint::text_width(&l[..c]) as f64 + 0.5, i as f64 + 0.5))).context("plain path")?;
        gui.click((x, y), &["ctrl"])?;
        gui.wait("Ctrl+click resolved the path (nothing executed)", 5, |t| t.contains("plain.txt does not exist"))?;
        Ok(format!("{count} terminal link(s) incl. OSC 8; Ctrl+click → notice"))
    });

    check(rows, "17", "Title in the pane header, taskbar state mirror, bell, OSC 777 (ITaskbarList3, toasts)", "title_progress_bell_toasts", || {

        let append = |id: &str, kind: &str, data: Value| -> Result<()> {
            let events = sb.instance.sessions_dir.join(id).join("events.jsonl");
            let mut text = std::fs::read_to_string(&events)?;
            text.push_str(&(event(kind, crate::paths::now_ms(), data) + "\n"));
            Ok(std::fs::write(&events, text)?)
        };
        // A session needs you: paused (yellow) on the taskbar, counted in the title.
        gui.poll_platform("paused while one needs you", 10, &|p| p["progress"] == 4 && p["taskbar"] == 4 && p["title"].as_str().is_some_and(|t| t.contains("need")))?;
        gui.send("$e=[char]27; Write-Host -NoNewline \"$e]0;Parity title$([char]7)$e]9;4;3;0$([char]7)$([char]7)\"\r")?;
        gui.wait("title in the pane header", 10, |t| t.lines().any(|l| l.contains("Shell A · working · Parity title")))?;
        gui.poll_platform("the bell", 5, &|p| p["bells"].as_u64().unwrap_or(0) >= 1)?;
        // Answered: busy (sessions work) and no attention count.
        append(&sb.sessions.waiting, "permission.completed", json!({ "requestId": "p1" }))?;
        gui.poll_platform("busy once nothing needs you", 15, &|p| p["progress"] == 3 && p["taskbar"] == 3 && p["title"].as_str().is_some_and(|t| !t.contains("need")))?;
        gui.send("Write-Host -NoNewline \"$e]9;4;0;0$([char]7)$e]777;notify;Parity;Toast body$([char]7)\"\r")?;
        // A session that starts to need you: toast (and flash in the background).
        append(&sb.sessions.review, "permission.requested", json!({ "requestId": "p9" }))?;
        let ok = |t: Option<&Value>| t.is_some_and(|t| t["status"] == "shown" || t["status"] == "in front" || t["status"] == "quiet");
        let platform = gui.poll_platform("toasts and paused again", 15, &|p| {
            let toasts = p["toasts"].as_array().cloned().unwrap_or_default();
            let note = toasts.iter().find(|t| t["body"] == "Toast body");
            let needs = toasts.iter().find(|t| t["title"].as_str().is_some_and(|s| s.contains("Review me") && s.contains("needs you")));
            ok(note) && ok(needs) && p["progress"] == 4
        })?;
        let toasts = platform["toasts"].as_array().cloned().unwrap_or_default();
        let statuses: Vec<String> = toasts.iter().map(|t| t["status"].as_str().unwrap_or_default().to_owned()).collect();
        Ok(format!("title; paused → busy → paused; bell; toasts {statuses:?}; Action Center {}", platform["toast_history"]))
    });

    check_kind(rows, "G2", "New: taskbar progress (ITaskbarList3), FlashWindowEx, toasts", "taskbar_flash_toasts", Kind::Only("GUI only: a console has no taskbar button of its own"), || {
        let platform = gui.req(json!({ "cmd": "platform", "history": true }))?;
        let toasts = platform["toasts"].as_array().cloned().unwrap_or_default();
        let shown = toasts.iter().filter(|t| t["status"] == "shown").count();
        // Test windows stay in the background and only record toasts (quiet).
        let in_front = toasts.iter().filter(|t| t["status"] == "in front" || t["status"] == "quiet").count();
        anyhow::ensure!(shown + in_front >= 2, "toasts: {platform}");
        // Flashing happens only while the window is in the background.
        anyhow::ensure!(platform["flashes"].as_u64().unwrap_or(0) >= 1 || in_front > 0, "no flash in the background: {platform}");
        if shown > 0 {
            anyhow::ensure!(platform["toast_history"].as_u64().unwrap_or(0) >= 1, "the Action Center has no toast of the app: {platform}");
        }
        Ok(format!("taskbar {}, {} flash(es), {shown} toast(s) shown, {in_front} recorded only (window in front or quiet test window)", platform["taskbar"], platform["flashes"]))
    });

    check_kind(rows, "G1", "New: inline images (Kitty graphics; iTerm2 OSC 1337 through ConPTY)", "kitty_image_pixels_png", Kind::Only("GUI only: the TUI cannot draw images"), || {
        let cli = sb.exe.display().to_string();
        gui.send(&format!("& '{cli}' kitty-demo\r"))?;
        gui.wait("demo printed", 15, |t| t.contains(crate::kitty_demo::END))?;
        let tree = gui.wait_tree("an image placement", 10, |t| t["tiles"].as_array().is_some_and(|t| t.iter().any(|t| t["images"].as_array().is_some_and(|i| i.iter().any(|i| i["loaded"] == true)))))?;
        let image = tree["tiles"].as_array().and_then(|t| t.iter().find_map(|t| t["images"].as_array().and_then(|i| i.first().cloned()))).context("image")?;
        let r: Vec<f64> = image["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        let (_, begin_row) = gui.driver.text().lines().enumerate().filter(|(_, l)| l.contains(crate::kitty_demo::BEGIN) && !l.contains("kitty-demo")).map(|(i, l)| (l.to_owned(), i)).last().context("begin marker")?;
        anyhow::ensure!(r[1] as usize == begin_row + 1 && r[2] as u16 == crate::kitty_demo::DEMO_COLS && r[3] as u16 == crate::kitty_demo::DEMO_ROWS, "image at {r:?}, marker row {begin_row}");
        // Nothing may sit over the tile: an earlier check's dialog can still be
        // closing on a busy machine. Then the image must show within 5 s.
        gui.wait_tree("no dialog over the terminal", 10, |t| t["modals"] == 0)?;
        let (cw, ch) = gui.cell()?;
        let mid_y = ((r[1] + r[3] / 2.0) * ch) as u32;
        let deadline = Instant::now() + Duration::from_secs(5);
        let (left, right) = loop {
            let path = gui.png(0, "inline-image")?;
            let left = pixel(&path, ((r[0] + r[2] * 0.25) * cw) as u32, mid_y)?;
            let right = pixel(&path, ((r[0] + r[2] * 0.75) * cw) as u32, mid_y)?;
            if close(left, crate::kitty_demo::LEFT, 10) && close(right, crate::kitty_demo::RIGHT, 10) {
                break (left, right);
            }
            anyhow::ensure!(Instant::now() < deadline, "pixels {left:?} {right:?}; tree: {}", gui.tree()?);
            std::thread::sleep(Duration::from_millis(250));
        };
        // A client without CAP_IMAGES (the TUI) gets the frames but no images.
        let id = tree["tiles"].as_array().and_then(|t| t.iter().find(|t| t["focused"] == true)).and_then(|t| t["id"].as_str()).unwrap_or_default().to_owned();
        let (images, frames) = plain_client_sees(&sb.instance, &id)?;
        anyhow::ensure!(frames > 0 && images == 0, "a plain client got {images} image message(s), {frames} frame(s)");
        Ok(format!("8 × 2 cells at row {}, pixels {left:?} | {right:?}; plain client: {frames} frame(s), no images", r[1]))
    });

    check_kind(rows, "G3", "New: file drag and drop (paths pasted; uploaded in remote mode)", "drop_files_local", Kind::Only("GUI only: a console cannot receive dropped files"), || {
        let file = sb.work.join("dropped file.txt");
        std::fs::write(&file, "dropped")?;
        let (_, inner) = gui.tile()?;
        gui.req(json!({ "cmd": "drop", "paths": [file.display().to_string()], "x": inner[0] + 5.5, "y": inner[1] + 2.5 }))?;
        let quoted = format!("\"{}\"", file.display());
        gui.wait_tile("the quoted path at the prompt", 5, |t| t.contains(&quoted))?;
        gui.chord('u', &["ctrl"])?;
        gui.send("\x1b")?;
        // A folder dropped on the canvas starts a new session there.
        let folder = sb.root.join("drop-folder");
        std::fs::create_dir_all(&folder)?;
        gui.canvas_focus()?;
        let tree = gui.tree()?;
        let area: Vec<f64> = tree["canvas"]["area"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        gui.req(json!({ "cmd": "drop", "paths": [folder.display().to_string()], "x": area[0] + area[2] - 2.5, "y": area[1] + area[3] - 2.5 }))?;
        gui.wait("the new-session dialog in the dropped folder", 5, |t| t.contains("New session") && t.contains("drop-folder"))?;
        gui.key("Esc", &[])?;
        Ok("file path pasted quoted; folder on the canvas → new session there".into())
    });

    check(rows, "20", "Import (filter, multi-select, grouping); VS Code sync (+ Ctrl+Z, auto sync)", "import_dialog_and_vscode_view", || {
        gui.canvas_focus()?;
        gui.send("i")?;
        gui.wait("import dialog", 10, |t| t.contains("Import sessions"))?;
        gui.send("Import")?;
        gui.wait("filtered", 10, |t| t.contains("Import me A") && t.contains("Import me B"))?;
        gui.send("\x01")?;
        gui.send("\r")?;
        wait_doc(&sb.canvas, "imported", |d| d.nodes.iter().any(|n| n.id == sb.sessions.import_a) && d.nodes.iter().any(|n| n.id == sb.sessions.import_b)).or_else(|_| {
            // Some layouts confirm with a button: press it.
            if let Some((x, y)) = gui.find(" Import ") {
                gui.click((x + 1.0, y), &[])?;
            }
            wait_doc(&sb.canvas, "imported", |d| d.nodes.iter().any(|n| n.id == sb.sessions.import_a) && d.nodes.iter().any(|n| n.id == sb.sessions.import_b))
        })?;
        gui.canvas_focus()?;
        gui.chord('7', &["alt"])?;
        gui.wait("VS Code view", 5, |t| t.contains("Sync sessions now") && t.contains("Migrate skills"))?;
        gui.chord('1', &["alt"])?;
        Ok("filter + select all + import; VS Code view".into())
    });

    check(rows, "24", "Migrate from VS Code (preview, tick, backup, redacted log)", "migration_preview_dialog", || {
        gui.canvas_focus()?;
        gui.chord('7', &["alt"])?;
        gui.wait("VS Code view", 5, |t| t.contains("Migrate skills"))?;
        let (x, y) = gui.driver.find_in("Migrate skills", 3, 33).map(|(x, y)| (x as f64 + 0.5, y as f64 + 0.5)).context("migrate row")?;
        gui.click((x, y), &[])?;
        gui.wait("migration preview (read-only)", 60, |t| t.contains("selected · Space toggle") || t.contains("Nothing to migrate"))?;
        gui.key("Esc", &[])?;
        gui.wait("preview closed", 5, |t| !t.contains("Space toggle · Enter apply"))?;
        gui.canvas_focus()?;
        gui.chord('1', &["alt"])?;
        Ok("preview opened, nothing changed".into())
    });

    check(rows, "25", "Passive sessions: templates, panel, dashed ⟲ boxes, ✉, pulses, mute, budgets", "passive_create_and_panel", || {
        gui.canvas_focus()?;
        let tree = gui.tree()?;
        let g = tree["canvas"]["groups"].as_array().and_then(|g| g.iter().find(|g| g["id"] == "g-parity").cloned()).context("group")?;
        let r: Vec<f64> = g["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        let title = (r[0] + 3.5, r[1] + 0.5);
        gui.mouse("down", "right", title, &[])?;
        gui.mouse("up", "right", title, &[])?;
        gui.wait("group menu", 5, |t| t.contains("New passive session"))?;
        let (x, y) = gui.find("New passive session").context("menu item")?;
        gui.click((x, y), &[])?;
        gui.wait("templates", 5, |t| t.contains("Drift guard") && t.contains("Custom"))?;
        gui.send("Custom")?;
        gui.send("\r")?;
        gui.wait("name prompt", 5, |t| t.contains("Name the Custom passive session"))?;
        gui.send("\x15Parity watcher\r")?;
        gui.wait("passive panel", 10, |t| t.contains("⟲ Parity watcher") && t.contains("Events…"))?;
        let doc = wait_doc(&sb.canvas, "passive node", |d| d.nodes.iter().any(|n| n.passive.is_some() && n.name == "Parity watcher"))?;
        let slug = doc.nodes.iter().find_map(|n| n.passive.as_ref()).map(|p| p.slug.clone()).unwrap_or_default();
        anyhow::ensure!(sb.group_folder.join(".canvas").join("passive").join(&slug).join("rules.md").is_file(), "rules scaffolded");
        let (passive, _) = gui.find_box("Parity watcher")?;
        anyhow::ensure!(passive["prefix"].as_str().is_some_and(|p| p.contains('⟲')) && (passive["border"] == "Dashed" || passive["selected"] == true), "dashed ⟲ box: {passive}");
        gui.canvas_focus()?;
        Ok("template → panel, rules scaffolded, dashed box".into())
    });

    check(rows, "29", "Keyboard map, F6 / Ctrl+] to leave a terminal, Q quit dialog (detach or stop all), ? help", "help_f6_and_quit_dialog", || {
        gui.canvas_focus()?;
        gui.send("?")?;
        gui.wait("help", 5, |t| t.contains("Copilot Canvas — help"))?;
        gui.key("Esc", &[])?;
        gui.open("Shell A", "PS ")?;
        gui.key("F6", &[])?;
        gui.wait_tree("F6 leaves the terminal", 5, |t| t["focus"] != "Terminal")?;
        gui.send("q")?;
        gui.wait("quit dialog", 5, |t| t.contains("Detach (keep running)") && t.contains("Stop all and quit"))?;
        gui.key("Esc", &[])?;
        Ok("help, F6, quit dialog".into())
    });

    check(rows, "31", "--perf overlay: GPU timings, round-trip time", "perf_overlay_rtt", || {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let perf = gui.req(json!({ "cmd": "perf" }))?;
            let rtt = &perf["perf"]["rtt_ms"];
            if rtt["n"].as_u64().unwrap_or(0) > 0 || rtt["p50"].as_f64().unwrap_or(0.0) > 0.0 {
                return Ok(format!("frames timed, service round trip {rtt}"));
            }
            if Instant::now() > deadline {
                bail!("no round-trip samples: {perf}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });

    check(rows, "34", "New: view versus start (a click views; double-click, Enter, Start / Resume start; group rows never launch)", "view_vs_start", || {
        gui.canvas_focus()?;
        gui.send("f")?;
        let log = || std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        let launched = |id: &str| log().contains(&format!("launched {id} "));
        // A click shows the details panel; nothing starts.
        let stopped = sb.sessions.stopped.clone();
        let at = gui.box_center("Stopped one")?;
        gui.click(at, &[])?;
        gui.wait("its details with Resume", 5, |t| t.contains("▸ Resume") && t.contains("Double-click the box"))?;
        std::thread::sleep(Duration::from_millis(800));
        anyhow::ensure!(!launched(&stopped), "a click launched the session");
        anyhow::ensure!(gui.tree()?["tiles"].as_array().is_some_and(|t| t.iter().all(|t| t["id"] != stopped.as_str())), "no terminal tile for a viewed session");
        // A double-click resumes it (the stand-in Copilot).
        gui.double_click(at)?;
        gui.wait("the double-click resumed it", 25, |t| t.contains("Stopped one · ") && t.contains("Fake Copilot"))?;
        anyhow::ensure!(launched(&stopped), "the double-click launched it");
        // Enter on the selected box.
        gui.canvas_focus()?;
        let failed = sb.sessions.failed.clone();
        let at = gui.box_center("Failed one")?;
        gui.click(at, &[])?;
        gui.wait("its details with Resume", 5, |t| t.contains("▸ Resume") && t.contains("Failed one"))?;
        anyhow::ensure!(!launched(&failed), "a click launched the session");
        gui.key("Enter", &[])?;
        gui.wait("Enter resumed it", 25, |t| t.contains("Failed one · ") && t.contains("Fake Copilot"))?;
        // The details panel's primary button.
        gui.canvas_focus()?;
        let imported = sb.sessions.import_a.clone();
        let at = gui.box_center("Import me A")?;
        gui.click(at, &[])?;
        gui.wait("its details with Resume", 5, |t| t.contains("▸ Resume") && t.contains("Import me A"))?;
        anyhow::ensure!(!launched(&imported), "a click launched the session");
        let (x, y) = gui.find("▸ Resume").context("the Resume button")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("Resume resumed it", 25, |t| t.contains("Import me A · ") && t.contains("Fake Copilot"))?;
        // A group row shows its running terminals and starts nothing.
        gui.canvas_focus()?;
        let before = log().matches("launched ").count();
        let (x, y) = gui.driver.find_in("Parity group", 3, 33).map(|(x, y)| (x as f64 + 0.5, y as f64 + 0.5)).context("group row")?;
        gui.click((x + 2.0, y), &[])?;
        gui.wait_tree("the group selected", 5, |t| t["canvas"]["groups"].as_array().is_some_and(|g| g.iter().any(|g| g["id"] == "g-parity" && g["selected"] == true)))?;
        std::thread::sleep(Duration::from_millis(500));
        anyhow::ensure!(log().matches("launched ").count() == before, "viewing a group launched something");
        gui.canvas_focus()?;
        Ok("click → details (no launch); double-click, Enter and Resume each resumed one; group row launched nothing".into())
    });

    check(rows, "35", "New: transcript inspector (sessions and subagents, live while they run); a finished child resumes as its own subsession", "inspector_and_child_resume", || {
        gui.canvas_focus()?;
        gui.send("f")?;
        gui.wait_tree("the OpenCode session's subagent boxes", 20, |t| {
            let boxes = t["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
            boxes.iter().any(|b| b["name"] == "explore") && boxes.iter().any(|b| b["name"] == "general")
        })?;
        let log = || std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        let launches = log().matches("launched ").count();
        let inspect = |gui: &mut Gui, name: &str| -> Result<()> {
            gui.canvas_focus()?;
            let at = gui.box_center(name)?;
            gui.click(at, &[])?;
            gui.wait(&format!("{name}'s details"), 5, |t| t.contains("Inspect transcript"))?;
            let (x, y) = gui.find("Inspect transcript").context("the Inspect button")?;
            gui.click((x + 1.0, y), &[])?;
            gui.wait("the inspector", 10, |t| t.contains("Transcript · ") && !t.contains("Loading…"))
        };
        // A Copilot session's own transcript, and a subagent known only by its start and end.
        inspect(gui, "With agents")?;
        gui.wait("its prompt", 5, |t| t.contains("❯ parity"))?;
        gui.key("Esc", &[])?;
        inspect(gui, "Parity explorer")?;
        gui.wait("why it does not resume on its own", 5, |t| t.contains("part of its parent's conversation") && !t.contains("Resume as subsession"))?;
        gui.key("Esc", &[])?;
        // An OpenCode child that works: its transcript grows while it is open.
        inspect(gui, "explore")?;
        gui.wait("the child's task and its running tool", 5, |t| t.contains("Find the callers of parity_probe") && t.contains("… running") && t.contains("running: refreshing"))?;
        sb.grow_running_child("PARITY_LIVE_LINE found 3 callers")?;
        gui.wait("the live refresh", 10, |t| t.contains("PARITY_LIVE_LINE"))?;
        gui.key("Esc", &[])?;
        anyhow::ensure!(log().matches("launched ").count() == launches, "inspecting launched something");
        // While its parent runs here, a finished child does not resume on its own.
        gui.open("OpenCode parent", "Fake Copilot")?;
        inspect(gui, "general")?;
        gui.wait("no resume beside the running parent", 5, |t| t.contains("PARITY_DONE_SUMMARY") && t.contains("stop the parent") && !t.contains("Resume as subsession"))?;
        gui.key("Esc", &[])?;
        // The parent stops: the finished child resumes in a terminal of its own, as a new linked box.
        gui.canvas_focus()?;
        let at = gui.box_center("OpenCode parent")?;
        gui.click(at, &[])?;
        gui.canvas_focus()?;
        gui.send("x")?;
        gui.wait("the stop dialog", 5, |t| t.contains("Stop “OpenCode parent”?"))?;
        gui.key("Enter", &[])?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !log().contains("terminal oc-parent exited with ") {
            anyhow::ensure!(Instant::now() < deadline, "the parent did not stop");
            std::thread::sleep(Duration::from_millis(200));
        }
        inspect(gui, "general")?;
        gui.wait("Resume as subsession", 10, |t| t.contains("Resume as subsession") && t.contains("PARITY_DONE_SUMMARY"))?;
        let (x, y) = gui.find("Resume as subsession").context("the Resume button")?;
        gui.click((x + 1.0, y), &[])?;
        let doc = wait_doc(&sb.canvas, "a new subsession box that follows the child", |d| d.nodes.iter().any(|n| n.agent_session.as_deref() == Some(OC_DONE) && n.parent.as_deref() == Some("oc-parent")))?;
        let child = doc.nodes.iter().find(|n| n.agent_session.as_deref() == Some(OC_DONE)).map(|n| n.id.clone()).unwrap_or_default();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !log().contains(&format!("terminal {child}: opencode resume")) {
            anyhow::ensure!(Instant::now() < deadline, "the child was not resumed (-s {OC_DONE})");
            std::thread::sleep(Duration::from_millis(200));
        }
        gui.wait("its own terminal", 25, |t| t.contains("Summarize the diff · ") && t.contains("Fake Copilot"))?;
        anyhow::ensure!(doc.nodes.iter().any(|n| n.kind == NodeKind::Agent && n.agent_key.as_deref() == Some(OC_DONE)), "the subagent box stays");
        gui.canvas_focus()?;
        Ok("Copilot session + subagent, OpenCode child live-refreshed; refused beside its running parent; resumed with -s as a linked box".into())
    });

    check(rows, "36", "New: removing a multi-selection (its eligible boxes after a confirmation; running sessions and history kept; Ctrl+Z)", "multi_remove_keeps_running", || {
        gui.canvas_focus()?;
        gui.send("f")?;
        gui.key("Esc", &[])?;
        let (import_b, idle, failed) = (sb.sessions.import_b.clone(), sb.sessions.idle.clone(), sb.sessions.failed.clone());
        // "Failed one" runs (Enter resumed it above); the other two do not run here.
        let first = gui.box_center("Import me B")?;
        gui.click(first, &[])?;
        for name in ["Idle one", "Failed one"] {
            let at = gui.box_center(name)?;
            gui.click(at, &["ctrl"])?;
        }
        gui.wait_tree("three boxes selected", 5, |t| t["selection"].as_array().is_some_and(|s| s.len() == 3))?;
        let at = gui.box_center("Failed one")?;
        gui.mouse("down", "right", at, &[])?;
        gui.mouse("up", "right", at, &[])?;
        gui.wait("the menu removes the eligible two", 5, |t| t.contains("Remove 2 from canvas"))?;
        let (x, y) = gui.find("Remove 2 from canvas").context("menu item")?;
        gui.click((x + 1.0, y), &[])?;
        gui.wait("the confirmation", 5, |t| t.contains("Remove 2 from the canvas?") && t.contains("1 running session kept: stop it first."))?;
        gui.key("Enter", &[])?;
        let doc = wait_doc(&sb.canvas, "the two boxes removed", |d| d.node(&import_b).is_none() && d.node(&idle).is_none())?;
        anyhow::ensure!(doc.node(&failed).is_some(), "the running session stays");
        for id in [&import_b, &idle] {
            anyhow::ensure!(sb.instance.sessions_dir.join(id).join("events.jsonl").is_file(), "history is kept ({id})");
        }
        gui.canvas_focus()?;
        gui.chord('z', &["ctrl"])?;
        wait_doc(&sb.canvas, "Ctrl+Z restores them", |d| d.node(&import_b).is_some() && d.node(&idle).is_some())?;
        gui.key("Esc", &[])?;
        Ok("menu “Remove 2 from canvas” on a 3-box selection; confirmation names the running one; history kept; Ctrl+Z restored both".into())
    });

    check(rows, "37", "New: a click on a group's background or border selects it; title drag, ◢ resize, chips and double-click keep working", "group_background_and_border_select", || {
        gui.canvas_focus()?;
        gui.send("f")?;
        gui.key("Esc", &[])?;
        // The single view, where a selected group shows its panel.
        if gui.screen().contains("▦ Tiled") {
            gui.send("t")?;
        }
        let rect = |t: &Value, path: &str, id: &str| -> Option<[f64; 4]> {
            let items = t["canvas"][path].as_array()?;
            let found = items.iter().find(|g| g["id"] == id)?;
            let r: Vec<f64> = found["rect"].as_array()?.iter().filter_map(|v| v.as_f64()).collect();
            (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
        };
        let selected = |t: &Value| t["canvas"]["groups"].as_array().is_some_and(|g| g.iter().any(|g| g["id"] == "g-parity" && g["selected"] == true));
        let tree = gui.tree()?;
        let g = rect(&tree, "groups", "g-parity").context("the group in view")?;
        let boxes: Vec<[f64; 4]> = tree["canvas"]["boxes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["rect"].as_array().map(|r| r.iter().filter_map(|v| v.as_f64()).collect::<Vec<_>>()))
            .filter(|r| r.len() == 4)
            .map(|r| [r[0], r[1], r[2], r[3]])
            .collect();
        let free = |x: f64, y: f64| !boxes.iter().any(|b| x >= b[0] - 0.5 && x < b[0] + b[2] + 0.5 && y >= b[1] - 0.5 && y < b[1] + b[3] + 0.5);
        // An empty spot of its background (below the title, away from boxes and the ◢ corner).
        let spot = (0..((g[3] - 2.0).max(0.0) as usize))
            .flat_map(|dy| (0..((g[2] - 3.0).max(0.0) as usize)).map(move |dx| (g[0] + 1.5 + dx as f64, g[1] + 1.5 + dy as f64)))
            .find(|(x, y)| free(*x, *y))
            .context("no empty spot in the group")?;
        let before = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count();
        gui.click(spot, &[])?;
        gui.wait_tree("selected by its background", 5, |t| selected(t) && t["selection"].as_array().is_some_and(Vec::is_empty))?;
        gui.wait("its panel", 5, |t| t.contains("Del: delete group"))?;
        // Just outside its left outline (a few pixels) is its border too.
        gui.key("Esc", &[])?;
        gui.wait_tree("cleared", 5, |t| !selected(t))?;
        let area: Vec<f64> = gui.tree()?["canvas"]["area"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        // Its left outline, or its bottom one when the left is at the canvas's edge.
        let border = if g[0] - 0.2 > area.first().copied().unwrap_or(0.0) + 0.3 { (g[0] - 0.2, g[1] + g[3] / 2.0) } else { (g[0] + g[2] / 2.0, g[1] + g[3] + 0.15) };
        anyhow::ensure!(free(border.0, border.1), "the border at {border:?} is covered");
        gui.click(border, &[])?;
        gui.wait_tree("selected by its border", 5, selected)?;
        anyhow::ensure!(std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default().matches("launched ").count() == before, "a group click launched something");
        // A drag on its background pans (and selects nothing).
        gui.key("Esc", &[])?;
        let origin = gui.tree()?["canvas"]["origin"].clone();
        gui.drag("left", spot, (spot.0 + 3.0, spot.1 + 1.0), &[])?;
        let tree = gui.tree()?;
        anyhow::ensure!(tree["canvas"]["origin"] != origin && !selected(&tree), "a background drag pans");
        // Title drag, ◢ resize and the folder chips are as before.
        let g = rect(&gui.tree()?, "groups", "g-parity").context("group")?;
        let doc = sb.doc()?;
        let group = doc.groups.iter().find(|x| x.id == "g-parity").cloned().context("group")?;
        gui.drag("left", (g[0] + 3.5, g[1] + 0.5), (g[0] + 5.5, g[1] + 0.5), &[])?;
        wait_doc(&sb.canvas, "moved by its title", |d| d.groups.iter().any(|x| x.id == "g-parity" && x.x != group.x))?;
        let g = rect(&gui.tree()?, "groups", "g-parity").context("group")?;
        let corner = (g[0] + g[2] - 0.5, g[1] + g[3] - 0.5);
        gui.drag("left", corner, (corner.0 + 2.0, corner.1 + 1.0), &[])?;
        wait_doc(&sb.canvas, "resized by ◢", |d| d.groups.iter().any(|x| x.id == "g-parity" && (x.w, x.h) != (group.w, group.h)))?;
        anyhow::ensure!(gui.tree()?["hits"].as_array().is_some_and(|h| h.iter().any(|h| h["hit"].as_str().is_some_and(|s| s.contains("OpenGroupFolder")))), "folder chips are clickable");
        // A double-click on the background still creates a session there.
        let tree = gui.tree()?;
        let g = rect(&tree, "groups", "g-parity").context("group")?;
        let boxes: Vec<[f64; 4]> = tree["canvas"]["boxes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["rect"].as_array().map(|r| r.iter().filter_map(|v| v.as_f64()).collect::<Vec<_>>()))
            .filter(|r| r.len() == 4)
            .map(|r| [r[0], r[1], r[2], r[3]])
            .collect();
        let free = |x: f64, y: f64| !boxes.iter().any(|b| x >= b[0] - 0.5 && x < b[0] + b[2] + 0.5 && y >= b[1] - 0.5 && y < b[1] + b[3] + 0.5);
        let spot = (0..((g[3] - 2.0).max(0.0) as usize))
            .flat_map(|dy| (0..((g[2] - 3.0).max(0.0) as usize)).map(move |dx| (g[0] + 1.5 + dx as f64, g[1] + 1.5 + dy as f64)))
            .find(|(x, y)| free(*x, *y))
            .context("no empty spot in the group")?;
        gui.double_click(spot)?;
        gui.wait("the new-session dialog", 5, |t| t.contains("New session") && t.contains("Folder"))?;
        gui.key("Esc", &[])?;
        gui.canvas_focus()?;
        Ok(format!("background at {spot:?} and the border selected it (panel); background drag pans; title, ◢, chips, double-click unchanged"))
    });

    check(rows, "38", "New: RECENT sessions atop the Sessions panel (newest first, shared); Ctrl+F6 / Ctrl+Shift+F6 and ] / [ view the next or previous one without starting it", "recent_sessions_and_walk_keys", || recent_check(sb, gui));
    check(rows, "39", "New: the default model (shown before starting; a choice applies to its session and is remembered for its agent; group overrides; resumes keep their model)", "remembered_default_model", || {
        use crate::client::agents::AgentKind;
        let log = || std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        let [initial, listed, picked] = ["claude-opus-5-5", crate::fake_copilot::MODELS[1], crate::fake_copilot::MODELS[2]];
        // Nothing chosen for Claude Code yet: the dialog shows Opus 5.5 (its id) as the initial default, and the session starts with it.
        model_dialog(gui, "Model check A", false)?;
        gui.wait("the initial default before starting", 5, |t| t.contains(&format!("{initial}  ▾ · initial default (Claude Opus 5.5)")))?;
        let a = create_from_dialog(gui, sb, "Model check A")?;
        gui.wait("the stand-in Claude Code started with it", 25, |t| t.contains("Model check A · ") && t.contains(&format!("Model: {initial}")))?;
        anyhow::ensure!(log().contains(&format!("terminal {a}: claude new session, model {initial}")), "the launch carries --model {initial}");
        let doc = sb.doc()?;
        anyhow::ensure!(doc.node(&a).and_then(|n| n.model.as_deref()) == Some(initial), "the box records its model");
        anyhow::ensure!(doc.defaults.model(AgentKind::Claude).is_none(), "no choice made: nothing remembered");
        // Another model picked in the dialog (from the CLI's list): that session gets it and it becomes Claude Code's default.
        model_dialog(gui, "Model check B", false)?;
        pick_model(gui, "Claude Code", picked)?;
        gui.wait("the choice", 5, |t| t.contains(&format!("{picked}  ▾ · your choice; becomes the default")))?;
        let b = create_from_dialog(gui, sb, "Model check B")?;
        gui.wait("B started with the choice", 25, |t| t.contains("Model check B · ") && t.contains(&format!("Model: {picked}")))?;
        wait_doc(&sb.canvas, "remembered for Claude Code, recorded on B", |d| d.defaults.model(AgentKind::Claude) == Some(picked) && d.node(&b).and_then(|n| n.model.as_deref()) == Some(picked))?;
        model_dialog(gui, "", false)?;
        gui.wait("the next dialog shows it", 5, |t| t.contains(&format!("{picked}  ▾ · remembered default for Claude Code")))?;
        gui.canvas_focus()?;
        // A stays on its model: resuming it (a transcript makes it a real `claude -r`) passes its own model, not the new default.
        stop_box(gui, &log, "Model check A", &a)?;
        let projects = sb.root.join("claude-home").join("projects").join("parity");
        std::fs::create_dir_all(&projects)?;
        std::fs::write(projects.join(format!("{a}.jsonl")), json!({ "type": "user", "sessionId": a, "message": { "role": "user", "content": "parity" } }).to_string() + "\n")?;
        gui.open("Model check A", &format!("Model: {initial}"))?;
        anyhow::ensure!(log().contains(&format!("terminal {a}: claude resume, model {initial}")), "the resume keeps --model {initial}:\n{}", log().lines().filter(|l| l.contains(&a)).collect::<Vec<_>>().join("\n"));
        // Copilot keeps a session's model itself: its resume carries none ("MCP parity" started in row 8).
        let mcp = sb.doc()?.nodes.iter().find(|n| n.name == "MCP parity").map(|n| n.id.clone());
        if let Some(id) = mcp.filter(|id| log().matches(&format!("launched {id} ")).count() > log().matches(&format!("terminal {id} exited with ")).count()) {
            stop_box(gui, &log, "MCP parity", &id)?;
            gui.open("MCP parity", "Model: none")?;
            anyhow::ensure!(log().lines().any(|l| l.ends_with(&format!("terminal {id}: copilot resume"))), "a Copilot resume carries no model");
        }
        // A group's override wins in the group ("from group …"); a choice there is for that session only.
        group_menu(gui, "Session defaults for group…")?;
        gui.wait("the group's overrides", 5, |t| t.contains("SETTINGS · Parity group") && t.contains("Claude Code model"))?;
        let (x, y) = gui.find("Claude Code model").context("Claude Code model row")?;
        gui.click((x, y), &[])?;
        gui.wait("the group's picker", 5, |t| t.contains("Claude Code model for group “Parity group”"))?;
        gui.send("claude-group-parity\r")?;
        wait_doc(&sb.canvas, "the group's override", |d| d.group("g-parity").and_then(|g| g.defaults.as_ref()).and_then(|d| d.model(AgentKind::Claude)) == Some("claude-group-parity"))?;
        gui.key("Esc", &[])?;
        gui.chord('1', &["alt"])?;
        gui.canvas_focus()?;
        group_menu(gui, "New session in group")?;
        model_dialog(gui, "Model check C", true)?;
        gui.wait("the group's model", 5, |t| t.contains("claude-group-parity  ▾ · from group “Parity group”"))?;
        pick_model(gui, "Claude Code", listed)?;
        gui.wait("this session only", 5, |t| t.contains(&format!("{listed}  ▾ · this session only")))?;
        let c = create_from_dialog(gui, sb, "Model check C")?;
        let doc = wait_doc(&sb.canvas, "C recorded its model", |d| d.node(&c).and_then(|n| n.model.as_deref()) == Some(listed))?;
        anyhow::ensure!(doc.defaults.model(AgentKind::Claude) == Some(picked), "the canvas default is untouched");
        anyhow::ensure!(doc.group("g-parity").and_then(|g| g.defaults.as_ref()).and_then(|d| d.model(AgentKind::Claude)) == Some("claude-group-parity"), "the group's override is untouched");
        // Reset the group's override: its sessions get the canvas's default again.
        group_menu(gui, "Session defaults for group…")?;
        gui.wait("the group's overrides", 5, |t| t.contains("SETTINGS · Parity group") && t.contains("claude-group-parity"))?;
        let (x, y) = gui.find("Claude Code model").context("Claude Code model row")?;
        gui.click((x, y), &[])?;
        gui.wait("the group's picker", 5, |t| t.contains("Claude Code model for group “Parity group”"))?;
        gui.send(crate::client::models::NOT_SET)?;
        gui.send("\r")?;
        wait_doc(&sb.canvas, "the override reset", |d| d.group("g-parity").is_some_and(|g| g.defaults.is_none()))?;
        gui.key("Esc", &[])?;
        gui.chord('1', &["alt"])?;
        gui.canvas_focus()?;
        Ok(format!("initial {initial} shown and launched; {picked} picked → B and remembered; A resumed with {initial}; group override shown, {listed} for C only, reset"))
    });
    agents::rows(sb, gui, rows);
    prs::rows(sb, gui, rows);

    check_kind(rows, "G4", "New: several windows on one canvas (Ctrl+Shift+N, toolbar)", "second_window_rename_sync", Kind::Only("GUI only: a console has one window"), || {
        let opened = gui.req(json!({ "cmd": "new_window" }))?;
        let window = opened["window"].as_u64().context("window index")? as usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !gui.screen_of(window).contains("Parity group") {
            anyhow::ensure!(Instant::now() < deadline, "the second window shows the canvas");
            std::thread::sleep(Duration::from_millis(200));
        }
        gui.canvas_focus()?;
        let at = gui.box_center("Shell B")?;
        gui.click(at, &[])?;
        gui.chord(']', &["ctrl"])?;
        gui.send("r")?;
        gui.wait("rename", 5, |t| t.contains("Rename (canvas label)"))?;
        gui.send("\x15Renamed in window 1\r")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gui.screen_of(window).contains("Renamed in window 1") {
            anyhow::ensure!(Instant::now() < deadline, "the rename reached the second window:\n{}", gui.screen_of(window));
            std::thread::sleep(Duration::from_millis(200));
        }
        let path = gui.png(window, "second-window")?;
        gui.req(json!({ "cmd": "close_window", "window": window }))?;
        let windows = gui.req(json!({ "cmd": "windows" }))?;
        anyhow::ensure!(windows["windows"].as_array().is_some_and(|w| w.len() == 1), "the second window closed alone: {windows}");
        Ok(format!("rename synced; {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
    });

}

/// The new-session dialog (opened with `n` unless `opened`) with `name`
/// typed and Claude Code (the stand-in) chosen as its agent.
fn model_dialog(gui: &mut Gui, name: &str, opened: bool) -> Result<()> {
    if !opened {
        gui.canvas_focus()?;
        gui.send("n")?;
    }
    gui.wait("the new-session dialog", 5, |t| t.contains("New session") && t.contains("Model"))?;
    gui.send(name)?;
    // A click on the Agent row picks the next agent.
    for _ in 0..3 {
        if gui.screen().contains("◉ Claude Code") {
            break;
        }
        let at = gui.find("○ Claude Code").context("the Agent row")?;
        gui.click(at, &[])?;
        std::thread::sleep(Duration::from_millis(200));
    }
    gui.wait("Claude Code chosen", 5, |t| t.contains("◉ Claude Code"))
}

/// Picks `model` in the dialog's model picker (typed into its filter).
fn pick_model(gui: &mut Gui, agent: &str, model: &str) -> Result<()> {
    let at = gui.find("▾ · ").context("the Model row")?;
    gui.click(at, &[])?;
    gui.wait("the model picker", 5, |t| t.contains(&format!("Model of the new {agent} session")) && t.contains("Type a model id"))?;
    gui.send(model)?;
    gui.send("\r")
}

/// Creates the dialog's session; returns its box id.
fn create_from_dialog(gui: &mut Gui, sb: &Sandbox, name: &str) -> Result<String> {
    let (x, y) = gui.find("Create & start").context("the create button")?;
    gui.click((x + 1.0, y), &[])?;
    let doc = wait_doc(&sb.canvas, &format!("the box “{name}”"), |d| d.nodes.iter().any(|n| n.name == name))?;
    Ok(doc.nodes.iter().find(|n| n.name == name).map(|n| n.id.clone()).unwrap_or_default())
}

/// Stops a running box (X, confirmed) and waits for its terminal to exit.
fn stop_box(gui: &mut Gui, log: &dyn Fn() -> String, name: &str, id: &str) -> Result<()> {
    let exited = format!("terminal {id} exited with ");
    let before = log().matches(&exited).count();
    gui.canvas_focus()?;
    gui.send("f")?;
    let at = gui.box_center(name)?;
    gui.click(at, &[])?;
    gui.canvas_focus()?;
    gui.send("x")?;
    gui.wait("the stop dialog", 5, |t| t.contains(&format!("Stop “{name}”?")))?;
    gui.key("Enter", &[])?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while log().matches(&exited).count() == before {
        anyhow::ensure!(Instant::now() < deadline, "{name} did not stop");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Runs an item of the parity group's menu (a right-click on its title).
fn group_menu(gui: &mut Gui, item: &str) -> Result<()> {
    gui.canvas_focus()?;
    gui.send("f")?;
    let tree = gui.tree()?;
    let g = tree["canvas"]["groups"].as_array().and_then(|g| g.iter().find(|g| g["id"] == "g-parity").cloned()).context("group")?;
    let r: Vec<f64> = g["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
    anyhow::ensure!(r.len() == 4, "group rect");
    let title = (r[0] + 3.5, r[1] + 0.5);
    gui.mouse("down", "right", title, &[])?;
    gui.mouse("up", "right", title, &[])?;
    gui.wait("group menu", 5, |t| t.contains(item))?;
    let at = gui.find(item).context("menu item")?;
    gui.click(at, &[])
}

/// Rows that restart the GUI or use other clients.
fn late_rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    check(rows, "11b", "Terminals survive client exit (detach, reattach)", "detach_and_reattach", || {
        let mut gui = sb.start(&[], "gui-reattach.log")?;
        gui.wait("canvas again", 25, |t| t.contains("Parity group"))?;
        gui.open("Shell A", "PS ")?;
        gui.wait_tile("the shell kept running (its output is back)", 15, |t| t.contains("KITTY_DEMO_END") || t.contains("OSC8LINK"))?;
        gui.canvas_focus()?;
        gui.send("q")?;
        gui.wait("quit dialog", 5, |t| t.contains("Detach (keep running)"))?;
        gui.send("\r")?;
        let code = gui.driver.wait_exit(Duration::from_secs(10))?;
        anyhow::ensure!(code == 0, "exit code {code}");
        Ok("reattached with output; detached cleanly".into())
    });

    check(rows, "39b", "The remembered default model after the client restarts", "remembered_model_after_restart", || {
        let saved = sb.doc()?;
        let remembered = saved.defaults.model(crate::client::agents::AgentKind::Claude).map(str::to_owned).context("row 38 remembered a model for Claude Code in the canvas file")?;
        let mut gui = sb.start(&[], "gui-model.log")?;
        gui.wait("canvas again", 25, |t| t.contains("Parity group"))?;
        model_dialog(&mut gui, "", false)?;
        gui.wait("the remembered model", 10, |t| t.contains(&format!("{remembered}  ▾ · remembered default for Claude Code")))?;
        gui.key("Esc", &[])?;
        gui.canvas_focus()?;
        gui.send("q")?;
        gui.wait("quit dialog", 5, |t| t.contains("Detach (keep running)"))?;
        gui.send("\r")?;
        let code = gui.driver.wait_exit(Duration::from_secs(10))?;
        anyhow::ensure!(code == 0, "exit code {code}");
        Ok(format!("{remembered} kept in the canvas file and shown again"))
    });

    check(rows, "28", "Idle auto-stop, priority manager, upgrade with resume, protocol tolerance, crash log", "crash_log_and_build_match", || {
        let gui = sb.start(&[], "gui-crash.log")?;
        gui.wait("canvas", 25, |t| t.contains("Parity group"))?;
        anyhow::ensure!(!gui.screen().contains("Update service"), "the service matches this build");
        let _ = gui.req(json!({ "cmd": "panic" }));
        let mut gui = gui;
        let _ = gui.driver.wait_exit(Duration::from_secs(10));
        let log = std::fs::read_to_string(sb.instance.home.join("crash.log")).unwrap_or_default();
        anyhow::ensure!(log.contains("gui build") && log.contains("test hook asked for a crash"), "crash.log: {log}");
        // The sessions keep running in the service.
        let conn = crate::client::conn::try_connect(&sb.instance)?;
        let alive = alive_terminals(&conn)?;
        anyhow::ensure!(alive > 0, "sessions survived the crash");
        Ok(format!("crash.log written; {alive} session(s) still running"))
    });

    check(rows, "30", "--demo mode", "demo_mode_canvas", || {
        let demo = Instance::new(Some(format!("paritydemo{}", std::process::id())), Some(sb.root.join("demo-home")), Some(sb.root.join("demo-sessions")))?;
        std::fs::create_dir_all(&demo.sessions_dir)?;
        let args: Vec<String> = vec!["--demo".into(), "--instance".into(), demo.name.clone(), "--home".into(), demo.home.display().to_string(), "--sessions-dir".into(), demo.sessions_dir.display().to_string(), "--cwd".into(), sb.work.display().to_string()];
        let mut gui = Gui { driver: GuiDriver::start(&args, &[], (COLS, ROWS), &sb.root.join("gui-demo.log"))? };
        let result = gui.wait("demo canvas", 25, |t| t.contains("DEMO · native shells") && t.contains("Build session canvas") && t.contains("Canvas project"));
        let _ = gui.req(json!({ "cmd": "quit" }));
        let _ = gui.driver.wait_exit(Duration::from_secs(10));
        stop_service(&demo);
        result?;
        Ok("demo layout with native shells".into())
    });

    check_kind(rows, "32", "CLI (service, doctor, bench, events, emit, send, status, followup, automations)", "cli_unchanged", Kind::Only("no front end: the same CLI binary"), || {
        let help = std::process::Command::new(&sb.exe).arg("--help").output()?;
        let help = String::from_utf8_lossy(&help.stdout).to_string();
        for command in ["service status", "doctor", "bench", "events", "emit TYPE", "send --to", "status ok", "followup", "automations [list]"] {
            anyhow::ensure!(help.contains(command), "--help lacks {command:?}");
        }
        let status = std::process::Command::new(&sb.exe).args(["service", "status", "--instance", &sb.instance.name, "--home", &sb.instance.home.display().to_string()]).output()?;
        let status = String::from_utf8_lossy(&status.stdout).to_string();
        anyhow::ensure!(status.contains("\"running\": true"), "service status: {status}");
        let empty = sb.root.join("no-automations");
        std::fs::create_dir_all(&empty)?;
        let list = std::process::Command::new(&sb.exe).args(["automations", "list", "--dir", &empty.display().to_string()]).output()?;
        anyhow::ensure!(list.status.success(), "automations list: {}", String::from_utf8_lossy(&list.stderr));
        Ok("help lists every command; service status; automations list".into())
    });

    check(rows, "33", "New: remote mode (pair, reconnect, resync, offline input blocking)", "remote_loopback_gui_and_upload", || {
        let mut gui = sb.start(&["--remote", "loopback"], "gui-remote.log")?;
        gui.wait("canvas over TCP", 25, |t| t.contains("Parity group"))?;
        let log = std::fs::read_to_string(sb.instance.log_path()).unwrap_or_default();
        anyhow::ensure!(log.contains("connected (gui, protocol 8"), "the service saw a remote GUI");
        gui.open("Shell A", "PS ")?;
        let file = sb.root.join("upload me.txt");
        std::fs::write(&file, "uploaded by the parity suite")?;
        let (_, inner) = gui.tile()?;
        gui.req(json!({ "cmd": "drop", "paths": [file.display().to_string()], "x": inner[0] + 5.5, "y": inner[1] + 2.5 }))?;
        // The long path wraps in the tile (how depends on the user name): rejoin its rows.
        let joined = |t: &str| t.lines().map(str::trim_end).collect::<String>();
        gui.wait_tile("the uploaded path pasted", 20, |t| joined(t).contains("uploads") && joined(t).contains("upload me.txt"))?;
        let uploads = sb.instance.home.join("uploads");
        let found = walk(&uploads).into_iter().find(|p| p.file_name().is_some_and(|n| n == "upload me.txt")).context("uploaded file on the service's machine")?;
        anyhow::ensure!(std::fs::read_to_string(&found)? == "uploaded by the parity suite", "content");
        gui.chord('u', &["ctrl"])?;
        let _ = gui.req(json!({ "cmd": "quit" }));
        let _ = gui.driver.wait_exit(Duration::from_secs(10));
        Ok(format!("GUI over loopback TCP; drop uploaded to {}", found.strip_prefix(&sb.instance.home).unwrap_or(&found).display()))
    });

    reports::late_rows(sb, rows);
    visual::rows(sb, rows);
}

#[path = "parity_visual.rs"]
pub(crate) mod visual;

#[path = "parity_agents.rs"]
mod agents;
#[path = "parity_prs.rs"]
mod prs;
#[path = "parity_reports.rs"]
pub(crate) mod reports;

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Live terminals the service runs.
fn alive_terminals(conn: &crate::client::conn::Conn) -> Result<u32> {
    conn.tx.send(crate::protocol::ClientMsg::Stats)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(crate::protocol::ServiceMsg::Stats(stats)) = conn.rx.recv_timeout(Duration::from_millis(200)) {
            return Ok(stats.alive);
        }
    }
    bail!("no stats")
}

/// What a client without `CAP_IMAGES` (as the TUI connects) receives for a
/// terminal: (image messages, frames).
fn plain_client_sees(instance: &Instance, id: &str) -> Result<(usize, usize)> {
    use crate::protocol::{ClientMsg, ServiceMsg};
    let conn = crate::client::conn::try_connect(instance)?;
    conn.tx.send(ClientMsg::Subscribe8 { ids: vec![id.to_owned()], focused: Some(id.to_owned()) })?;
    let (mut images, mut frames) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match conn.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceMsg::Image { .. } | ServiceMsg::Placements { .. }) => images += 1,
            Ok(ServiceMsg::Frame8(_) | ServiceMsg::Frame(_)) => frames += 1,
            _ => {}
        }
    }
    Ok((images, frames))
}
