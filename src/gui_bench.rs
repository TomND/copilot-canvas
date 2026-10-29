//! `bench --client gui`: the GPU client's PLAN §9 measurements, driven over
//! its test hooks against an isolated service: first paint, frame cost with
//! 100 sessions + 1,200 subagents + 4 streaming tiles, panning, keystroke →
//! present through an `echo` terminal, idle CPU with 10 live sessions, memory.
//!
//! The GUI ports of the TUI's load probes:
//! - `--stress [--sessions N --agents M]`: N live sessions from synthetic
//!   histories (M subagents each; 30 busy ones append events every 50 ms,
//!   spawn subagents and ask permission) while the GUI idles, pans and fits;
//! - `--perf-canvas FILE`: panning a copy of a canvas file at 100, 50 and 25 %
//!   (`synthetic` writes a realistic 94-session canvas first);
//! - `--burn N`: keystroke → present while N CPU-burning background sessions
//!   run, with and without the service's priority manager.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::conn::{self, ClientSession, ConnOptions, ConnTarget};
use crate::client::doc::{Doc, GRID_X, GRID_Y, Group, Node, NodeKind, Store};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, LaunchKind, ServiceMsg};
use crate::selftest::{GuiDriver, Ui};

/// Which measurement to run.
pub enum Probe {
    Standard,
    Stress { sessions: usize, agents: usize },
    PerfCanvas(String),
    Burn(usize),
    /// Idle CPU and frames with no effect, with fireworks or attention in view,
    /// out of view and acknowledged.
    Effects,
    /// CPU and frames with looping sessions in view (their dots moving),
    /// while panning, out of view, and with reduced motion.
    Loops,
}

pub struct Options {
    pub seconds: u64,
    pub json: Option<PathBuf>,
    /// The client connects over TCP (`--remote loopback`).
    pub remote: bool,
    pub probe: Probe,
}

const ECHO: &str = "bench-echo";

