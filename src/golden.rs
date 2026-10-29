//! `copilot-canvas selftest --client gui --golden [--update]`: golden
//! screenshots of the GUI compared with a tolerant image diff (PLAN.md §10,
//! "GUI": the self-test's steps plus tolerant image diffs).
//!
//! The GUI runs with its test hooks at 170 × 46 cells on scripted states that
//! do not depend on the machine's data or the clock: the demo canvas (idle,
//! zoomed out to 50 %, a context menu, the new-session dialog), two tiled
//! PowerShell terminals printing fixed text (colours, styles, underline
//! kinds, emoji, CJK) under a fixed prompt, and the parity sandbox's canvas
//! with every status. Animations hold one frame (the `freeze` hook), notices
//! are waited out, and a capture counts once the screen text has settled and
//! two renders in a row are identical. Regions that change legitimately are
//! masked: the service's figures in the status bar, folders inside the
//! temporary sandbox, automations' next-run times (relative to today); the
//! stored goldens show them hatched.
//!
//! Each capture is compared with `tests/golden/gui/<name>.png`
//! ([`TOLERANCE`]): a pixel differs when a colour channel moved by more than
//! 24; at most 0.5 % of the compared pixels may differ; and no 8 × 8 block's
//! mean luminance may move by more than 16, which catches missing or added
//! text that is too small a share of the image for the pixel fraction. On a
//! mismatch `<name>.actual.png` and `<name>.diff.png` (differing pixels red,
//! failing blocks outlined yellow, masks blue) are written next to the golden.
//! `--update` rewrites the goldens. They belong to the reference machine (its
//! GPU, fonts and DPI); elsewhere the sizes or the anti-aliasing may differ.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths::Instance;
use crate::selftest::{GuiDriver, Ui};

const COLS: u16 = 170;
const ROWS: u16 = 46;

/// How far a capture may be from its golden.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerance {
    /// A pixel differs when one of its colour channels moved by more than this.
    pub channel: u8,
    /// Share of the compared (unmasked) pixels that may differ.
    pub fraction: f64,
    /// Side of the square blocks of the luminance guard, in pixels.
    pub block: u32,
    /// Largest change of a block's mean luminance (0–255).
    pub block_luma: f64,
}

pub const TOLERANCE: Tolerance = Tolerance { channel: 24, fraction: 0.005, block: 8, block_luma: 16.0 };

/// A region in cells (fractional on a zoomed canvas) left out of the comparison.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mask {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub why: &'static str,
}

/// Pixel rectangle `x0, y0, x1, y1` (end exclusive).
pub type PxRect = (u32, u32, u32, u32);

/// RGBA pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().context("png size")?];
        let info = reader.next_frame(&mut buf)?;
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
            other => bail!("{}: unsupported PNG colour type {other:?}", path.display()),
        };
        Ok(Self { width: info.width, height: info.height, rgba })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&self.rgba)?;
        writer.finish()?;
        Ok(())
    }

    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }
}

/// What a comparison found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Comparison {
    /// Pixels compared (outside the masks).
    pub compared: u64,
    /// Pixels with a channel change over the tolerance.
    pub differing: u64,
    pub max_delta: u8,
    /// Largest change of a block's mean luminance, at that block's corner.
    pub worst_block: f64,
    pub worst_at: (u32, u32),
    /// Blocks over the luminance tolerance.
    pub failed_blocks: u32,
}

impl Comparison {
    pub fn fraction(&self) -> f64 {
        if self.compared == 0 { 0.0 } else { self.differing as f64 / self.compared as f64 }
    }

    pub fn passes(&self, t: &Tolerance) -> bool {
        self.fraction() <= t.fraction && self.failed_blocks == 0
    }

    fn describe(&self, t: &Tolerance) -> String {
        format!(
            "{} of {} px differ ({:.3} %, max channel Δ {}); worst {}×{} block Δ luminance {:.1} at px {},{}",
            self.differing,
            self.compared,
            self.fraction() * 100.0,
            self.max_delta,
            t.block,
            t.block,
            self.worst_block,
            self.worst_at.0,
            self.worst_at.1
        )
    }
}

