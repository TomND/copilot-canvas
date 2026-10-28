//! Performance benchmark (no Copilot or model calls) and its helper programs.
//!
//! `bench` starts an isolated service, launches N real ConPTY terminals running
//! `noise` (continuous colored output), subscribes to four of them like a tiled
//! UI, and measures keystroke→screen latency through an `echo` terminal while
//! the others stream. It also times full UI renders of a 100-session canvas.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::client::app::{App, TermView};
use crate::client::conn::{self, Conn};
use crate::client::doc::{Doc, GRID_X, GRID_Y, Group, Node, NodeKind, Store};
use crate::paths::Instance;
use crate::protocol::{
    Activity, AgentStatus, AgentSummary, ClientMsg, Input, Key, LaunchKind, Run, ServiceMsg, ServiceStats, SessionSummary, TermInfo,
};

/// Streams agent-like colored output at `rate` lines per second.
pub fn noise(rate: u32) -> Result<()> {
    let interval = Duration::from_micros(1_000_000 / rate.max(1) as u64);
    let mut out = std::io::stdout().lock();
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    for i in 0u64.. {
        let color = 31 + (i % 6);
        writeln!(
            out,
            "\x1b[{color}m●\x1b[0m step {i:>6} \x1b[2mreading src/module_{:03}.rs\x1b[0m · \x1b[1mtool\x1b[0m result ok — 中文 ✓ {}",
            i % 997,
            "lorem ipsum dolor sit amet ".repeat(1 + (i % 2) as usize)
        )?;
        write!(out, "\x1b[36m{} Thinking…\x1b[0m\r", spinner[(i % 10) as usize])?;
        out.flush()?;
        std::thread::sleep(interval);
    }
    Ok(())
}

/// Spins one CPU core forever (load generator for `bench --burn`).
pub fn burn() -> Result<()> {
    let mut x: u64 = 1;
    loop {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
}

/// Prints `<n>:<char>` for every key, used for latency measurement.
pub fn echo() -> Result<()> {
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    write!(out, "echo ready\r\n")?;
    out.flush()?;
    let mut n = 0u64;
    loop {
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
                break;
            }
            if let KeyCode::Char(c) = key.code {
                n += 1;
                write!(out, "{n}:{c}\r\n")?;
                out.flush()?;
            }
        }
    }
    ratatui::crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

fn percentile(samples: &mut [f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let index = ((samples.len() as f64 - 1.0) * p).round() as usize;
    (samples[index] * 100.0).round() / 100.0
}

fn stats(conn: &Conn, views: &mut HashMap<String, TermView>) -> Result<ServiceStats> {
    conn.tx.send(ClientMsg::Stats)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match conn.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceMsg::Stats(stats)) => return Ok(stats),
            Ok(ServiceMsg::Frame(frame)) => views.entry(frame.id.clone()).or_default().apply(frame),
            _ => {}
        }
    }
    bail!("no stats from service")
}

fn row_text(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect::<String>()
}