pub fn run(options: Options) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-guibench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    std::fs::create_dir_all(root.join("work"))?;
    let instance = Instance::new(Some(format!("guibench{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let result = match &options.probe {
        Probe::Standard => measure(&instance, &root, &options),
        Probe::Stress { sessions, agents } => stress(&instance, &root, &options, *sessions, *agents),
        Probe::PerfCanvas(file) => perf_canvas(&instance, &root, &options, file),
        Probe::Burn(burners) => burn(&instance, &root, &options, *burners),
        Probe::Effects => effects(&instance, &root, &options),
        Probe::Loops => loops(&instance, &root, &options),
    };
    stop_service(&instance);
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
        agent_session: None, last_used_ms: None, accessed_ms: None, stacked: false, model: None,
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
    let streaming_full = forced_full(&ui, seconds)?;

    eprintln!("streaming, everything in view (fit)…");
    ui.send(b"f")?;
    std::thread::sleep(Duration::from_millis(500));
    let fitted = forced_full(&ui, seconds)?;
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
        "streaming_4_tiles_100pct": { "frame_ms": p(&streaming, "frame_ms"), "damage_frame_ms": p(&streaming, "damage_ms"), "full_frame_ms": p(&streaming, "full_ms"), "present_ms": p(&streaming, "present_ms"), "frames": streaming["perf"]["presented"], "private_mb": streaming["private_mb"] },
        "streaming_4_tiles_forced_full_frames": { "full_frame_ms": p(&streaming_full, "full_ms"), "view_ms": p(&streaming_full, "view_ms"), "scene_ms": p(&streaming_full, "scene_ms"), "encode_ms": p(&streaming_full, "encode_ms"), "damage_frame_ms": p(&streaming_full, "damage_ms") },
        "full_frame_1300_boxes_fit": { "boxes_drawn": boxes_fit, "full_frame_ms": p(&fitted, "full_ms"), "damage_frame_ms": p(&fitted, "damage_ms"), "view_ms": p(&fitted, "view_ms"), "scene_ms": p(&fitted, "scene_ms"), "encode_ms": p(&fitted, "encode_ms") },
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

fn stop_service(instance: &Instance) {
    if let Ok(conn) = conn::try_connect(instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Full redraws (the `redraw` hook, every 10 ms) for `duration` while
/// whatever streams keeps streaming; returns the `perf` answer.
fn forced_full(ui: &GuiDriver, duration: Duration) -> Result<Value> {
    perf(ui, true)?;
    let started = Instant::now();
    while started.elapsed() < duration {
        ui.request(json!({ "cmd": "redraw" }))?;
        std::thread::sleep(Duration::from_millis(10));
    }
    perf(ui, false)
}

/// The frame statistics of a `perf` answer.
fn frames(v: &Value) -> Value {
    let p = |key: &str| ms(&v["perf"][key]);
    json!({
        "frame_ms": p("frame_ms"), "full_frame_ms": p("full_ms"), "damage_frame_ms": p("damage_ms"), "view_ms": p("view_ms"), "scene_ms": p("scene_ms"),
        "encode_ms": p("encode_ms"), "input_to_present_ms": p("input_ms"), "frames": v["perf"]["presented"], "private_mb": v["private_mb"],
    })
}

/// CPU (% of one core) the GUI used between two `perf` answers.
fn cpu_percent(before: &Value, after: &Value, elapsed: Duration) -> f64 {
    let cpu = after["cpu_ms"].as_u64().unwrap_or(0).saturating_sub(before["cpu_ms"].as_u64().unwrap_or(0));
    (cpu as f64 / elapsed.as_millis().max(1) as f64 * 10_000.0).round() / 100.0
}

fn percentiles(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    let at = |p: f64| samples.get(((samples.len().max(1) - 1) as f64 * p).round() as usize).map(|v| (v * 100.0).round() / 100.0);
    json!({ "n": samples.len(), "p50": at(0.5), "p95": at(0.95), "p99": at(0.99), "max": at(1.0) })
}

/// UI responsiveness: round trips of a cheap command the UI thread answers.
fn hook_rtt(ui: &GuiDriver, count: usize) -> Result<Value> {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        ui.request(json!({ "cmd": "size" }))?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(percentiles(samples))
}

/// The canvas area's centre (cells).
fn canvas_center(ui: &GuiDriver) -> Result<(f64, f64)> {
    let tree = ui.request(json!({ "cmd": "tree" }))?;
    let area = &tree["tree"]["canvas"]["area"];
    let v = |i: usize, d: f64| area[i].as_f64().unwrap_or(d);
    Ok((v(0, 34.0) + v(2, 60.0) / 2.0, v(1, 1.0) + v(3, 40.0) / 2.0))
}

/// Middle-drags the canvas back and forth for `duration`, as a hand would
/// (a report every 8 ms); returns the `perf` answer.
fn pan(ui: &GuiDriver, duration: Duration) -> Result<Value> {
    let (cx, cy) = canvas_center(ui)?;
    perf(ui, true)?;
    ui.request(json!({ "cmd": "mouse", "kind": "down", "button": "middle", "x": cx, "y": cy }))?;
    let started = Instant::now();
    let mut step = 0;
    while started.elapsed() < duration {
        step += 1;
        let t = step as f64 * 0.05;
        ui.request(json!({ "cmd": "mouse", "kind": "drag", "button": "middle", "x": cx + 20.0 * t.sin(), "y": cy - 8.0 * (t * 0.7).sin() }))?;
        std::thread::sleep(Duration::from_millis(8));
    }
    ui.request(json!({ "cmd": "mouse", "kind": "up", "button": "middle", "x": cx, "y": cy }))?;
    perf(ui, false)
}

fn boxes(ui: &GuiDriver) -> usize {
    ui.request(json!({ "cmd": "tree" })).ok().and_then(|t| t["tree"]["canvas"]["boxes"].as_array().map(Vec::len)).unwrap_or(0)
}

fn bare_node(id: &str, name: &str, x: i32, y: i32, cwd: &str) -> Node {
    let mut n = node(id, name, x, y, None);
    n.cwd = Some(cwd.to_owned());
    n
}

/// `--stress`: N live sessions from synthetic histories (M subagents each);
/// 30 of them append events every 50 ms (tools, new subagents, permission
/// prompts). Frames while idle, panning and with everything in view, the
/// UI thread's responsiveness, CPU and memory.
fn stress(instance: &Instance, root: &Path, options: &Options, n: usize, m: usize) -> Result<Value> {
    anyhow::ensure!((1..=1000).contains(&n), "--sessions takes 1-1000");
    let pid = std::process::id();
    let sessions = instance.sessions_dir.clone();
    let work = root.join("work").to_string_lossy().into_owned();
    let ids: Vec<String> = (0..n).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let mut doc = Doc::default();
    eprintln!("{n} live sessions with {m} subagents each…");
    for (i, id) in ids.iter().enumerate() {
        let dir = sessions.join(id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("workspace.yaml"),
            format!("id: {id}\nname: \"Stress {i}\"\ncwd: C:\\\\stress\nclient_name: github/cli\ncreated_at: 2026-10-21T10:00:00Z\nupdated_at: 2026-10-21T10:00:00Z\n"),
        )?;
        let mut events = String::from("{\"type\":\"session.start\",\"data\":{}}\n{\"type\":\"user.message\",\"data\":{}}\n");
        for a in 0..m {
            events.push_str(&format!("{{\"type\":\"subagent.started\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\",\"agentName\":\"Agent {a}\"}}}}\n"));
            if a % 2 == 0 {
                events.push_str(&format!("{{\"type\":\"subagent.completed\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\"}}}}\n"));
            }
        }
        std::fs::write(dir.join("events.jsonl"), events)?;
        // This process holds the in-use lock: the sessions are live.
        std::fs::write(dir.join(format!("inuse.{pid}.lock")), "")?;
        let (x, y) = ((i % 10) as i32 * (GRID_X + 4), (i / 10) as i32 * (GRID_Y * 9));
        doc.nodes.push(bare_node(id, &format!("Stress {i}"), x, y, &work));
    }
    for g in 0..n.div_ceil(10) as i32 {
        doc.groups.push(Group { id: format!("g{g}"), name: format!("Group {g}"), x: -2, y: g * GRID_Y * 9 - 2, w: 10 * (GRID_X + 4) + 2, h: GRID_Y * 9 - 1, color: (g % 6) as u8, folder: None, source: None, defaults: None });
    }
    let path = root.join("stress-canvas.json");
    Store::open(&path)?.save(&doc)?;

    // Busy sessions append events continuously.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, sessions, busy) = (stop.clone(), sessions.clone(), ids[..n.min(30)].to_vec());
        std::thread::spawn(move || {
            let mut tick = 0u64;
            while !stop.load(Ordering::Relaxed) {
                tick += 1;
                for (i, id) in busy.iter().enumerate() {
                    let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(sessions.join(id).join("events.jsonl")) else { continue };
                    let mut lines = vec![
                        format!("{{\"type\":\"tool.execution_start\",\"data\":{{\"toolCallId\":\"t{tick}\",\"toolName\":\"shell\"}}}}"),
                        format!("{{\"type\":\"tool.execution_complete\",\"data\":{{\"toolCallId\":\"t{tick}\"}}}}"),
                    ];
                    if tick.is_multiple_of(10) && tick / 10 <= 12 {
                        lines.push(format!("{{\"type\":\"subagent.started\",\"agentId\":\"a{tick}\",\"data\":{{\"toolCallId\":\"s{tick}\",\"agentName\":\"Worker {tick}\"}}}}"));
                    }
                    if tick.is_multiple_of(15) && tick > 5 {
                        lines.push(format!("{{\"type\":\"subagent.completed\",\"agentId\":\"a{}\",\"data\":{{\"toolCallId\":\"s{}\"}}}}", tick - 5, tick - 5));
                    }
                    if (tick + i as u64).is_multiple_of(40) {
                        lines.push(format!("{{\"type\":\"permission.requested\",\"data\":{{\"requestId\":\"p{tick}\"}}}}"));
                    }
                    if (tick + i as u64) % 40 == 20 && tick > 20 {
                        lines.push(format!("{{\"type\":\"permission.completed\",\"data\":{{\"requestId\":\"p{}\"}}}}", tick - 20));
                    }
                    for line in lines {
                        let _ = writeln!(f, "{line}");
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let result = (|| -> Result<Value> {
        let remote: &[&str] = if options.remote { &["--remote", "loopback"] } else { &[] };
        let canvas_arg = path.to_string_lossy().into_owned();
        let log = root.join("gui-stderr.log");
        let size = (200, 56);
        let started = Instant::now();
        let mut ui = GuiDriver::start(&args(instance, root, &[&["--canvas", &canvas_arg], remote].concat()), &[], size, &log)?;
        ui.wait("the stress canvas", Duration::from_secs(60), |t| t.contains("Stress 0"))?;
        let shown = started.elapsed();
        // Subagent boxes arrive with the first history pass.
        let deadline = Instant::now() + Duration::from_secs(60);
        while boxes(&ui) < n + n * m / 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_secs(2));
        let seconds = Duration::from_secs(options.seconds.max(3));

        eprintln!("idle while 30 sessions stream events…");
        let before = perf(&ui, true)?;
        let idle_started = Instant::now();
        std::thread::sleep(seconds);
        let idle = perf(&ui, false)?;
        let idle_cpu = cpu_percent(&before, &idle, idle_started.elapsed());
        let rtt = hook_rtt(&ui, 40)?;

        eprintln!("panning…");
        let panning = pan(&ui, seconds)?;
        ui.screenshot("stress-panning")?;

        eprintln!("everything in view…");
        ui.send(b"f")?;
        std::thread::sleep(Duration::from_millis(600));
        let before = perf(&ui, true)?;
        let fit_started = Instant::now();
        std::thread::sleep(seconds);
        let fitted = perf(&ui, false)?;
        let fit_cpu = cpu_percent(&before, &fitted, fit_started.elapsed());
        let drawn = boxes(&ui);
        ui.screenshot("stress-fit")?;
        let fitted_full = forced_full(&ui, seconds)?;
        let rtt_fit = hook_rtt(&ui, 40)?;
        ui.request(json!({ "cmd": "quit" }))?;
        let _ = ui.wait_exit(Duration::from_secs(10));
        Ok(json!({
            "client": "gui",
            "probe": "stress",
            "remote": options.remote,
            "window_cells": format!("{}x{}", size.0, size.1),
            "sessions": n,
            "subagents_per_session": m,
            "busy_sessions": n.min(30),
            "boxes_on_canvas": drawn,
            "canvas_text_visible_ms": shown.as_millis() as u64,
            "idle_busy_sessions": { "frames": frames(&idle), "cpu_percent_of_one_core": idle_cpu },
            "ui_round_trip_ms_idle": rtt,
            "panning_100pct": frames(&panning),
            "fit_all_in_view": { "frames": frames(&fitted), "cpu_percent_of_one_core": fit_cpu },
            "fit_all_in_view_forced_full_frames": frames(&fitted_full),
            "ui_round_trip_ms_fit": rtt_fit,
        }))
    })();
    stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    result
}

/// A realistic canvas for `--perf-canvas synthetic`: 94 sessions in 9 groups
/// with subagents and a few subsessions (no real canvas file is needed).
fn synthetic_canvas() -> Doc {
    let mut doc = Doc::default();
    let mut i = 0;
    for g in 0..9i32 {
        let (gx, gy) = ((g % 3) * 140, (g / 3) * 70);
        let count = if g < 4 { 11 } else { 10 };
        for k in 0..count {
            let id = format!("s{i:03}");
            let (x, y) = (gx + 2 + (k % 3) * 44, gy + 3 + (k / 3) * 16);
            let mut session = bare_node(&id, &format!("Session {i:03} · refactor the parser and fix the build"), x, y, r"C:\src\repo");
            if k % 4 == 3 {
                // A subsession of the previous session (dashed edge).
                session.parent = Some(format!("s{:03}", i - 1));
            }
            doc.nodes.push(session);
            for a in 0..(i % 4) {
                let mut agent = node(&format!("{id}:agent-{a}"), &format!("Explorer {a}"), x + 4, y + 5 + a * 3, None);
                agent.kind = NodeKind::Agent;
                agent.parent = Some(id.clone());
                agent.agent_key = Some(format!("agent-{a}"));
                doc.nodes.push(agent);
            }
            i += 1;
        }
        doc.groups.push(Group { id: format!("g{g}"), name: format!("Project {g}"), x: gx, y: gy, w: 136, h: 66, color: g as u8, folder: None, source: None, defaults: None });
    }
    doc
}

/// `--perf-canvas FILE`: pans a copy of a canvas at 100, 50 and 25 %.
fn perf_canvas(instance: &Instance, root: &Path, options: &Options, file: &str) -> Result<Value> {
    let canvas = root.join("perf-canvas.json");
    let source = if file == "synthetic" {
        Store::open(&canvas)?.save(&synthetic_canvas())?;
        "synthetic: 94 sessions, 9 groups, subagents and subsessions".to_owned()
    } else {
        std::fs::copy(file, &canvas).with_context(|| format!("cannot copy {file}"))?;
        file.to_owned()
    };
    let doc = crate::selftest::load(&canvas)?;
    let remote: &[&str] = if options.remote { &["--remote", "loopback"] } else { &[] };
    let canvas_arg = canvas.to_string_lossy().into_owned();
    let log = root.join("gui-stderr.log");
    let size = (200, 55);
    let mut ui = GuiDriver::start(&args(instance, root, &[&["--canvas", &canvas_arg], remote].concat()), &[], size, &log)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while boxes(&ui) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the canvas did not show any box");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(1));
    let seconds = Duration::from_secs(options.seconds.max(3));
    let scale = |ui: &GuiDriver| ui.request(json!({ "cmd": "tree" })).ok().and_then(|t| t["tree"]["canvas"]["scale"].as_f64()).unwrap_or(1.0);
    let mut zooms = serde_json::Map::new();
    for (label, target) in [("100", 1.0), ("50", 0.5), ("25", 0.25)] {
        for _ in 0..8 {
            let now = scale(&ui);
            if (now - target).abs() < 1e-3 {
                break;
            }
            ui.send(if now > target { b"-" } else { b"+" })?;
            std::thread::sleep(Duration::from_millis(150));
        }
        eprintln!("panning at {label} %…");
        let panned = pan(&ui, seconds)?;
        let mut summary = frames(&panned);
        summary["scale"] = json!(scale(&ui));
        summary["boxes_in_model"] = json!(boxes(&ui));
        zooms.insert(format!("zoom_{label}"), summary);
        ui.screenshot(&format!("perf-canvas-{label}"))?;
    }
    ui.request(json!({ "cmd": "quit" }))?;
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(json!({
        "client": "gui",
        "probe": "perf-canvas",
        "canvas": source,
        "sessions": doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).count(),
        "boxes": doc.nodes.len(),
        "groups": doc.groups.len(),
        "window_cells": format!("{}x{}", size.0, size.1),
        "panning": zooms,
    }))
}

/// `--burn N`: keystroke → present through the echo terminal while N
/// background sessions burn CPU, with the service's priority manager and
/// without it (`COPILOT_CANVAS_NO_PRIORITY=1` for a fresh service).
fn burn(instance: &Instance, root: &Path, options: &Options, burners: usize) -> Result<Value> {
    anyhow::ensure!((1..=200).contains(&burners), "--burn takes 1-200");
    let exe = std::env::current_exe()?;
    let cwd = root.join("work").to_string_lossy().into_owned();
    let canvas = root.join("burn-canvas.json");
    let mut doc = Doc::default();
    doc.nodes.push(node(ECHO, "Bench echo", 2, 2, Some(vec!["cmd".into()])));
    doc.tiles = vec![ECHO.into()];
    doc.selected = Some(ECHO.into());
    Store::open(&canvas)?.save(&doc)?;
    let mut runs = serde_json::Map::new();
    for (label, no_priority) in [("priority_manager_on", false), ("priority_manager_off", true)] {
        stop_service(instance);
        let env: &[(&str, &str)] = if no_priority { &[("COPILOT_CANVAS_NO_PRIORITY", "1")] } else { &[] };
        conn::spawn_service_exe(&exe, instance, env)?;
        let target = if options.remote { ConnTarget::Loopback(instance.clone()) } else { ConnTarget::Local(instance.clone()) };
        let deadline = Instant::now() + Duration::from_secs(20);
        let conn = loop {
            match conn::open(&target, &ConnOptions { start: false, name: "bench".into(), ..Default::default() }, &ClientSession::new()) {
                Ok(conn) => break conn,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
                Err(error) => return Err(error),
            }
        };
        let launch = |id: &str, args: Vec<String>| conn.tx.send(ClientMsg::Launch { id: id.into(), kind: LaunchKind::Command { program: exe.to_string_lossy().into_owned(), args }, cwd: cwd.clone(), cols: 98, rows: 26 }).map_err(|_| anyhow::anyhow!("the service connection closed"));
        launch(ECHO, vec!["echo".into()])?;
        let ids: Vec<String> = std::iter::once(ECHO.to_owned()).chain((0..burners).map(|i| format!("burn-{i}"))).collect();
        for id in &ids[1..] {
            launch(id, vec!["burn".into()])?;
        }
        wait_alive(&conn, &ids)?;
        eprintln!("{label}: typing while {burners} sessions burn CPU…");
        let remote: &[&str] = if options.remote { &["--remote", "loopback"] } else { &[] };
        let canvas_arg = canvas.to_string_lossy().into_owned();
        let mut ui = GuiDriver::start(&args(instance, root, &[&["--canvas", &canvas_arg], remote].concat()), &[], (160, 44), &root.join("gui-stderr.log"))?;
        ui.wait("the echo terminal", Duration::from_secs(30), |t| t.contains("echo ready"))?;
        ui.send(b"\x1d")?;
        std::thread::sleep(Duration::from_secs(3));
        perf(&ui, true)?;
        let keys = 80;
        ui.request(json!({ "cmd": "post_keys", "text": "x".repeat(keys), "interval_ms": 100 }))?;
        std::thread::sleep(Duration::from_millis(100 * keys as u64 + 2000));
        let typed = perf(&ui, false)?;
        runs.insert(label.into(), json!({ "keystroke_to_present_ms": ms(&typed["perf"]["key_ms"]), "input_to_present_ms": ms(&typed["perf"]["input_ms"]), "keys": keys }));
        ui.request(json!({ "cmd": "quit" }))?;
        let _ = ui.wait_exit(Duration::from_secs(10));
        for id in &ids {
            let _ = conn.tx.send(ClientMsg::Stop { id: id.clone(), force: true });
        }
    }
    Ok(json!({
        "client": "gui",
        "probe": "burn",
        "remote": options.remote,
        "cpu_burners": burners,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "runs": runs,
    }))
}

/// `--effects`: the effects' cost. The visuals canvas (the parity suite's),
/// idle; then fireworks on a box in view, attention, the canvas panned so the
/// box is out of view, and the effects acknowledged: CPU (% of one core),
/// frames per second and frame times of each phase.
fn effects(instance: &Instance, root: &Path, options: &Options) -> Result<Value> {
    let canvas = crate::parity::visual::canvas(&instance.home, &root.join("work"), &root.join("group"), false, false)?;
    let on = PathBuf::from("on");
    let mut extra = vec!["--canvas", canvas.to_str().unwrap_or_default()];
    if options.remote {
        extra.extend(["--remote", "loopback"]);
    }
    let ui = GuiDriver::start(&args(instance, root, &extra), &[("COPILOT_CANVAS_EFFECTS", &on)], (170, 46), &root.join("gui-effects.log"))?;
    ui.wait("the visuals canvas", Duration::from_secs(30), |t| t.contains("Visual group"))?;
    let mut ui = ui;
    ui.send(b"f")?;
    std::thread::sleep(Duration::from_secs(3));
    let seconds = options.seconds.max(5);
    let phase = |ui: &GuiDriver, label: &str| -> Result<Value> {
        let before = perf(ui, true)?;
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(seconds));
        let after = perf(ui, false)?;
        let frames = after["perf"]["presented"].as_u64().unwrap_or(0).saturating_sub(before["perf"]["presented"].as_u64().unwrap_or(0));
        let elapsed = started.elapsed();
        eprintln!("{label}: {frames} frames in {:.1} s, {:.2} % of one core", elapsed.as_secs_f64(), cpu_percent(&before, &after, elapsed));
        Ok(json!({
            "cpu_percent_of_one_core": cpu_percent(&before, &after, elapsed),
            "fps": (frames as f64 / elapsed.as_secs_f64() * 10.0).round() / 10.0,
            "frame_ms": ms(&after["perf"]["frame_ms"]),
            "frames": frames,
        }))
    };
    let exe = std::env::current_exe()?;
    let effect = |args: &[&str]| crate::parity::visual::effect(&exe, &instance.home, &instance.name, args);
    let wait_effects = |ui: &GuiDriver, count: usize| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let tree = ui.request(json!({ "cmd": "tree" }))?;
            if tree["tree"]["canvas"]["effects"].as_array().map(Vec::len) == Some(count) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("the canvas does not show {count} effect(s)")
    };
    let idle = phase(&ui, "idle, no effects")?;
    effect(&["fireworks", "--on", "Fresh one", "--note", "PR !7 approved"])?;
    wait_effects(&ui, 1)?;
    let fireworks = phase(&ui, "fireworks in view")?;
    effect(&["clear", "--on", "Fresh one"])?;
    effect(&["attention", "--on", "Unused 5 days", "--note", "2 comments"])?;
    wait_effects(&ui, 1)?;
    let attention = phase(&ui, "attention in view")?;
    // Pan the boxes out of view (the effect stays on the canvas).
    for _ in 0..12 {
        ui.request(json!({ "cmd": "key", "key": "Down", "mods": ["ctrl"] }))?;
    }
    std::thread::sleep(Duration::from_secs(1));
    let away = phase(&ui, "attention out of view")?;
    effect(&["clear", "--on", "Unused 5 days"])?;
    wait_effects(&ui, 0)?;
    let cleared = phase(&ui, "effects cleared")?;
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(json!({
        "probe": "effects",
        "seconds_per_phase": seconds,
        "idle_no_effects": idle,
        "fireworks_in_view": fireworks,
        "attention_in_view": attention,
        "attention_out_of_view": away,
        "cleared": cleared,
    }))
}

/// `--loops`: the loop animation's cost. A group with eight looping
/// (passive) sessions and four plain ones, fitted in view: CPU (% of one
/// core), frames per second by kind and frame times with the dots moving,
/// while panning (a drag every 16 ms), scrolled out of view, and with
/// reduced motion.
fn loops(instance: &Instance, root: &Path, options: &Options) -> Result<Value> {
    let mut doc = Doc::default();
    for i in 0..12 {
        let (x, y) = (2 + (i % 3) * (GRID_X + 6), 2 + (i / 3) * GRID_Y);
        let mut n = node(&format!("loop-{i:02}"), &format!("Loop {i}"), x, y, None);
        n.reviewed_ms = Some(crate::paths::now_ms());
        if i < 8 {
            n.passive = Some(crate::client::doc::Passive { key: format!("loop-{i}-key"), group: "g-loops".into(), slug: format!("loop-{i}"), template: "Custom".into(), rollover_runs: 20, max_runs_per_day: 24, ..Default::default() });
        }
        doc.nodes.push(n);
    }
    doc.groups.push(Group { id: "g-loops".into(), name: "Loops".into(), x: 0, y: 0, w: 3 * (GRID_X + 6), h: 4 * GRID_Y + 2, color: 2, folder: Some(root.join("group").display().to_string()), source: None, defaults: None });
    let canvas = instance.home.join("loops.json");
    std::fs::create_dir_all(&instance.home)?;
    std::fs::write(&canvas, serde_json::to_string_pretty(&doc)?)?;
    let on = PathBuf::from("on");
    let mut extra = vec!["--canvas", canvas.to_str().unwrap_or_default()];
    if options.remote {
        extra.extend(["--remote", "loopback"]);
    }
    let ui = GuiDriver::start(&args(instance, root, &extra), &[("COPILOT_CANVAS_EFFECTS", &on)], (170, 46), &root.join("gui-loops.log"))?;
    ui.wait("the loops canvas", Duration::from_secs(30), |t| t.contains("Loops") && t.contains("Loop 11"))?;
    let mut ui = ui;
    ui.send(b"f")?;
    std::thread::sleep(Duration::from_secs(3));
    let seconds = options.seconds.max(5);
    let count = |v: &Value, key: &str| v["perf"][key].as_u64().unwrap_or(0);
    let phase = |ui: &GuiDriver, label: &str, during: &dyn Fn(&GuiDriver) -> Result<()>| -> Result<Value> {
        let before = perf(ui, true)?;
        let started = Instant::now();
        during(ui)?;
        let rest = Duration::from_secs(seconds).saturating_sub(started.elapsed());
        std::thread::sleep(rest);
        let after = perf(ui, false)?;
        let elapsed = started.elapsed();
        let fps = |key: &str| ((count(&after, key).saturating_sub(count(&before, key))) as f64 / elapsed.as_secs_f64() * 10.0).round() / 10.0;
        let in_view = ui.request(json!({ "cmd": "tree" }))?["tree"]["canvas"]["loop_arcs"].as_array().map(Vec::len).unwrap_or(0);
        let cpu = cpu_percent(&before, &after, elapsed);
        eprintln!("{label}: {in_view} loops in view, {:.1} loop + {:.1} full frames/s, {cpu:.2} % of one core", fps("loop_frames"), fps("full_frames"));
        Ok(json!({
            "loops_in_view": in_view,
            "cpu_percent_of_one_core": cpu,
            "fps": fps("presented"),
            "loop_fps": fps("loop_frames"),
            "full_fps": fps("full_frames"),
            "loop_frame_ms": ms(&after["perf"]["loop_ms"]),
            "full_frame_ms": ms(&after["perf"]["full_ms"]),
            "frame_ms": ms(&after["perf"]["frame_ms"]),
            "present_ms": ms(&after["perf"]["present_ms"]),
            "acquire_ms": ms(&after["perf"]["acquire_ms"]),
        }))
    };
    let idle = |_: &GuiDriver| Ok(());
    let _ = ui.screenshot("bench-loops");
    let in_view = phase(&ui, "loops in view", &idle)?;
    // Panning: a drag across empty canvas below the group, one step every 16 ms.
    let pan = |ui: &GuiDriver| -> Result<()> {
        let tree = ui.request(json!({ "cmd": "tree" }))?;
        let area: Vec<f64> = tree["tree"]["canvas"]["area"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let (x, y) = (area.first().copied().unwrap_or(40.0) + 4.0, area.get(1).copied().unwrap_or(2.0) + area.get(3).copied().unwrap_or(40.0) - 3.0);
        ui.request(json!({ "cmd": "mouse", "kind": "down", "button": "middle", "x": x, "y": y }))?;
        let started = Instant::now();
        let mut i = 0;
        while started.elapsed() < Duration::from_secs(seconds.saturating_sub(1)) {
            i += 1;
            let dx = ((i as f64) * 0.15).sin() * 6.0;
            ui.request(json!({ "cmd": "mouse", "kind": "drag", "button": "middle", "x": x + dx, "y": y - 1.0 }))?;
            std::thread::sleep(Duration::from_millis(16));
        }
        ui.request(json!({ "cmd": "mouse", "kind": "up", "button": "middle", "x": x, "y": y - 1.0 }))?;
        Ok(())
    };
    let panning = phase(&ui, "panning with loops in view", &pan)?;
    ui.send(b"f")?;
    std::thread::sleep(Duration::from_secs(1));
    // Scrolled away: the loops are on the canvas, not in view.
    for _ in 0..30 {
        ui.request(json!({ "cmd": "key", "key": "Down", "mods": ["ctrl"] }))?;
    }
    std::thread::sleep(Duration::from_secs(1));
    let away = phase(&ui, "loops out of view", &idle)?;
    ui.send(b"f")?;
    ui.request(json!({ "cmd": "motion", "value": "reduced" }))?;
    std::thread::sleep(Duration::from_secs(1));
    let _ = ui.screenshot("bench-loops-reduced-motion");
    let reduced = phase(&ui, "reduced motion, loops in view", &idle)?;
    let _ = ui.request(json!({ "cmd": "quit" }));
    let _ = ui.wait_exit(Duration::from_secs(10));
    Ok(json!({
        "probe": "loops",
        "seconds_per_phase": seconds,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "loops_in_view": in_view,
        "panning_with_loops": panning,
        "loops_out_of_view": away,
        "reduced_motion": reduced,
    }))
}