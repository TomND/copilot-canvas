//! `copilot-canvas selftest --client gui --hardening`: the GUI's Phase 6
//! checks over its test hooks, each with ✓ or ✗ and its reason:
//! the toolbar's status counts at 170 columns, overlapping boxes, GPU device
//! loss, a DPI change, window placement across restarts (and a monitor that
//! is gone), the UI Automation tree as a UIA client sees it, the crash log
//! (UI and helper threads), the software-renderer fallback, damage frames.
//! `CANVAS_GUI_SCREENS=DIR` keeps the PNGs.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::doc::{Doc, GRID_X, GRID_Y, Group, Node, NodeKind};
use crate::paths::Instance;
use crate::selftest::{GuiDriver, Ui};

struct Sandbox {
    root: PathBuf,
    instance: Instance,
    work: PathBuf,
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
        agent_session: None,
    }
}

fn event(kind: &str, at_ms: u64, data: Value) -> String {
    json!({ "type": kind, "timestamp": crate::paths::format_rfc3339_ms(at_ms), "data": data }).to_string()
}

impl Sandbox {
    fn new(name: &str) -> Result<Self> {
        let root = std::env::temp_dir().join(format!("copilot-canvas-hardening-{name}-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(root.join("sessions"))?;
        let instance = Instance::new(Some(format!("hard{name}{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
        std::fs::create_dir_all(&instance.home)?;
        Ok(Self { root, instance, work })
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

    fn canvas(&self, doc: &Doc) -> Result<String> {
        let path = self.instance.home.join("canvas.json");
        std::fs::write(&path, serde_json::to_string_pretty(doc)?)?;
        Ok(path.display().to_string())
    }

    fn start(&self, extra: &[&str], env: &[(&str, &Path)], size: Option<(u16, u16)>, log: &str) -> Result<GuiDriver> {
        GuiDriver::start_sized(&self.args(extra), env, size, &self.root.join(log))
    }

    fn crash_log(&self) -> String {
        std::fs::read_to_string(self.instance.home.join("crash.log")).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(conn) = crate::client::conn::try_connect(&self.instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && conn.rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
        }
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn screens_dir() -> PathBuf {
    std::env::var_os("CANVAS_GUI_SCREENS").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("copilot-canvas-hardening-screens"))
}

fn png(ui: &GuiDriver, name: &str) -> Result<PathBuf> {
    let path = screens_dir().join(format!("{name}.png"));
    ui.request(json!({ "cmd": "png", "path": path }))?;
    Ok(path)
}

/// RGBA pixels and size of a PNG file.
fn pixels(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("png size")?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// Pixels that differ between two captures (`None`: different sizes).
fn differing(a: &Path, b: &Path) -> Result<Option<usize>> {
    let (pa, pb) = (pixels(a)?, pixels(b)?);
    if (pa.0, pa.1) != (pb.0, pb.1) {
        return Ok(None);
    }
    Ok(Some(pa.2.chunks(4).zip(pb.2.chunks(4)).filter(|(x, y)| x != y).count()))
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

struct Check {
    name: &'static str,
    result: Result<String, String>,
}

fn check(checks: &mut Vec<Check>, name: &'static str, body: impl FnOnce() -> Result<String>) {
    let started = Instant::now();
    println!("  · {name} …");
    let result = body().map_err(|e| format!("{e:#}"));
    match &result {
        Ok(detail) => println!("    ✓ {detail} ({:.1} s)", started.elapsed().as_secs_f64()),
        Err(error) => println!("    ✗ {error}"),
    }
    checks.push(Check { name, result });
}

pub fn run() -> Result<()> {
    println!("Copilot Canvas GUI hardening checks (test hooks; PNGs in {})", screens_dir().display());
    let started = Instant::now();
    let mut checks = Vec::new();
    check(&mut checks, "toolbar_counts_never_cover_buttons_at_170_columns", toolbar_counts);
    check(&mut checks, "overlapping_box_covers_the_label_below", overlapping_boxes);
    check(&mut checks, "device_lost_rebuilds_and_renders_the_same", device_lost);
    check(&mut checks, "dpi_change_relayouts_and_keeps_the_canvas_centred", dpi_change);
    check(&mut checks, "window_placement_restored_and_clamped", placement);
    check(&mut checks, "ui_automation_tree_seen_by_a_uia_client", uia_tree);
    check(&mut checks, "crash_log_ui_and_helper_threads", crash_log);
    check(&mut checks, "software_renderer_fallback_and_clear_error", software_fallback);
    check(&mut checks, "damage_frames_while_terminals_stream", damage_frames);
    println!();
    println!("| Check | Result |");
    println!("|---|---|");
    for c in &checks {
        match &c.result {
            Ok(detail) => println!("| `{}` | ✓ {} |", c.name, detail.replace('|', "/")),
            Err(error) => println!("| `{}` | ✗ {} |", c.name, error.lines().next().unwrap_or_default().replace('|', "/")),
        }
    }
    let failed = checks.iter().filter(|c| c.result.is_err()).count();
    if failed > 0 {
        bail!("{failed} of {} hardening checks failed", checks.len());
    }
    println!("PASS: {} hardening checks in {:.0} s", checks.len(), started.elapsed().as_secs_f64());
    Ok(())
}

/// Sessions in every status that shows up in the toolbar's counts.
fn busy_canvas(sb: &Sandbox) -> Result<String> {
    let now = crate::paths::now_ms();
    let dir = &sb.instance.sessions_dir;
    let mut doc = Doc::default();
    let start = |at| event("session.start", at, json!({}));
    let user = |at| event("user.message", at, json!({ "content": "hardening" }));
    let turn = |at| event("assistant.turn_start", at, json!({}));
    let sessions: [(&str, Vec<String>, bool); 8] = [
        ("Working one", vec![start(now - 9000), user(now - 8000), turn(now - 7000), event("tool.execution_start", now - 6000, json!({ "toolCallId": "t1", "toolName": "powershell" }))], true),
        ("Working two", vec![start(now - 9000), user(now - 8000), turn(now - 7000), event("tool.execution_start", now - 6000, json!({ "toolCallId": "t2", "toolName": "powershell" }))], true),
        ("Needs you", vec![start(now - 9000), user(now - 8000), turn(now - 7000), event("permission.requested", now - 6000, json!({ "requestId": "p1" }))], true),
        ("Review A", vec![start(now - 9000), user(now - 8000), turn(now - 7000), event("assistant.turn_end", now - 6000, json!({})), event("session.idle", now - 5000, json!({}))], true),
        ("Review B", vec![start(now - 9000), user(now - 8000), turn(now - 7000), event("assistant.turn_end", now - 6000, json!({})), event("session.idle", now - 5000, json!({}))], true),
        ("Elsewhere", vec![], true),
        ("Failed one", vec![start(now - 9000), event("session.error", now - 8000, json!({ "message": "boom" }))], false),
        ("Idle one", vec![start(now - 7_200_000), user(now - 7_100_000), event("session.idle", now - 7_000_000, json!({}))], true),
    ];
    for (i, (name, events, live)) in sessions.iter().enumerate() {
        let id = uuid::Uuid::new_v4().to_string();
        let folder = dir.join(&id);
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("workspace.yaml"), format!("id: {id}\nname: {name}\ncwd: {}\n", sb.work.display()))?;
        if !events.is_empty() {
            std::fs::write(folder.join("events.jsonl"), events.join("\n") + "\n")?;
        }
        if *live {
            std::fs::write(folder.join(format!("inuse.{}.lock", std::process::id())), "hardening")?;
        }
        let mut n = node(&id, name, 2 + (i as i32 % 3) * (GRID_X + 2), 2 + (i as i32 / 3) * (GRID_Y + 2), &sb.work);
        n.reviewed_ms = Some(if name.starts_with("Review") { now - 600_000 } else { now });
        doc.nodes.push(n);
    }
    sb.canvas(&doc)
}

fn toolbar_counts() -> Result<String> {
    let sb = Sandbox::new("toolbar")?;
    let canvas = busy_canvas(&sb)?;
    let mut ui = sb.start(&["--canvas", &canvas], &[], Some((170, 46)), "toolbar.log")?;
    ui.wait("the counts", Duration::from_secs(30), |t| t.lines().next().is_some_and(|l| l.contains("needs you") && l.contains("working")))?;
    std::thread::sleep(Duration::from_secs(2));
    let top = ui.text().lines().next().unwrap_or_default().to_owned();
    png(&ui, "toolbar-170-columns")?;
    let quit = top.find(" ⏻ Quit ").context(format!("the Quit button is not intact: {top:?}"))?;
    let after = &top[quit + " ⏻ Quit ".len()..];
    anyhow::ensure!(after.starts_with("  ") || after.trim().is_empty(), "the counts start right after the buttons: {top:?}");
    anyhow::ensure!(!top.contains("Qui⠋") && !top.contains("⧉ ⠋"), "counts overwrite a button: {top:?}");
    let counts = after.trim().to_owned();
    anyhow::ensure!(counts.contains("needs you"), "needs you is kept: {counts:?}");
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(format!("buttons intact, counts after them: {counts:?}"))
}

/// A static canvas: idle boxes in a group, two of them overlapping.
fn static_canvas(sb: &Sandbox) -> Result<String> {
    let mut doc = Doc::default();
    doc.nodes.push(node("lower", "Lower box with a long label that runs under", 4, 3, &sb.work));
    doc.nodes.push(node("upper", "Upper box on top", 14, 4, &sb.work));
    doc.nodes.push(node("third", "A third box", 4, 12, &sb.work));
    doc.nodes.push(node("fourth", "Fourth box", 40, 12, &sb.work));
    doc.groups.push(Group { id: "g".into(), name: "Hardening group".into(), x: 1, y: 1, w: 80, h: 20, color: 2, folder: None, source: None, defaults: None });
    doc.view.x = -2;
    doc.view.y = -1;
    sb.canvas(&doc)
}

fn overlapping_boxes() -> Result<String> {
    let sb = Sandbox::new("overlap")?;
    let canvas = static_canvas(&sb)?;
    let mut ui = sb.start(&["--canvas", &canvas], &[], Some((170, 46)), "overlap.log")?;
    let t = wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.len() >= 4))?;
    let level = |id: &str| t["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|x| x["id"] == id)).and_then(|b| b["level"].as_u64());
    let (lower, upper, third) = (level("lower"), level("upper"), level("third"));
    png(&ui, "overlapping-boxes")?;
    anyhow::ensure!(upper > lower, "the upper box is drawn above the lower one's label (levels {lower:?} / {upper:?})");
    anyhow::ensure!(third == Some(0), "a box overlapping nothing stays at the bottom level ({third:?})");
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(format!("box levels: lower {}, upper {}; overlapping-boxes.png", lower.unwrap_or(0), upper.unwrap_or(0)))
}

/// Two captures in a row are identical (nothing animates).
fn steady(ui: &GuiDriver, name: &str) -> Result<PathBuf> {
    let mut last = png(ui, &format!("{name}-a"))?;
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(400));
        let next = png(ui, &format!("{name}-{}", if i % 2 == 0 { "b" } else { "a" }))?;
        if differing(&last, &next)? == Some(0) {
            return Ok(next);
        }
        last = next;
    }
    bail!("the scene does not settle (two captures differ)")
}

fn device_lost() -> Result<String> {
    let sb = Sandbox::new("device")?;
    let canvas = static_canvas(&sb)?;
    let mut ui = sb.start(&["--canvas", &canvas], &[], Some((170, 46)), "device.log")?;
    wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.len() >= 4))?;
    let before = steady(&ui, "device-before")?;
    let lost = ui.request(json!({ "cmd": "device-lost" }))?;
    let after = png(&ui, "device-after")?;
    let diff = differing(&before, &after)?;
    anyhow::ensure!(diff == Some(0), "the frame after rebuilding differs ({diff:?} pixels)");
    // A real loss: the device is destroyed and the driver reports it.
    let destroyed = ui.request(json!({ "cmd": "device-lost", "destroy": true }))?;
    ui.request(json!({ "cmd": "redraw" }))?;
    let again = png(&ui, "device-after-destroy")?;
    let diff2 = differing(&before, &again)?;
    anyhow::ensure!(diff2 == Some(0), "the frame after a destroyed device differs ({diff2:?} pixels)");
    let perf = ui.request(json!({ "cmd": "perf" }))?;
    anyhow::ensure!(perf["gpu_recoveries"].as_u64() == Some(2), "two recoveries counted: {}", perf["gpu_recoveries"]);
    // Frames are presented again.
    let presented = perf["perf"]["presented"].as_u64().unwrap_or(0);
    ui.request(json!({ "cmd": "redraw" }))?;
    let later = ui.request(json!({ "cmd": "perf" }))?["perf"]["presented"].as_u64().unwrap_or(0);
    anyhow::ensure!(later > presented, "no frame presented after the recovery");
    let log = sb.crash_log();
    anyhow::ensure!(log.contains("rebuilding the GPU objects") && log.contains("[adapter: "), "the crash log notes the rebuild with the adapter:\n{log}");
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(format!("rebuilt in {} ms and {} ms (destroyed device); PNGs before/after identical", lost["ms"], destroyed["ms"]))
}

fn dpi_change() -> Result<String> {
    let sb = Sandbox::new("dpi")?;
    let canvas = static_canvas(&sb)?;
    let mut ui = sb.start(&["--canvas", &canvas], &[], Some((170, 46)), "dpi.log")?;
    wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.len() >= 4))?;
    let before = steady(&ui, "dpi-before")?;
    let size = ui.request(json!({ "cmd": "size" }))?;
    let scale = size["scale"].as_f64().unwrap_or(1.0);
    let center = |t: &Value| {
        let c = &t["canvas"];
        let (s, ox, oy) = (c["scale"].as_f64().unwrap_or(1.0), c["origin"][0].as_f64().unwrap_or(0.0), c["origin"][1].as_f64().unwrap_or(0.0));
        let (w, h) = (c["area"][2].as_f64().unwrap_or(0.0), c["area"][3].as_f64().unwrap_or(0.0));
        (ox + w / s / 2.0, oy + h / s / 2.0)
    };
    let c0 = center(&tree(&ui)?);
    let bigger = scale * 1.5;
    let changed = ui.request(json!({ "cmd": "dpi", "scale": bigger }))?;
    std::thread::sleep(Duration::from_millis(500));
    let scaled = png(&ui, "dpi-150pct")?;
    let (w, h, _) = pixels(&scaled)?;
    let (cw, ch) = (changed["cell_w"].as_f64().unwrap_or(0.0), changed["cell_h"].as_f64().unwrap_or(0.0));
    anyhow::ensure!(cw > size["cell_w"].as_f64().unwrap_or(0.0), "the cells grew: {changed}");
    anyhow::ensure!((w as f64 - 170.0 * cw).abs() < 2.0 && (h as f64 - 46.0 * ch).abs() < 2.0, "a 170 × 46 grid of the new cells ({w} × {h})");
    let c1 = center(&tree(&ui)?);
    anyhow::ensure!((c0.0 - c1.0).abs() < 0.05 && (c0.1 - c1.1).abs() < 0.05, "the canvas stays centred: {c0:?} → {c1:?}");
    ui.request(json!({ "cmd": "dpi", "scale": scale }))?;
    std::thread::sleep(Duration::from_millis(500));
    let back = png(&ui, "dpi-back")?;
    let diff = differing(&before, &back)?;
    anyhow::ensure!(diff == Some(0), "back at the first DPI the frame differs ({diff:?} pixels)");
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(format!("{scale} → {bigger}: cells {} → {cw} px, {w} × {h} px, centre kept; back to {scale}: identical frame", size["cell_w"]))
}

fn placement() -> Result<String> {
    let sb = Sandbox::new("place")?;
    let canvas = static_canvas(&sb)?;
    let start = |log: &str| sb.start(&["--canvas", &canvas], &[], None, log);
    let state = |ui: &GuiDriver| ui.request(json!({ "cmd": "placement" }));
    // Move and size the window, then restart.
    let mut ui = start("place1.log")?;
    let first = state(&ui)?;
    let work = first["monitors"].as_array().and_then(|m| m.iter().find(|m| m["primary"] == true).or(m.first())).map(|m| m["work"].clone()).context("a monitor")?;
    let (wx, wy) = (work[0].as_i64().unwrap_or(0), work[1].as_i64().unwrap_or(0));
    let (x, y) = (wx + 137, wy + 91);
    ui.request(json!({ "cmd": "place", "x": x, "y": y, "width": 1280, "height": 760 }))?;
    std::thread::sleep(Duration::from_millis(1800));
    let moved = state(&ui)?;
    anyhow::ensure!(moved["saved"]["x"] == moved["tracked"]["x"] && moved["saved"]["width"] == moved["tracked"]["width"], "the placement was saved: {moved}");
    ui.request(json!({ "cmd": "quit" }))?;
    ui.wait_exit(Duration::from_secs(10))?;
    let mut ui = start("place2.log")?;
    let restored = state(&ui)?;
    anyhow::ensure!(restored["position"] == moved["position"] && restored["size"] == moved["size"], "restored where it was: {} vs {}", restored, moved);
    // Maximized survives a restart too.
    ui.request(json!({ "cmd": "place", "maximized": true }))?;
    std::thread::sleep(Duration::from_millis(1800));
    ui.request(json!({ "cmd": "quit" }))?;
    ui.wait_exit(Duration::from_secs(10))?;
    let mut ui = start("place3.log")?;
    let maximized = state(&ui)?;
    anyhow::ensure!(maximized["maximized"] == true, "restored maximized: {maximized}");
    anyhow::ensure!(maximized["saved"]["x"] == moved["saved"]["x"], "the normal placement is kept while maximized: {maximized}");
    ui.request(json!({ "cmd": "quit" }))?;
    ui.wait_exit(Duration::from_secs(10))?;
    // Its monitor is gone and the position is off every screen: clamped.
    let path = PathBuf::from(maximized["path"].as_str().context("placement path")?);
    std::fs::write(&path, json!({ "x": -40000, "y": -40000, "width": 1200, "height": 700, "maximized": false, "monitor": r"\\.\DISPLAY99" }).to_string())?;
    let mut ui = start("place4.log")?;
    let clamped = state(&ui)?;
    let (px, py) = (clamped["position"][0].as_i64().unwrap_or(-99999), clamped["position"][1].as_i64().unwrap_or(-99999));
    let on_screen = clamped["monitors"].as_array().into_iter().flatten().any(|m| {
        let w = &m["work"];
        let (x0, y0, x1, y1) = (w[0].as_i64().unwrap_or(0), w[1].as_i64().unwrap_or(0), w[2].as_i64().unwrap_or(0), w[3].as_i64().unwrap_or(0));
        px >= x0 - 16 && py >= y0 - 16 && px < x1 && py < y1
    });
    anyhow::ensure!(on_screen, "clamped onto a monitor: {clamped}");
    ui.request(json!({ "cmd": "quit" }))?;
    ui.wait_exit(Duration::from_secs(10))?;
    Ok(format!(
        "moved to {} {}, restored there; maximized restored; a gone monitor → {px},{py} on {}",
        moved["position"],
        moved["size"],
        clamped["monitor"].as_str().unwrap_or("?")
    ))
}

/// The window's UI Automation elements as a UIA client sees them (the
/// `IUIAutomation` COM API: what Narrator and test tools use): control type,
/// name, full description and value.
fn uia_snapshot(hwnd: isize) -> Result<Value> {
    std::thread::spawn(move || -> Result<Value> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement6, IUIAutomationValuePattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_DocumentControlTypeId,
            UIA_TreeItemControlTypeId, UIA_ValuePatternId,
        };
        use windows::core::Interface;
        // SAFETY: COM calls on this thread's own apartment and objects.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let window = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
            let all = window.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            let mut nodes = Vec::new();
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                let control = e.CurrentControlType().map(|c| c.0).unwrap_or(0);
                let kind = match control {
                    c if c == UIA_ButtonControlTypeId.0 => "Button".to_owned(),
                    c if c == UIA_TreeItemControlTypeId.0 => "TreeItem".to_owned(),
                    c if c == UIA_DocumentControlTypeId.0 => "Document".to_owned(),
                    _ => e.CurrentLocalizedControlType().map(|b| b.to_string()).unwrap_or_default(),
                };
                let description = e.cast::<IUIAutomationElement6>().ok().and_then(|e| e.CurrentFullDescription().ok()).map(|b| b.to_string()).unwrap_or_default();
                let value = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok().and_then(|p| p.CurrentValue().ok()).map(|b| b.to_string());
                nodes.push(json!({ "type": kind, "name": e.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "description": description, "value": value }));
            }
            Ok(json!({ "window": window.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "count": nodes.len(), "nodes": nodes }))
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}