pub fn run(terminals: usize, seconds: u64, json: Option<PathBuf>, burn: usize) -> Result<()> {
    anyhow::ensure!((1..=300).contains(&terminals), "--terminals must be 1-300");
    let root = std::env::temp_dir().join(format!("copilot-canvas-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    let instance = Instance::new(Some(format!("bench{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let result = measure(&instance, &root, terminals, seconds, burn);
    if let Ok(conn) = conn::try_connect(&instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(3));
    }
    for _ in 0..10 {
        if std::fs::remove_dir_all(&root).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let report = result?;
    let text = serde_json::to_string_pretty(&report)?;
    println!("{text}");
    if let Some(path) = json {
        std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

fn measure(instance: &Instance, root: &std::path::Path, terminals: usize, seconds: u64, burn: usize) -> Result<serde_json::Value> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let cwd = root.to_string_lossy().into_owned();
    eprintln!("starting an isolated service and {terminals} streaming terminals…");
    let conn = conn::connect(instance, true)?;
    let mut views: HashMap<String, TermView> = HashMap::new();
    let launch_started = Instant::now();
    let launch = |id: String, args: Vec<String>| -> Result<()> {
        conn.tx.send(ClientMsg::Launch { id, kind: LaunchKind::Command { program: exe.clone(), args }, cwd: cwd.clone(), cols: 100, rows: 30 })?;
        Ok(())
    };
    launch("echo".into(), vec!["echo".into()])?;
    for i in 0..terminals {
        launch(format!("noise-{i}"), vec!["noise".into(), "--rate".into(), "20".into()])?;
    }
    // CPU burners in background sessions (never shown, never typed in).
    for i in 0..burn {
        launch(format!("burn-{i}"), vec!["burn".into()])?;
    }
    let mut alive: HashMap<String, bool> = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while alive.values().filter(|a| **a).count() < terminals + burn + 1 {
        if Instant::now() > deadline {
            bail!("only {} of {} terminals started", alive.values().filter(|a| **a).count(), terminals + burn + 1);
        }
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Terminals(infos)) => {
                for TermInfo { id, alive: a, .. } in infos {
                    alive.insert(id, a);
                }
            }
            Ok(ServiceMsg::Error { message, .. }) => bail!("launch failed: {message}"),
            _ => {}
        }
    }
    let launch_ms = launch_started.elapsed().as_millis();
    conn.tx.send(ClientMsg::Subscribe { ids: vec!["echo".into(), "noise-0".into(), "noise-1".into(), "noise-2".into()] })?;
    // Warm up: let every terminal start streaming.
    let warm = Instant::now();
    while warm.elapsed() < Duration::from_secs(3) {
        if let Ok(ServiceMsg::Frame(frame)) = conn.rx.recv_timeout(Duration::from_millis(50)) {
            views.entry(frame.id.clone()).or_default().apply(frame);
        }
    }
    let before = stats(&conn, &mut views)?;
    let measured = Instant::now();
    let mut latencies = Vec::new();
    let (mut frames, mut frame_bytes, mut counter) = (0u64, 0u64, 0u64);
    let keys: Vec<char> = ('a'..='z').collect();
    while measured.elapsed() < Duration::from_secs(seconds) {
        counter += 1;
        let c = keys[(counter as usize) % keys.len()];
        let token = format!("{counter}:{c}");
        let sent = Instant::now();
        conn.tx.send(ClientMsg::Input { id: "echo".into(), input: Input::Key { key: Key::Char(c), mods: 0 } })?;
        let timeout = sent + Duration::from_secs(2);
        let mut seen = false;
        while Instant::now() < timeout && !seen {
            let Ok(message) = conn.rx.recv_timeout(Duration::from_millis(20)) else { continue };
            if let ServiceMsg::Frame(frame) = message {
                frames += 1;
                frame_bytes += postcard::to_stdvec(&frame).map(|b| b.len() as u64).unwrap_or(0);
                if frame.id == "echo" && frame.changed.iter().any(|(_, runs)| row_text(runs).trim() == token) {
                    seen = true;
                    latencies.push(sent.elapsed().as_secs_f64() * 1000.0);
                }
                views.entry(frame.id.clone()).or_default().apply(frame);
            }
        }
        if !seen {
            latencies.push(2000.0);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let elapsed = measured.elapsed().as_secs_f64();
    let after = stats(&conn, &mut views)?;
    let children = crate::win::descendants(after.pid);
    let child_private: u64 = children.iter().map(|pid| crate::win::process_usage(Some(*pid)).private).sum();
    let missed = latencies.iter().filter(|l| **l >= 2000.0).count();
    let cpu = (after.cpu_ms - before.cpu_ms) as f64 / (elapsed * 1000.0) * 100.0;
    let parsed = (after.bytes_parsed - before.bytes_parsed) as f64 / elapsed / 1_048_576.0;
    let render = render_bench()?;
    let round = |v: f64| (v * 10.0).round() / 10.0;
    Ok(serde_json::json!({
        "build": crate::paths::build_id(),
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "streaming_terminals": terminals,
        "cpu_burners": burn,
        "priority_manager": std::env::var_os("COPILOT_CANVAS_NO_PRIORITY").is_none(),
        "launch_all_ms": launch_ms,
        "keystroke_to_screen_ms": {
            "samples": latencies.len(),
            "p50": percentile(&mut latencies.clone(), 0.50),
            "p95": percentile(&mut latencies.clone(), 0.95),
            "p99": percentile(&mut latencies.clone(), 0.99),
            "max": percentile(&mut latencies, 1.0),
            "missed": missed,
        },
        "service": {
            "private_mb": round(after.private_bytes as f64 / 1_048_576.0),
            "working_set_mb": round(after.working_set_bytes as f64 / 1_048_576.0),
            "private_kb_per_terminal": round(after.private_bytes as f64 / 1024.0 / (terminals + 1) as f64),
            "cpu_percent_of_one_core": round(cpu),
            "output_parsed_mb_per_s": round(parsed),
        },
        "hosted_processes": {
            "count": children.len(),
            "private_mb": round(child_private as f64 / 1_048_576.0),
            "note": "benchmark noise/echo programs plus console hosts; real Copilot processes are much larger",
        },
        "client_stream": { "frames_per_s": (frames as f64 / elapsed).round(), "kb_per_s": (frame_bytes as f64 / 1024.0 / elapsed).round() },
        "ui_render": render,
    }))
}

/// Times full UI frames for a 100-session canvas with 1,200 subagent boxes.
fn render_bench() -> Result<serde_json::Value> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let dir = std::env::temp_dir().join(format!("copilot-canvas-render-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir.join("canvas.json"))?;
    let mut doc = Doc::default();
    let mut sessions = HashMap::new();
    for i in 0..100 {
        let id = uuid::Uuid::new_v4().to_string();
        let (x, y) = ((i % 10) * (GRID_X + 4), (i / 10) * (GRID_Y * 7));
        let session = Node {
            id: id.clone(),
            kind: NodeKind::Session,
            name: format!("Session {i:03} with a longer descriptive name"),
            x,
            y,
            cwd: Some(r"C:\src\repo".into()),
            parent: None,
            agent_key: None,
            source: None,
            command: None,
            fresh: false,
            automation: None,
            synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None,
        };
        let mut agents = Vec::new();
        for a in 0..12 {
            let key = format!("agent-{a}");
            doc.nodes.push(Node {
                id: format!("{id}:{key}"),
                kind: NodeKind::Agent,
                name: format!("Explorer {a}"),
                x: x + 4,
                y: y + 5 + a * 4,
                parent: Some(id.clone()),
                agent_key: Some(key.clone()),
                cwd: None,
                ..session.clone()
            });
            agents.push(AgentSummary { key, name: format!("Explorer {a}"), description: String::new(), status: AgentStatus::Working, finished_ms: None });
        }
        doc.nodes.push(session);
        let summary = SessionSummary {
            id: id.clone(),
            name: format!("Session {i}"),
            cwd: r"C:\src\repo".into(),
            branch: "main".into(),
            tracked: true,
            live: true,
            activity: Activity::Working,
            agents,
            ..Default::default()
        };
        sessions.insert(id, summary);
    }
    for g in 0..10 {
        doc.groups.push(Group { id: format!("g{g}"), name: format!("Group {g}"), x: -2, y: g * GRID_Y * 7 - 2, w: 10 * (GRID_X + 4) + 2, h: GRID_Y * 7 - 1, color: g as u8, folder: None, source: None, defaults: None });
    }
    let (tx, _keep) = crossbeam_channel::bounded(1_000_000);
    let (_service, rx) = crossbeam_channel::bounded(1);
    let conn = Conn { tx, rx, service_build: crate::paths::build_id().to_owned(), protocol_mismatch: false };
    let (app_tx, _app_rx) = crossbeam_channel::unbounded();
    let mut app = App::new(conn, store, doc, false, r"C:\".into(), app_tx, dir.clone());
    app.sessions = sessions;
    app.doc.ui.tiled = true;
    let ids: Vec<String> = app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).take(4).map(|n| n.id.clone()).collect();
    for id in &ids {
        app.terms.insert(id.clone(), TermInfo { id: id.clone(), alive: true, copilot: true, ..Default::default() });
        let line: Vec<Run> = (0..6).map(|k| Run { col: k * 12, text: "streaming ok".into(), fg: 1 + k as u32, bg: 0, flags: 0, link: None }).collect();
        app.views.insert(id.clone(), TermView { cols: 80, rows: 22, lines: vec![line; 22], cursor: Some((0, 0)), display_offset: 0, history: 0 });
        app.show_terminal(id);
    }
    let mut terminal = Terminal::new(TestBackend::new(200, 50))?;
    let mut results = serde_json::Map::new();
    for zoom in 0..3u8 {
        app.doc.view.zoom = zoom;
        let mut samples = Vec::new();
        for frame in 0..300i32 {
            app.doc.view.x = (frame % 40) * 3;
            app.doc.view.y = (frame % 25) * 2;
            app.tick = frame as usize;
            let started = Instant::now();
            terminal.draw(|f| crate::client::ui::draw(&mut app, f))?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        results.insert(
            format!("zoom_{}", ["100", "50", "25"][zoom as usize]),
            serde_json::json!({ "p50_ms": percentile(&mut samples.clone(), 0.5), "p95_ms": percentile(&mut samples.clone(), 0.95), "max_ms": percentile(&mut samples, 1.0) }),
        );
    }
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(serde_json::json!({ "sessions": 100, "subagent_boxes": 1200, "groups": 10, "terminal_tiles": 4, "screen": "200x50", "frames": results }))
}
