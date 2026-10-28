//! `bench --client gui`: the GPU client's PLAN §9 measurements, driven over
//! its test hooks against an isolated service: first paint, frame cost with
//! 100 sessions + 1,200 subagents + 4 streaming tiles, panning, keystroke →
//! present through an `echo` terminal, idle CPU with 10 live sessions, memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::conn::{self, ClientSession, ConnOptions, ConnTarget};
use crate::client::doc::{Doc, GRID_X, GRID_Y, Group, Node, NodeKind, Store};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, LaunchKind, ServiceMsg};
use crate::selftest::{GuiDriver, Ui};

pub struct Options {
    pub seconds: u64,
    pub json: Option<PathBuf>,
    /// The client connects over TCP (`--remote loopback`).
    pub remote: bool,
}

const ECHO: &str = "bench-echo";

pub fn run(options: Options) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-guibench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    std::fs::create_dir_all(root.join("work"))?;
    let instance = Instance::new(Some(format!("guibench{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let result = measure(&instance, &root, &options);
    if let Ok(conn) = conn::try_connect(&instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(3));
    }
    match &result {
        Ok(_) => {
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err(_) => eprintln!("artifacts kept in {}", root.display()),
    }
    let report = result?;
    let text = serde_json::to_string_pretty(&report)?;
    println!("{text}");
    if let Some(path) = &options.json {
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

fn args(instance: &Instance, root: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--sessions-dir".into(),
        instance.sessions_dir.to_string_lossy().into_owned(),
        "--cwd".into(),
        root.join("work").to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

fn perf(ui: &GuiDriver, reset: bool) -> Result<Value> {
    ui.request(json!({ "cmd": "perf", "reset": reset }))
}

/// 100 sessions with 12 subagents each in 10 groups, plus the benchmark's
/// four terminals (tiled; the echo terminal first).
fn canvas(tiles: &[String]) -> Doc {
    let mut doc = Doc::default();
    for (i, id) in tiles.iter().enumerate() {
        doc.nodes.push(node(id, &format!("Bench {id}"), -40, i as i32 * GRID_Y, Some(vec!["cmd".into()])));
    }
    for i in 0..100 {
        let id = format!("session-{i:03}");
        let (x, y) = ((i % 10) * (GRID_X + 4), (i / 10) * (GRID_Y * 7));
        for a in 0..12 {
            let mut agent = node(&format!("{id}:agent-{a}"), &format!("Explorer {a}"), x + 4, y + 5 + a * 4, None);
            agent.kind = NodeKind::Agent;
            agent.parent = Some(id.clone());
            agent.agent_key = Some(format!("agent-{a}"));
            doc.nodes.push(agent);
        }
        doc.nodes.push(node(&id, &format!("Session {i:03} with a longer descriptive name"), x, y, None));
    }
    for g in 0..10 {
        doc.groups.push(Group { id: format!("g{g}"), name: format!("Group {g}"), x: -2, y: g * GRID_Y * 7 - 2, w: 10 * (GRID_X + 4) + 2, h: GRID_Y * 7 - 1, color: g as u8, folder: None, source: None, defaults: None });
    }
    doc.tiles = tiles.to_vec();
    doc.ui.tiled = true;
    doc.view.x = -44;
    doc.view.y = -2;
    doc
}

fn node(id: &str, name: &str, x: i32, y: i32, command: Option<Vec<String>>) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x,
        y,
        cwd: Some(r"C:\src\repo".into()),
        parent: None,
        agent_key: None,
        source: None,
        command,
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

fn ms(value: &Value) -> Value {
    json!({ "p50": value["p50"], "p95": value["p95"], "p99": value["p99"], "max": value["max"], "n": value["n"] })
}

fn measure(instance: &Instance, root: &Path, options: &Options) -> Result<Value> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let remote: &[&str] = if options.remote { &["--remote", "loopback"] } else { &[] };
    let log = root.join("gui-stderr.log");
    let size = (200, 56);

    eprintln!("first paint: the GUI starts the service…");
    let started = Instant::now();
    let ui = GuiDriver::start(&args(instance, root, &[&["--demo"], remote].concat()), &[], size, &log)?;
    let connected = started.elapsed();
    ui.wait("the demo canvas", Duration::from_secs(30), |t| t.contains("Build session canvas"))?;
    let shown = started.elapsed();
    let cold = perf(&ui, false)?;
    let gpu = ui.request(json!({ "cmd": "size" }))?;
    ui.request(json!({ "cmd": "quit" }))?;
    drop(ui);
    if std::env::var_os("CANVAS_GUI_TRACE").is_some() {
        eprintln!("{}", std::fs::read_to_string(&log).unwrap_or_default());
    }

    eprintln!("launching an echo and three streaming terminals…");
    let target = if options.remote { ConnTarget::Loopback(instance.clone()) } else { ConnTarget::Local(instance.clone()) };
    let conn = conn::open(&target, &ConnOptions { start: true, name: "bench".into(), ..Default::default() }, &ClientSession::new())?;
    let tiles: Vec<String> = std::iter::once(ECHO.to_owned()).chain((0..3).map(|i| format!("bench-noise-{i}"))).collect();
    let cwd = root.join("work").to_string_lossy().into_owned();
    let launch = |id: &str, args: Vec<String>| -> Result<()> {
        conn.tx.send(ClientMsg::Launch { id: id.into(), kind: LaunchKind::Command { program: exe.clone(), args }, cwd: cwd.clone(), cols: 98, rows: 26 })?;
        Ok(())
    };
    launch(ECHO, vec!["echo".into()])?;
    for id in &tiles[1..] {
        launch(id, vec!["noise".into(), "--rate".into(), "20".into()])?;
    }
    wait_alive(&conn, &tiles)?;
    let path = root.join("bench-canvas.json");
    Store::open(&path)?.save(&canvas(&tiles))?;

    eprintln!("100 sessions + 1,200 subagents + 4 tiles…");
    let started = Instant::now();
    let canvas_arg = path.to_string_lossy().into_owned();
    let mut ui = GuiDriver::start(&args(instance, root, &[&["--canvas", &canvas_arg], remote].concat()), &[], size, &log)?;
    ui.wait("the canvas", Duration::from_secs(30), |t| t.contains("Session 000") && t.contains("echo ready"))?;
    let warm_shown = started.elapsed();
    let warm = perf(&ui, false)?;
    let seconds = Duration::from_secs(options.seconds.max(3));

    eprintln!("streaming at 100 %…");
    perf(&ui, true)?;
    std::thread::sleep(seconds);
    let streaming = perf(&ui, false)?;
    ui.screenshot("bench-streaming-tiles")?;

    eprintln!("streaming, everything in view (fit)…");
    ui.send(b"f")?;
    std::thread::sleep(Duration::from_millis(500));
    perf(&ui, true)?;
    std::thread::sleep(seconds);
    let fitted = perf(&ui, false)?;
    ui.screenshot("bench-1300-boxes-fit")?;
    let tree = ui.request(json!({ "cmd": "tree" }))?;
    let boxes_fit = tree["tree"]["canvas"]["boxes"].as_array().map(Vec::len).unwrap_or(0);

    eprintln!("panning at 100 %…");
    ui.send(b"+")?;
    ui.send(b"+")?;
    ui.send(b"+")?;
    let canvas_area = tree["tree"]["canvas"]["area"].clone();
    let (cx, cy) = (canvas_area[0].as_f64().unwrap_or(34.0) + 20.5, canvas_area[1].as_f64().unwrap_or(1.0) + 30.5);
    perf(&ui, true)?;
    ui.request(json!({ "cmd": "mouse", "kind": "down", "button": "middle", "x": cx, "y": cy }))?;
    let pan_started = Instant::now();
    let mut step = 0;
    while pan_started.elapsed() < seconds {
        step += 1;
        let t = step as f64 * 0.05;
        ui.request(json!({ "cmd": "mouse", "kind": "drag", "button": "middle", "x": cx + 12.0 * t.sin(), "y": cy - 6.0 * (t * 0.7).sin() }))?;
        std::thread::sleep(Duration::from_millis(8));
    }
    ui.request(json!({ "cmd": "mouse", "kind": "up", "button": "middle", "x": cx, "y": cy }))?;
    let panning = perf(&ui, false)?;

    eprintln!("keystroke → present through the echo terminal…");
    ui.send(b"\x1d")?;
    std::thread::sleep(Duration::from_millis(500));
    perf(&ui, true)?;
    let keys = 60;
    ui.request(json!({ "cmd": "post_keys", "text": "x".repeat(keys), "interval_ms": 120 }))?;
    std::thread::sleep(Duration::from_millis(120 * keys as u64 + 1500));
    let typing = perf(&ui, false)?;
    let private_mb = typing["private_mb"].clone();

    eprintln!("idle with 10 live sessions…");
    for id in &tiles[1..] {
        conn.tx.send(ClientMsg::Stop { id: id.clone(), force: true })?;
    }
    for i in 0..9 {
        launch(&format!("bench-idle-{i}"), vec!["echo".into()])?;
    }
    std::thread::sleep(Duration::from_secs(3));
    ui.send(b"\x1d")?;
    std::thread::sleep(Duration::from_secs(2));
    let before = perf(&ui, true)?;
    let idle_for = seconds.max(Duration::from_secs(10));
    std::thread::sleep(idle_for);
    let after = perf(&ui, false)?;
    let cpu = after["cpu_ms"].as_u64().unwrap_or(0).saturating_sub(before["cpu_ms"].as_u64().unwrap_or(0));
    let idle_cpu = cpu as f64 / idle_for.as_millis() as f64 * 100.0;
    ui.request(json!({ "cmd": "quit" }))?;
    let _ = ui.wait_exit(Duration::from_secs(10));
    for i in 0..9 {
        let _ = conn.tx.send(ClientMsg::Stop { id: format!("bench-idle-{i}"), force: true });
    }

    let p = |v: &Value, key: &str| ms(&v["perf"][key]);
    Ok(json!({
        "client": "gui",
        "remote": options.remote,
        "window_cells": format!("{}x{}", size.0, size.1),
        "adapter": gpu["adapter"],
        "present_mode": gpu["present_mode"],
        "dpi_scale": gpu["scale"],
        "first_paint": {
            "cold_service_start_ms": cold["first_paint_ms"],
            "cold_canvas_text_visible_ms": shown.as_millis() as u64,
            "cold_service_connected_ms": connected.as_millis() as u64,
            "warm_ms": warm["first_paint_ms"],
            "warm_1300_boxes_and_tiles_visible_ms": warm_shown.as_millis() as u64,
        },
        "streaming_4_tiles_100pct": { "frame_ms": p(&streaming, "frame_ms"), "view_ms": p(&streaming, "view_ms"), "scene_ms": p(&streaming, "scene_ms"), "encode_ms": p(&streaming, "encode_ms"), "present_ms": p(&streaming, "present_ms"), "frames": streaming["perf"]["presented"], "private_mb": streaming["private_mb"] },
        "full_frame_1300_boxes_fit": { "boxes_drawn": boxes_fit, "frame_ms": p(&fitted, "frame_ms"), "view_ms": p(&fitted, "view_ms"), "scene_ms": p(&fitted, "scene_ms"), "encode_ms": p(&fitted, "encode_ms") },
        "panning_100pct": { "frame_ms": p(&panning, "frame_ms"), "input_to_present_ms": p(&panning, "input_ms") },
        "keystroke_to_present_ms": p(&typing, "key_ms"),
        "input_to_present_ms": p(&typing, "input_ms"),
        "idle_10_live_sessions": { "cpu_percent_of_one_core": (idle_cpu * 100.0).round() / 100.0, "seconds": idle_for.as_secs(), "frames": after["perf"]["presented"].as_u64().unwrap_or(0).saturating_sub(before["perf"]["presented"].as_u64().unwrap_or(0)) },
        "private_mb_4_tiles_1300_boxes": private_mb,
        "service_rtt_ms": p(&typing, "rtt_ms"),
    }))
}

fn wait_alive(conn: &crate::client::conn::Conn, ids: &[String]) -> Result<()> {
    let mut alive: HashMap<String, bool> = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while ids.iter().any(|id| alive.get(id) != Some(&true)) {
        if Instant::now() > deadline {
            bail!("the benchmark terminals did not start");
        }
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Terminals(infos)) => {
                for info in infos {
                    alive.insert(info.id, info.alive);
                }
            }
            Ok(ServiceMsg::Error { message, .. }) => bail!("launch failed: {message}"),
            _ => {}
        }
    }
    Ok(())
}