/// Presses the first button named `name` through UIA's Invoke pattern.
fn uia_invoke(hwnd: isize, name: &str) -> Result<()> {
    let name = name.to_owned();
    std::thread::spawn(move || -> Result<()> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationInvokePattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_InvokePatternId};
        // SAFETY: COM calls on this thread's own apartment and objects.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let window = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
            let all = window.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                if e.CurrentControlType().map(|c| c.0) == Ok(UIA_ButtonControlTypeId.0) && e.CurrentName().map(|b| b.to_string()).is_ok_and(|n| n.trim() == name) {
                    e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)?.Invoke()?;
                    return Ok(());
                }
            }
            anyhow::bail!("no button {name:?}")
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}

fn uia_tree() -> Result<String> {
    let sb = Sandbox::new("uia")?;
    let mut ui = sb.start(&["--demo"], &[], Some((170, 46)), "uia.log")?;
    ui.wait("the demo canvas", Duration::from_secs(30), |t| t.contains("Build session canvas"))?;
    // Open the selected box's terminal (a demo PowerShell).
    let t = tree(&ui)?;
    let b = t["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|b| b["name"] == "Build session canvas")).cloned().context("the demo box")?;
    let r: Vec<f64> = b["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
    let at = (r[0] + 2.0, r[1] + r[3] / 2.0);
    for kind in ["down", "up", "down", "up"] {
        ui.request(json!({ "cmd": "mouse", "kind": kind, "button": "left", "x": at.0, "y": at.1 }))?;
    }
    wait_tree(&ui, "a terminal tile", 30, |t| t["tiles"].as_array().is_some_and(|t| !t.is_empty()))?;
    ui.wait("the shell", Duration::from_secs(30), |t| t.contains("PS "))?;
    let hwnd = ui.request(json!({ "cmd": "windows" }))?["windows"][0]["hwnd"].as_i64().context("the window handle")? as isize;
    let has = |snap: &Value, f: &dyn Fn(&Value) -> bool| snap["nodes"].as_array().is_some_and(|n| n.iter().any(f));
    let ty = |n: &Value, t: &str| n["type"].as_str() == Some(t);
    let name = |n: &Value, s: &str| n["name"].as_str().is_some_and(|x| x.contains(s));
    let status = |n: &Value| n["description"].as_str().is_some_and(|s| !s.is_empty());
    let deadline = Instant::now() + Duration::from_secs(20);
    let snap = loop {
        let snap = uia_snapshot(hwnd)?;
        let ready = has(&snap, &|n| ty(n, "Button") && name(n, "+ New"))
            && has(&snap, &|n| ty(n, "TreeItem") && name(n, "Build session canvas"))
            && has(&snap, &|n| ty(n, "Button") && name(n, "Build session canvas") && status(n))
            && has(&snap, &|n| ty(n, "Document") && name(n, "Build session canvas") && n["value"].as_str().is_some_and(|v| v.contains("PS ")));
        if ready {
            break snap;
        }
        if Instant::now() > deadline {
            let names: Vec<String> = snap["nodes"].as_array().into_iter().flatten().take(60).map(|n| format!("{}:{}:{}", n["type"].as_str().unwrap_or("?"), n["name"].as_str().unwrap_or(""), n["description"].as_str().unwrap_or(""))).collect();
            bail!("the UIA tree lacks an expected element ({} nodes): {}", snap["count"], names.join(" | "));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    std::fs::write(screens_dir().join("uia-tree.json"), serde_json::to_string_pretty(&snap)?).ok();
    let boxed = snap["nodes"].as_array().and_then(|n| n.iter().find(|n| ty(n, "Button") && name(n, "Build session canvas"))).cloned().unwrap_or_default();
    let tile = snap["nodes"].as_array().and_then(|n| n.iter().find(|n| ty(n, "Document"))).cloned().unwrap_or_default();
    // The test hook's tree has the same nodes.
    let hook = tree(&ui)?;
    let hook_names: Vec<&str> = hook["access"]["nodes"].as_array().into_iter().flatten().filter_map(|n| n["name"].as_str()).collect();
    anyhow::ensure!(hook_names.contains(&"+ New") && hook_names.iter().any(|n| n.contains("Build session canvas")), "the tree hook lists the same elements");
    // A UIA client presses a button: "+ New" opens the new-session dialog.
    uia_invoke(hwnd, "+ New")?;
    wait_tree(&ui, "the dialog opened through UIA", 10, |t| t["modals"].as_u64().unwrap_or(0) > 0)?;
    let dialog = uia_snapshot(hwnd)?;
    let dialog_seen = dialog["nodes"].as_array().is_some_and(|n| n.iter().any(|n| n["type"] == "dialog" || n["type"].as_str().is_some_and(|t| t.contains("dialog"))));
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    let status = boxed["description"].as_str().unwrap_or("").to_owned();
    Ok(format!(
        "{} UIA elements in window {:?}: Button \"+ New\", TreeItem and Button \"Build session canvas\" ({status}), Document {:?} with {} chars of text; \"+ New\" invoked through UIA opened the dialog{}",
        snap["count"],
        snap["window"].as_str().unwrap_or(""),
        tile["name"].as_str().unwrap_or(""),
        tile["value"].as_str().map(str::len).unwrap_or(0),
        if dialog_seen { " (a UIA dialog element)" } else { "" }
    ))
}

fn crash_log() -> Result<String> {
    let sb = Sandbox::new("crash")?;
    let canvas = static_canvas(&sb)?;
    let mut details = Vec::new();
    for (command, thread) in [("panic", "main"), ("panic_thread", "test-panic")] {
        let before = sb.crash_log().len();
        let mut ui = sb.start(&["--canvas", &canvas], &[], Some((170, 46)), &format!("crash-{command}.log"))?;
        wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| !b.is_empty()))?;
        let _ = ui.ask(json!({ "cmd": command }));
        let code = ui.wait_exit(Duration::from_secs(15))?;
        anyhow::ensure!(code == 101, "{command}: exit code {code}");
        let log = sb.crash_log();
        let report = &log[before..];
        for needle in [format!("thread {thread:?}"), format!("build {}", crate::paths::build_id()), "windows 1".into(), format!("\"cmd\":\"{command}\""), "adapter ".into()] {
            anyhow::ensure!(report.contains(&needle), "{command}: the report lacks {needle:?}:\n{report}");
        }
        details.push(format!("{command} → thread {thread:?}"));
    }
    Ok(format!("{}; build, window count, adapter and last hook command in each report", details.join(", ")))
}

fn software_fallback() -> Result<String> {
    let sb = Sandbox::new("warp")?;
    let canvas = static_canvas(&sb)?;
    let nohw = PathBuf::from("nohw");
    let mut ui = sb.start(&["--canvas", &canvas], &[("CANVAS_GUI_TEST_GPU", &nohw)], Some((170, 46)), "warp.log")?;
    wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| !b.is_empty()))?;
    let size = ui.request(json!({ "cmd": "size" }))?;
    anyhow::ensure!(size["software"] == true, "drawing with WARP: {size}");
    ui.wait("the notice", Duration::from_secs(5), |t| t.contains("No usable GPU"))?;
    png(&ui, "software-renderer")?;
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    anyhow::ensure!(sb.crash_log().contains("hardware adapter: refused"), "the hardware failure is logged:\n{}", sb.crash_log());
    // No adapter at all: a clear error pointing to the TUI.
    let none = PathBuf::from("none");
    let mut ui = sb.start(&["--canvas", &canvas], &[("CANVAS_GUI_TEST_GPU", &none)], Some((170, 46)), "none.log")?;
    let code = ui.wait_exit(Duration::from_secs(20))?;
    let stderr = std::fs::read_to_string(sb.root.join("none.log")).unwrap_or_default();
    anyhow::ensure!(code == 1 && stderr.contains("terminal UI") && stderr.contains("copilot-canvas"), "exit {code}: {stderr}");
    Ok(format!("WARP on {}; without any adapter: exit 1 with {:?}", size["adapter"].as_str().unwrap_or("?"), stderr.lines().next().unwrap_or_default()))
}