fn masked(masks: &[PxRect], x: u32, y: u32) -> bool {
    masks.iter().any(|&(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1)
}

fn luma(p: [u8; 4]) -> f64 {
    0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64
}

/// Compares `actual` with `golden` outside `masks`, and draws the
/// differences: differing pixels red, failing blocks outlined yellow, masked
/// regions blue, the rest a dimmed grey copy of `actual`.
pub fn compare(golden: &Image, actual: &Image, masks: &[PxRect], t: &Tolerance) -> Result<(Comparison, Image)> {
    anyhow::ensure!(
        (golden.width, golden.height) == (actual.width, actual.height),
        "the capture is {}x{} px, the golden {}x{} px (another DPI, font or grid size?)",
        actual.width,
        actual.height,
        golden.width,
        golden.height
    );
    let (w, h) = (actual.width, actual.height);
    let mut c = Comparison::default();
    let mut diff = Image { width: w, height: h, rgba: vec![0; (w * h * 4) as usize] };
    for y in 0..h {
        for x in 0..w {
            let a = actual.px(x, y);
            let dim = (luma(a) * 0.35) as u8;
            let out = if masked(masks, x, y) {
                [dim / 2, dim / 2, dim.saturating_add(90), 255]
            } else {
                c.compared += 1;
                let g = golden.px(x, y);
                let delta = (0..3).map(|k| g[k].abs_diff(a[k])).max().unwrap_or(0);
                c.max_delta = c.max_delta.max(delta);
                if delta > t.channel {
                    c.differing += 1;
                    [255, 40, 40, 255]
                } else {
                    [dim, dim, dim, 255]
                }
            };
            let i = ((y * w + x) * 4) as usize;
            diff.rgba[i..i + 4].copy_from_slice(&out);
        }
    }
    let b = t.block.max(1);
    for by in (0..h).step_by(b as usize) {
        for bx in (0..w).step_by(b as usize) {
            let (x1, y1) = ((bx + b).min(w), (by + b).min(h));
            let (mut n, mut total, mut sum_g, mut sum_a) = (0u32, 0u32, 0.0, 0.0);
            for y in by..y1 {
                for x in bx..x1 {
                    total += 1;
                    if !masked(masks, x, y) {
                        n += 1;
                        sum_g += luma(golden.px(x, y));
                        sum_a += luma(actual.px(x, y));
                    }
                }
            }
            // Blocks mostly under a mask are left out.
            if n * 2 < total {
                continue;
            }
            let delta = ((sum_g - sum_a) / n as f64).abs();
            if delta > c.worst_block {
                c.worst_block = delta;
                c.worst_at = (bx, by);
            }
            if delta > t.block_luma {
                c.failed_blocks += 1;
                for y in by..y1 {
                    for x in bx..x1 {
                        if x == bx || y == by || x + 1 == x1 || y + 1 == y1 {
                            let i = ((y * w + x) * 4) as usize;
                            diff.rgba[i..i + 4].copy_from_slice(&[255, 210, 0, 255]);
                        }
                    }
                }
            }
        }
    }
    Ok((c, diff))
}

/// Identical outside the masks.
fn identical(a: &Image, b: &Image, masks: &[PxRect]) -> bool {
    (a.width, a.height) == (b.width, b.height) && (0..a.height).all(|y| (0..a.width).all(|x| masked(masks, x, y) || a.px(x, y) == b.px(x, y)))
}

fn mask_px(m: &Mask, (cell_w, cell_h): (f64, f64)) -> PxRect {
    (
        (m.x * cell_w).floor().max(0.0) as u32,
        (m.y * cell_h).floor().max(0.0) as u32,
        ((m.x + m.w) * cell_w).ceil().max(0.0) as u32,
        ((m.y + m.h) * cell_h).ceil().max(0.0) as u32,
    )
}

/// Hatches the masked regions (stored goldens show what is not compared).
fn blank(image: &mut Image, masks: &[PxRect]) {
    for &(x0, y0, x1, y1) in masks {
        for y in y0..y1.min(image.height) {
            for x in x0..x1.min(image.width) {
                let c = if (x + y) % 8 < 2 { [70, 76, 100, 255] } else { [40, 44, 58, 255] };
                let i = ((y * image.width + x) * 4) as usize;
                image.rgba[i..i + 4].copy_from_slice(&c);
            }
        }
    }
}

/// Where the goldens live: `CANVAS_GOLDEN_DIR`, else `tests/golden/gui` of
/// the repository this executable was built in (`target\release\…`), else
/// of the current folder.
fn golden_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CANVAS_GOLDEN_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let exe = std::env::current_exe()?;
    if let Some(repo) = exe.ancestors().skip(1).find(|d| d.join("Cargo.toml").is_file() && d.join("crates").is_dir()) {
        return Ok(repo.join("tests").join("golden").join("gui"));
    }
    Ok(std::env::current_dir()?.join("tests").join("golden").join("gui"))
}