fn damage_frames() -> Result<String> {
    let sb = Sandbox::new("damage")?;
    let exe = std::env::current_exe()?;
    let mut doc = Doc::default();
    let mut tiles = Vec::new();
    for i in 0..4 {
        let id = format!("stream-{i}");
        let mut n = node(&id, &format!("Stream {i}"), 2, 2 + i * 6, &sb.work);
        n.command = Some(vec![exe.display().to_string(), "noise".into(), "--rate".into(), "30".into()]);
        n.fresh = true;
        doc.nodes.push(n);
        tiles.push(id);
    }
    doc.tiles = tiles.clone();
    doc.ui.tiled = true;
    let canvas = sb.canvas(&doc)?;
    let mut ui = sb.start(&["--canvas", &canvas], &[], Some((200, 56)), "damage.log")?;
    wait_tree(&ui, "the boxes", 30, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.len() >= 4))?;
    // Start the four streams from their boxes (Enter on each).
    for name in ["Stream 0", "Stream 1", "Stream 2", "Stream 3"] {
        let t = tree(&ui)?;
        let b = t["canvas"]["boxes"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name)).cloned().context("a stream box")?;
        let r: Vec<f64> = b["rect"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        for kind in ["down", "up", "down", "up"] {
            ui.request(json!({ "cmd": "mouse", "kind": kind, "button": "left", "x": r[0] + 2.0, "y": r[1] + r[3] / 2.0 }))?;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    wait_tree(&ui, "four tiles", 30, |t| t["tiles"].as_array().is_some_and(|t| t.len() == 4))?;
    ui.wait("streaming output", Duration::from_secs(20), |t| t.contains("step"))?;
    std::thread::sleep(Duration::from_secs(2));
    ui.request(json!({ "cmd": "perf", "reset": true }))?;
    let first = ui.text();
    std::thread::sleep(Duration::from_secs(4));
    let perf = ui.request(json!({ "cmd": "perf" }))?;
    let second = ui.text();
    png(&ui, "damage-streaming-tiles")?;
    let damage = &perf["perf"]["damage_ms"];
    let full = &perf["perf"]["full_ms"];
    anyhow::ensure!(damage["n"].as_u64().unwrap_or(0) > 20, "damage frames while streaming: {}", perf["perf"]);
    anyhow::ensure!(first != second, "the terminals' text changes on screen");
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(format!(
        "{} damage frames (p50 {} / p95 {} ms) and {} full frames (p50 {} / p95 {} ms) in 4 s",
        damage["n"], damage["p50"], damage["p95"], full["n"], full["p50"], full["p95"]
    ))
}