struct Outcome {
    name: String,
    result: Result<String, String>,
}

/// The shots of one run: where captures go and how they are judged.
struct Shots {
    goldens: PathBuf,
    captures: PathBuf,
    update: bool,
    outcomes: Vec<Outcome>,
}

/// The regions a shot leaves out, found on the settled screen.
type Masks<'a> = &'a dyn Fn(&GuiDriver) -> Vec<Mask>;

impl Shots {
    fn shot(&mut self, ui: &GuiDriver, name: &str, masks: Masks) {
        let started = Instant::now();
        let result = self.take(ui, name, masks).map_err(|e| format!("{e:#}"));
        match &result {
            Ok(detail) => println!("  ✓ {name} ({:.1} s): {detail}", started.elapsed().as_secs_f64()),
            Err(error) => println!("  ✗ {name}: {error}"),
        }
        self.outcomes.push(Outcome { name: name.to_owned(), result });
    }

    fn failed(&mut self, name: &str, error: anyhow::Error) {
        println!("  ✗ {name}: {error:#}");
        self.outcomes.push(Outcome { name: name.to_owned(), result: Err(format!("{error:#}")) });
    }

    fn take(&self, ui: &GuiDriver, name: &str, masks: Masks) -> Result<String> {
        settle(ui)?;
        let masks = masks(ui);
        let size = ui.request(json!({ "cmd": "size" }))?;
        let cell = (size["cell_w"].as_f64().context("cell width")?, size["cell_h"].as_f64().context("cell height")?);
        let px: Vec<PxRect> = masks.iter().map(|m| mask_px(m, cell)).collect();
        let image = steady(ui, &self.captures, name, &px)?;
        let golden_path = self.goldens.join(format!("{name}.png"));
        let actual_path = self.goldens.join(format!("{name}.actual.png"));
        let diff_path = self.goldens.join(format!("{name}.diff.png"));
        let mut why: Vec<&str> = Vec::new();
        for m in &masks {
            if !why.contains(&m.why) {
                why.push(m.why);
            }
        }
        let masked = if why.is_empty() { String::new() } else { format!("; masked: {}", why.join(", ")) };
        if self.update {
            let mut stored = image.clone();
            blank(&mut stored, &px);
            stored.write(&golden_path)?;
            let _ = std::fs::remove_file(&actual_path);
            let _ = std::fs::remove_file(&diff_path);
            return Ok(format!("golden written ({}x{} px){masked}", image.width, image.height));
        }
        let golden = Image::read(&golden_path).with_context(|| format!("no golden for {name} (run with --update to create it)"))?;
        let (comparison, diff) = match compare(&golden, &image, &px, &TOLERANCE) {
            Ok(result) => result,
            Err(error) => {
                image.write(&actual_path)?;
                return Err(error.context(format!("capture kept in {}", actual_path.display())));
            }
        };
        let stats = comparison.describe(&TOLERANCE);
        if comparison.passes(&TOLERANCE) {
            let _ = std::fs::remove_file(&actual_path);
            let _ = std::fs::remove_file(&diff_path);
            return Ok(format!("{stats}{masked}"));
        }
        image.write(&actual_path)?;
        diff.write(&diff_path)?;
        bail!("{stats}; {} block(s) over Δ {}; see {} and {}", comparison.failed_blocks, TOLERANCE.block_luma, actual_path.display(), diff_path.display())
    }
}

/// Waits until no notice is up and the screen text (all but the status bar,
/// whose figures change) stayed the same for a second.
fn settle(ui: &GuiDriver) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let body = |text: String| text.lines().take(ROWS as usize - 1).collect::<Vec<_>>().join("\n");
    let mut last = body(ui.text());
    let mut since = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let notice = !tree(ui)?["notice"].is_null();
        let now = body(ui.text());
        if notice || now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= Duration::from_secs(1) {
            return Ok(());
        }
    }
    bail!("the screen does not settle (a notice or changing text)\n--- screen ---\n{}", ui.text())
}

/// Captures until two renders in a row are identical outside the masks.
fn steady(ui: &GuiDriver, dir: &Path, name: &str, masks: &[PxRect]) -> Result<Image> {
    let path = |i: usize| dir.join(format!("{name}-{}.png", i % 2));
    ui.request(json!({ "cmd": "png", "path": path(0) }))?;
    let mut last = Image::read(&path(0))?;
    for i in 1..16 {
        std::thread::sleep(Duration::from_millis(250));
        ui.request(json!({ "cmd": "png", "path": path(i) }))?;
        let next = Image::read(&path(i))?;
        if identical(&last, &next, masks) {
            return Ok(next);
        }
        last = next;
    }
    bail!("the frame does not settle: captures 250 ms apart keep differing")
}

fn tree(ui: &GuiDriver) -> Result<Value> {
    Ok(ui.request(json!({ "cmd": "tree" }))?["tree"].clone())
}

fn wait_tree(ui: &GuiDriver, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let t = tree(ui)?;
        if ok(&t) {
            return Ok(t);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}\n--- screen ---\n{}", ui.text());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn rect4(value: &Value) -> Option<[f64; 4]> {
    let r: Vec<f64> = value.as_array()?.iter().filter_map(Value::as_f64).collect();
    (r.len() == 4).then(|| [r[0], r[1], r[2], r[3]])
}

/// A canvas box's centre (screen cells), by name.
fn box_center(ui: &GuiDriver, name: &str) -> Result<(f64, f64)> {
    let t = tree(ui)?;
    let found = t["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name)).with_context(|| format!("no box {name:?}"))?;
    let r = rect4(&found["rect"]).context("box rect")?;
    Ok((r[0] + (r[2] / 2.0).min(4.0), r[1] + r[3] / 2.0))
}

fn mouse(ui: &GuiDriver, kind: &str, button: &str, (x, y): (f64, f64)) -> Result<()> {
    ui.request(json!({ "cmd": "mouse", "kind": kind, "button": button, "x": x, "y": y, "mods": [] })).map(|_| ())
}

fn click(ui: &GuiDriver, button: &str, at: (f64, f64)) -> Result<()> {
    mouse(ui, "down", button, at)?;
    mouse(ui, "up", button, at)
}

/// A tile's inner rect (cells), by terminal id.
fn tile_inner(t: &Value, id: &str) -> Option<[f64; 4]> {
    t["tiles"].as_array()?.iter().find(|tile| tile["id"] == id).and_then(|tile| rect4(&tile["inner"]))
}

/// The text of terminal `id`'s tile.
fn tile_text(ui: &GuiDriver, id: &str) -> Result<String> {
    let r = tile_inner(&tree(ui)?, id).with_context(|| format!("no tile {id}"))?;
    let (x, y, w, h) = (r[0] as usize, r[1] as usize, r[2] as usize, r[3] as usize);
    Ok(ui.text().lines().skip(y).take(h).map(|l| l.chars().skip(x).take(w).collect::<String>().trim_end().to_owned()).collect::<Vec<_>>().join("\n"))
}

fn wait_tile(ui: &GuiDriver, id: &str, what: &str, secs: u64, ok: impl Fn(&str) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(text) = tile_text(ui, id)
            && ok(&text)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what} in {id}\n--- screen ---\n{}", ui.text());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The service's figures at the status bar's right end (memory, live count, zoom).
fn status_figures() -> Mask {
    Mask { x: (COLS - 36) as f64, y: (ROWS - 1) as f64, w: 36.0, h: 1.0, why: "service figures" }
}

/// Cells `x..to` of row `y`.
fn row_from(x: u16, y: u16, to: u16, why: &'static str) -> Mask {
    Mask { x: x as f64, y: y as f64, w: to.saturating_sub(x) as f64, h: 1.0, why }
}

/// The selected session's folder in the right panel (inside the sandbox).
fn details_folder(ui: &GuiDriver) -> Option<Mask> {
    let (x, y) = ui.find_in("Folder   ", 90, COLS)?;
    Some(row_from(x + 9, y, COLS - 1, "sandbox folder (details)"))
}

/// The new-session dialog's folder field (inside the sandbox).
fn dialog_folder(ui: &GuiDriver) -> Option<Mask> {
    let (bx, by) = ui.find("Browse…")?;
    let line = ui.text().lines().nth(by as usize)?.to_owned();
    let byte = line.find("Folder")?;
    let fx = crate::client::paint::text_width(&line[..byte]) as u16;
    (fx + 15 < bx).then(|| row_from(fx + 15, by, bx - 1, "sandbox folder (dialog)"))
}

/// The third line of automation boxes: "daily at 08:30 · next 7d ago"
/// counts from today.
fn automation_subtitles(ui: &GuiDriver) -> Result<Vec<Mask>> {
    let t = tree(ui)?;
    let boxes = t["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
    Ok(boxes
        .iter()
        .filter(|b| b["prefix"] == "◷ ")
        .filter_map(|b| rect4(&b["rect"]))
        // The GUI puts a box's three labels at about 0.2, 0.45 and 0.7 of its height.
        .map(|[x, y, w, h]| Mask { x, y: y + h * 0.6, w, h: h * 0.3, why: "automation next-run times" })
        .collect())
}

/// Terminal A: colours, styles, underline kinds, emoji, CJK, block elements.
/// ASCII only (characters are built from code points); clears the screen
/// first and leaves a fixed prompt.
const SCRIPT_A: &str = r#"$e = [char]27
Clear-Host
$Host.UI.RawUI.WindowTitle = 'Golden A'
Write-Host 'GOLDEN-A colours, styles, emoji, CJK'
Write-Host ("$e[31mred $e[32mgreen $e[33myellow $e[34mblue $e[35mmagenta $e[36mcyan $e[90mgray$e[0m")
Write-Host ("$e[30;41m red $e[0m $e[30;42m green $e[0m $e[97;44m blue $e[0m $e[48;5;208m 208 $e[0m $e[38;2;255;128;0mrgb 255,128,0$e[0m")
Write-Host ("$e[1mbold$e[0m $e[2mdim$e[0m $e[3mitalic$e[0m $e[7minverse$e[0m $e[9mstrike$e[0m $e[53moverline$e[0m")
Write-Host ("$e[4msingle$e[0m $e[4:2mdouble$e[0m $e[4:3mcurly$e[0m $e[4:4mdotted$e[0m $e[4:5mdashed$e[0m $e[4;58;2;255;64;64mred line$e[0m")
Write-Host ('emoji: ' + [char]::ConvertFromUtf32(0x1F600) + ' ' + [char]::ConvertFromUtf32(0x1F680) + ' ' + [char]::ConvertFromUtf32(0x2705) + ' ' + [char]::ConvertFromUtf32(0x1F44D) + ' |')
Write-Host ('CJK:   ' + (-join [char[]](0x4E2D, 0x6587, 0x6E2C, 0x8A66)) + ' ' + (-join [char[]](0x65E5, 0x672C, 0x8A9E)) + ' ' + (-join [char[]](0xD55C, 0xAD6D, 0xC5B4)) + ' |')
Write-Host ('boxes: ' + (-join [char[]](0x250C, 0x2500, 0x252C, 0x2500, 0x2510, 0x20, 0x2588, 0x2593, 0x2592, 0x2591, 0x20, 0x256D, 0x2500, 0x256E)))
Write-Host 'GOLDEN-A-END'
function global:prompt { 'PS> ' }
"#;

/// Terminal B: the 256-colour palette and bold status words.
const SCRIPT_B: &str = r#"$e = [char]27
Clear-Host
$Host.UI.RawUI.WindowTitle = 'Golden B'
Write-Host 'GOLDEN-B palette'
$bar = ''; foreach ($i in 0..15) { $bar += "$e[48;5;${i}m   " }; Write-Host ($bar + "$e[0m")
$bar = ''; foreach ($i in 16..51) { $bar += "$e[48;5;${i}m " }; Write-Host ($bar + "$e[0m")
$bar = ''; foreach ($i in 232..255) { $bar += "$e[48;5;${i}m  " }; Write-Host ($bar + "$e[0m")
Write-Host ("$e[1;32mPASS$e[0m parser  $e[1;31mFAIL$e[0m renderer  $e[1;33mWARN$e[0m fonts")
Write-Host 'The quick brown fox jumps over the lazy dog. 0123456789'
Write-Host 'GOLDEN-B-END'
function global:prompt { 'PS> ' }
"#;

/// The demo canvas in an isolated home; agents' data folders are empty, so
/// nothing of the machine shows.
struct DemoSandbox {
    root: PathBuf,
    instance: Instance,
    work: PathBuf,
}

impl DemoSandbox {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("copilot-canvas-golden-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        for dir in [&work, &root.join("sessions"), &root.join("copilot-home"), &root.join("xdg-data"), &root.join("claude-home")] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(work.join("golden-a.ps1"), SCRIPT_A)?;
        std::fs::write(work.join("golden-b.ps1"), SCRIPT_B)?;
        let instance = Instance::new(Some(format!("golden{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
        Ok(Self { root, instance, work })
    }

    fn start(&self) -> Result<GuiDriver> {
        let args: Vec<String> = vec![
            "--demo".into(),
            "--instance".into(),
            self.instance.name.clone(),
            "--home".into(),
            self.instance.home.display().to_string(),
            "--sessions-dir".into(),
            self.instance.sessions_dir.display().to_string(),
            "--cwd".into(),
            self.work.display().to_string(),
        ];
        let (copilot, xdg, claude) = (self.root.join("copilot-home"), self.root.join("xdg-data"), self.root.join("claude-home"));
        // The default agent for new sessions, whatever is installed here.
        let agent = PathBuf::from("copilot");
        let env: [(&str, &Path); 4] = [("COPILOT_HOME", &copilot), ("XDG_DATA_HOME", &xdg), ("CLAUDE_CONFIG_DIR", &claude), ("COPILOT_CANVAS_AGENT", &agent)];
        GuiDriver::start(&args, &env, (COLS, ROWS), &self.root.join("gui-golden.log"))
    }
}

impl Drop for DemoSandbox {
    fn drop(&mut self) {
        crate::parity::stop_service(&self.instance);
        remove(&self.root);
    }
}

fn remove(dir: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The demo canvas: idle, at 50 %, a context menu, the new-session dialog,
/// then two tiled shells with fixed output.
fn demo_shots(shots: &mut Shots) -> Result<()> {
    let sb = DemoSandbox::new()?;
    let mut ui = sb.start()?;
    ui.wait("the demo canvas", Duration::from_secs(30), |t| t.contains("Build session canvas") && t.contains("Canvas project"))?;
    ui.request(json!({ "cmd": "freeze", "tick": 0 }))?;
    let canvas = |ui: &GuiDriver| -> Vec<Mask> { [Some(status_figures()), details_folder(ui)].into_iter().flatten().collect() };

    shots.shot(&ui, "demo-canvas", &canvas);

    ui.send(b"-")?;
    wait_tree(&ui, "zoom 50 %", 5, |t| t["canvas"]["scale"].as_f64() == Some(0.5))?;
    shots.shot(&ui, "demo-zoom-50", &canvas);
    ui.send(b"+")?;
    wait_tree(&ui, "zoom 100 %", 5, |t| t["canvas"]["scale"].as_f64() == Some(1.0))?;

    let at = box_center(&ui, "Build session canvas")?;
    click(&ui, "right", at)?;
    ui.wait("the box's context menu", Duration::from_secs(5), |t| t.contains("New subsession") && t.contains("Remove from canvas"))?;
    shots.shot(&ui, "demo-context-menu", &canvas);
    ui.send(b"\x1b")?;
    ui.wait("the menu closed", Duration::from_secs(5), |t| !t.contains("Remove from canvas"))?;

    ui.send(b"n")?;
    ui.wait("the new-session dialog", Duration::from_secs(5), |t| t.contains("New session") && t.contains("Agent"))?;
    ui.wait("the Git probe", Duration::from_secs(10), |t| t.contains("Not a Git repository"))?;
    shots.shot(&ui, "demo-new-session-dialog", &|ui: &GuiDriver| [canvas(ui), dialog_folder(ui).into_iter().collect()].concat());
    ui.send(b"\x1b")?;
    ui.wait("the dialog closed", Duration::from_secs(5), |t| !t.contains("Not a Git repository"))?;

    // Two shells, tiled, then each prints its fixed text (after the layout
    // is final, so nothing is reflowed).
    ui.send(b"\r")?;
    wait_tile(&ui, "demo-main", "a PowerShell prompt", 30, |t| t.contains("PS "))?;
    ui.send(b"\x1d")?;
    let at = box_center(&ui, "Review implementation")?;
    // A double-click starts it (a click only views it).
    click(&ui, "left", at)?;
    click(&ui, "left", at)?;
    wait_tile(&ui, "demo-review", "a PowerShell prompt", 30, |t| t.contains("PS "))?;
    ui.send(b"\x1d")?;
    ui.send(b"t")?;
    wait_tree(&ui, "two tiles", 5, |t| t["tiles"].as_array().is_some_and(|tiles| tiles.len() == 2))?;
    for (id, script, end) in [("demo-main", "golden-a.ps1", "GOLDEN-A-END"), ("demo-review", "golden-b.ps1", "GOLDEN-B-END")] {
        let r = tile_inner(&tree(&ui)?, id).with_context(|| format!("no tile {id}"))?;
        click(&ui, "left", (r[0] + r[2] / 2.0, r[1] + r[3] - 1.5))?;
        wait_tree(&ui, &format!("{id} focused"), 5, |t| t["focus"] == "Terminal" && t["focused_term"] == id)?;
        ui.send(format!("iex (Get-Content -Raw {script})\r").as_bytes())?;
        wait_tile(&ui, id, "the fixed output and prompt", 20, |t| t.contains(end) && t.trim_end().ends_with("PS>"))?;
    }
    shots.shot(&ui, "terminals-tiled", &|_: &GuiDriver| vec![status_figures()]);

    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(())
}

/// The parity sandbox's canvas: every status from synthetic histories, a
/// subagent, loops, a group with folder chips and the automations group.
fn status_shots(shots: &mut Shots) -> Result<()> {
    let sb = crate::parity::Sandbox::new()?;
    let result = (|| -> Result<()> {
        let gui = sb.start(&[], "gui-golden.log")?;
        let mut ui = gui.driver;
        ui.wait("the parity canvas", Duration::from_secs(30), |t| t.contains("Parity group"))?;
        ui.request(json!({ "cmd": "freeze", "tick": 0 }))?;
        // No selection (no details panel), everything in view.
        ui.send(b"\x1b")?;
        ui.send(b"f")?;
        let expect = [
            ("Working one", "working"),
            ("Needs you one", "needs you"),
            ("Review me", "done · review"),
            ("Idle one", "idle"),
            ("Stopped one", "stopped"),
            ("Failed one", "failed"),
            ("Elsewhere", "open elsewhere"),
        ];
        wait_tree(&ui, "every status, the subagent, the loop and the automations", 30, |t| {
            let boxes = t["canvas"]["boxes"].as_array().cloned().unwrap_or_default();
            let status = |name: &str| boxes.iter().find(|b| b["name"] == name).map(|b| b["status"].as_str().unwrap_or_default().to_owned());
            t["selection"].as_array().is_some_and(Vec::is_empty)
                && expect.iter().all(|(name, label)| status(name).as_deref() == Some(*label))
                && boxes.iter().any(|b| b["name"] == "Parity explorer")
                && boxes.iter().any(|b| b["name"] == "Looping" && !b["loop"].is_null())
                && boxes.iter().filter(|b| b["prefix"] == "◷ ").count() == 3
        })?;
        shots.shot(&ui, "all-statuses", &|ui: &GuiDriver| [vec![status_figures()], automation_subtitles(ui).unwrap_or_default()].concat());
        let _ = ui.request(json!({ "cmd": "quit" }));
        let _ = ui.wait_exit(Duration::from_secs(10));
        Ok(())
    })();
    crate::parity::stop_service(&sb.instance);
    remove(&sb.root);
    result
}

pub fn run(update: bool) -> Result<()> {
    let goldens = golden_dir()?;
    let captures = std::env::temp_dir().join(format!("copilot-canvas-golden-captures-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&captures)?;
    let t = TOLERANCE;
    println!(
        "Copilot Canvas GUI golden screenshots ({COLS}x{ROWS} cells; goldens in {}; {})",
        goldens.display(),
        if update {
            "updating them".to_owned()
        } else {
            format!("tolerance: channel Δ ≤ {}, ≤ {:.1} % of pixels, {}×{} block luminance Δ ≤ {}", t.channel, t.fraction * 100.0, t.block, t.block, t.block_luma)
        }
    );
    let started = Instant::now();
    let mut shots = Shots { goldens, captures: captures.clone(), update, outcomes: Vec::new() };
    if let Err(error) = demo_shots(&mut shots) {
        shots.failed("demo canvas session", error);
    }
    if let Err(error) = status_shots(&mut shots) {
        shots.failed("all-statuses session", error);
    }
    remove(&captures);
    println!();
    println!("{:<26} | result", "shot");
    println!("{}", "-".repeat(100));
    for o in &shots.outcomes {
        match &o.result {
            Ok(detail) => println!("{:<26} | ✓ {detail}", o.name),
            Err(error) => println!("{:<26} | ✗ {}", o.name, error.lines().next().unwrap_or_default()),
        }
    }
    let failed = shots.outcomes.iter().filter(|o| o.result.is_err()).count();
    anyhow::ensure!(failed == 0, "{failed} of {} golden screenshot(s) failed", shots.outcomes.len());
    println!("PASS: {} golden screenshots in {:.1} s", shots.outcomes.len(), started.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dark image, optionally with a light "word" (a filled rectangle).
    fn image(w: u32, h: u32, word: Option<(u32, u32, u32, u32)>, bg: [u8; 3]) -> Image {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let inside = word.is_some_and(|(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1);
                let c = if inside { [200, 205, 210] } else { bg };
                rgba.extend([c[0], c[1], c[2], 255]);
            }
        }
        Image { width: w, height: h, rgba }
    }

    #[test]
    fn identical_and_slightly_shifted_colours_pass() {
        let golden = image(200, 100, Some((20, 20, 60, 36)), [13, 17, 23]);
        let (c, _) = compare(&golden, &golden, &[], &TOLERANCE).unwrap();
        assert_eq!((c.differing, c.failed_blocks, c.worst_block), (0, 0, 0.0));
        // Anti-aliasing or driver noise: every channel off by up to 20.
        let noisy = image(200, 100, Some((20, 20, 60, 36)), [33, 30, 40]);
        let (c, _) = compare(&golden, &noisy, &[], &TOLERANCE).unwrap();
        assert!(c.passes(&TOLERANCE), "{c:?}");
        assert_eq!(c.max_delta, 20);
    }

    #[test]
    fn a_missing_word_fails_the_block_guard_even_under_the_pixel_fraction() {
        let golden = image(400, 300, Some((40, 40, 56, 56)), [13, 17, 23]);
        let actual = image(400, 300, None, [13, 17, 23]);
        let (c, diff) = compare(&golden, &actual, &[], &TOLERANCE).unwrap();
        assert!(c.fraction() < TOLERANCE.fraction, "only {:.3} % of the pixels", c.fraction() * 100.0);
        assert!(c.failed_blocks >= 4 && !c.passes(&TOLERANCE), "{c:?}");
        assert_eq!(diff.px(45, 45), [255, 40, 40, 255], "differing pixels are red");
        assert_eq!(diff.px(40, 40), [255, 210, 0, 255], "failing blocks are outlined");
    }

    #[test]
    fn a_global_colour_change_fails_the_pixel_fraction() {
        let golden = image(100, 50, None, [13, 17, 23]);
        let actual = image(100, 50, None, [13, 17, 60]);
        let (c, _) = compare(&golden, &actual, &[], &TOLERANCE).unwrap();
        assert!(c.fraction() > 0.99 && !c.passes(&TOLERANCE), "{c:?}");
    }

    #[test]
    fn masks_hide_changes_and_sizes_must_match() {
        let golden = image(100, 50, Some((10, 10, 30, 20)), [13, 17, 23]);
        let actual = image(100, 50, Some((60, 10, 90, 20)), [13, 17, 23]);
        let masks = [(8, 8, 32, 22), (56, 8, 96, 24)];
        let (c, diff) = compare(&golden, &actual, &masks, &TOLERANCE).unwrap();
        assert!(c.passes(&TOLERANCE) && c.differing == 0, "{c:?}");
        assert_eq!(c.compared, 100 * 50 - 24 * 14 - 40 * 16);
        assert!(diff.px(70, 15)[2] > diff.px(70, 15)[0], "masked regions are blue");
        assert!(identical(&golden, &actual, &masks) && !identical(&golden, &actual, &[]));
        assert!(compare(&golden, &image(100, 51, None, [0, 0, 0]), &[], &TOLERANCE).is_err());
    }

    #[test]
    fn masks_cover_whole_cells_in_pixels_and_are_hatched_in_goldens() {
        let m = Mask { x: 2.0, y: 3.0, w: 4.0, h: 1.0, why: "test" };
        assert_eq!(mask_px(&m, (10.5, 21.0)), (21, 63, 63, 84));
        let half = Mask { x: 1.25, y: 0.5, w: 0.5, h: 0.25, why: "test" };
        assert_eq!(mask_px(&half, (10.0, 20.0)), (12, 10, 18, 15), "fractional cells round outwards");
        let mut img = image(8, 4, None, [0, 0, 0]);
        blank(&mut img, &[(0, 0, 4, 2)]);
        assert_eq!((img.px(0, 0), img.px(2, 1), img.px(5, 3)), ([70, 76, 100, 255], [40, 44, 58, 255], [0, 0, 0, 255]));
    }

    #[test]
    fn images_round_trip_through_png() {
        let dir = std::env::temp_dir().join(format!("cc-golden-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("a.png");
        let img = image(7, 5, Some((1, 1, 3, 3)), [1, 2, 3]);
        img.write(&path).unwrap();
        assert_eq!(Image::read(&path).unwrap(), img);
        let _ = std::fs::remove_dir_all(dir);
    }
}
