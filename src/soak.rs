//! `copilot-canvas selftest --remote soak [--minutes N] [--drop-every S] [--client tui|gui]`:
//! the long-run remote soak (PLAN §5 Phase 6, §10).
//!
//! A service with three streaming terminals and two echo terminals listens
//! on loopback TCP behind the fault-injecting proxy of `remote_probe`. Two
//! clients connect through the proxy: a protocol client that types into
//! `echo` every 50 ms and checks every acknowledged keystroke arrived
//! exactly once and in order, and the real canvas (TUI in a hidden
//! pseudoconsole, or the GUI over its test hooks) showing four busy tiles and
//! typing into `ui-echo` twice a second. Every `drop_every` seconds the proxy
//! drops the connections, takes the network down for 5 or 15 s, cuts a frame
//! in half, or delays everything by 300 ms. Checked: reconnect ≤ 2 s after
//! the network returns, full resync ≤ 500 ms, no lost or doubled keystrokes,
//! memory and handle growth of client and service, bandwidth. A summary
//! every minute and a JSON report at the end.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, bounded};
use serde_json::{Value, json};

use crate::client::app::TermView;
use crate::client::conn::{self, ClientSession, Conn, ConnOptions, ConnTarget};
use crate::client::doc::{Doc, Node, NodeKind, Store};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, Input, Key, LaunchKind, ServiceMsg};
use crate::remote_probe::Proxy;
use crate::selftest::{Console, GuiDriver};

pub struct Options {
    pub minutes: f64,
    pub drop_every: u64,
    pub gui: bool,
    pub json: Option<PathBuf>,
}

const ECHO: &str = "echo";
const UI_ECHO: &str = "ui-echo";
const NOISE: usize = 3;
/// PLAN §9: back within 2 s after the network returns; full resync within 500 ms.
const RECONNECT_BUDGET: Duration = Duration::from_secs(2);
const RESYNC_BUDGET: Duration = Duration::from_millis(500);
/// Keystrokes of the protocol client.
const TYPE_EVERY: Duration = Duration::from_millis(50);
/// Keystrokes of the canvas.
const UI_TYPE_EVERY: Duration = Duration::from_millis(500);

/// One reconnect of the protocol client.
struct Reconnect {
    lost_at: Instant,
    back_at: Instant,
    /// Reconnected → the echo terminal's full frame.
    resync: Option<Duration>,
}

/// The protocol client: types into `echo`, reconnects like the canvas
/// (input blocked while offline, never queued) and reads every echo line.
struct Typist {
    target: ConnTarget,
    options: ConnOptions,
    session: Arc<ClientSession>,
    conn: Option<Conn>,
    reconnect: Option<Receiver<Result<Conn>>>,
    offline_since: Option<Instant>,
    view: TermView,
    /// Keystrokes handed to a connection: seq → key.
    keys: BTreeMap<u64, char>,
    /// Seqs the service never confirmed (never resent).
    lost: Vec<(u64, u64)>,
    blocked: u64,
    /// Echo lines seen: n → key.
    seen: BTreeMap<u64, char>,
    conflicts: u64,
    reconnects: Vec<Reconnect>,
    awaiting_full: Option<Instant>,
    /// Bytes received by connections that are gone.
    wire_before: u64,
}

impl Typist {
    fn connect(target: ConnTarget) -> Result<Self> {
        let options = ConnOptions { name: "soak".into(), ..Default::default() };
        let session = ClientSession::new();
        let conn = conn::open(&target, &options, &session)?;
        conn.tx.send(ClientMsg::Subscribe8 { ids: vec![ECHO.into()], focused: Some(ECHO.into()) })?;
        Ok(Self {
            target,
            options,
            session,
            conn: Some(conn),
            reconnect: None,
            offline_since: None,
            view: TermView::default(),
            keys: BTreeMap::new(),
            lost: Vec::new(),
            blocked: 0,
            seen: BTreeMap::new(),
            conflicts: 0,
            reconnects: Vec::new(),
            awaiting_full: None,
            wire_before: 0,
        })
    }

    fn online(&self) -> bool {
        self.conn.is_some()
    }

    fn wire_bytes(&self) -> u64 {
        self.wire_before + self.conn.as_ref().map(|c| c.wire.bytes_in.load(Ordering::Relaxed)).unwrap_or(0)
    }

    fn pump(&mut self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(rx) = &self.reconnect
                && let Ok(result) = rx.try_recv()
            {
                self.reconnect = None;
                self.swap(result.context("reconnecting")?)?;
            }
            let slice = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(5));
            match &self.conn {
                Some(conn) => match conn.rx.recv_timeout(slice) {
                    Ok(message) => self.on_message(message),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => self.lost_connection(),
                },
                None => std::thread::sleep(slice),
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    fn on_message(&mut self, message: ServiceMsg) {
        let ServiceMsg::Frame8(frame) = message else { return };
        if frame.id != ECHO {
            return;
        }
        if frame.full
            && let Some(since) = self.awaiting_full.take()
            && let Some(last) = self.reconnects.last_mut()
        {
            last.resync = Some(since.elapsed());
        }
        self.view.apply8(frame);
        for runs in &self.view.lines {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            let Some((n, c)) = text.trim().split_once(':') else { continue };
            let (Ok(n), mut chars) = (n.parse::<u64>(), c.chars()) else { continue };
            let (Some(c), None) = (chars.next(), chars.next()) else { continue };
            match self.seen.insert(n, c) {
                Some(old) if old != c => self.conflicts += 1,
                _ => {}
            }
        }
    }

    fn lost_connection(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.wire_before += conn.wire.bytes_in.load(Ordering::Relaxed);
        }
        self.offline_since = Some(Instant::now());
        let (tx, rx) = bounded(1);
        let (target, options, session) = (self.target.clone(), self.options.clone(), self.session.clone());
        std::thread::spawn(move || {
            let _ = tx.send(conn::reconnect(&target, &options, &session, |_, _| true));
        });
        self.reconnect = Some(rx);
    }

    fn swap(&mut self, conn: Conn) -> Result<()> {
        let (ready, sent) = (self.session.acked(), self.session.sent());
        if self.session.settle() > 0 {
            self.lost.push((ready + 1, sent));
        }
        let now = Instant::now();
        self.reconnects.push(Reconnect { lost_at: self.offline_since.take().unwrap_or(now), back_at: now, resync: None });
        conn.tx.send(ClientMsg::Subscribe8 { ids: vec![ECHO.into()], focused: Some(ECHO.into()) })?;
        self.awaiting_full = Some(now);
        self.conn = Some(conn);
        Ok(())
    }

    fn type_key(&mut self, key: char) {
        let Some(conn) = &self.conn else {
            self.blocked += 1;
            return;
        };
        let before = self.session.sent();
        if conn.tx.send(ClientMsg::Input { id: ECHO.into(), input: Input::Key { key: Key::Char(key), mods: 0 } }).is_err() {
            self.blocked += 1;
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        while self.session.sent() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        match self.session.sent() {
            seq if seq > before => {
                self.keys.insert(seq, key);
            }
            _ => self.blocked += 1,
        }
    }

    /// Keys the service applied, in order: all handed out minus unconfirmed ranges.
    fn applied(&self) -> Vec<char> {
        self.keys.iter().filter(|(seq, _)| !self.lost.iter().any(|(a, b)| (a..=b).contains(seq))).map(|(_, c)| *c).collect()
    }

    fn unconfirmed(&self) -> u64 {
        self.lost.iter().map(|(a, b)| b - a + 1).sum()
    }

    /// (matching, mismatching, not seen, extra) echo lines against the applied keys.
    fn verify(&self) -> (u64, u64, u64, u64) {
        let applied = self.applied();
        let (mut ok, mut wrong, mut unseen) = (0, 0, 0);
        for (i, c) in applied.iter().enumerate() {
            match self.seen.get(&(i as u64 + 1)) {
                Some(e) if e == c => ok += 1,
                Some(_) => wrong += 1,
                None => unseen += 1,
            }
        }
        let extra = self.seen.keys().filter(|n| **n > applied.len() as u64).count() as u64;
        (ok, wrong, unseen, extra)
    }
}

/// The canvas under test.
enum Client {
    Tui(Box<Console>),
    Gui(Box<GuiDriver>),
}

impl Client {
    fn screen(&self) -> String {
        match self {
            Client::Tui(c) => c.text(),
            Client::Gui(g) => g
                .ask(json!({ "cmd": "screen" }))
                .ok()
                .and_then(|r| r["lines"].as_array().map(|l| l.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("\n")))
                .unwrap_or_default(),
        }
    }

    fn connected(&self) -> bool {
        match self {
            Client::Tui(c) => !c.text().contains("Offline — reconnecting"),
            Client::Gui(g) => g.ask(json!({ "cmd": "windows" })).ok().is_some_and(|r| r["windows"][0]["connected"] == true),
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Client::Tui(c) => c.send_now(bytes),
            Client::Gui(g) => {
                for c in String::from_utf8_lossy(bytes).chars() {
                    let key = match c {
                        '\x1d' => json!({ "cmd": "key", "char": "]", "mods": ["ctrl"] }),
                        '\r' => json!({ "cmd": "key", "key": "Enter" }),
                        c => json!({ "cmd": "key", "char": c.to_string() }),
                    };
                    g.request(key)?;
                }
                Ok(())
            }
        }
    }

    fn pid(&self) -> Option<u32> {
        match self {
            Client::Tui(c) => c.pid(),
            Client::Gui(g) => Some(g.pid()),
        }
    }

    fn quit(self) {
        match self {
            Client::Tui(mut c) => {
                let _ = c.send(b"\x1d");
                let _ = c.send(b"q");
                let _ = c.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"));
                let _ = c.send(b"\r");
                let _ = c.wait_exit(Duration::from_secs(10));
            }
            Client::Gui(mut g) => {
                let _ = g.request(json!({ "cmd": "quit" }));
                let _ = crate::selftest::Ui::wait_exit(&mut *g, Duration::from_secs(10));
            }
        }
    }
}

/// The highest `u<n>:<c>` echo line on the screen.
fn ui_echo(screen: &str) -> Option<(u64, char)> {
    let mut best: Option<(u64, char)> = None;
    for (at, _) in screen.match_indices('u') {
        let rest = &screen[at + 1..];
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || !rest[digits..].starts_with(':') {
            continue;
        }
        let Ok(n) = rest[..digits].parse::<u64>() else { continue };
        let Some(c) = rest[digits + 1..].chars().next().filter(|c| c.is_ascii_lowercase()) else { continue };
        if best.is_none_or(|(b, _)| n > b) {
            best = Some((n, c));
        }
    }
    best
}

/// The canvas's typing into `ui-echo`: one key at a time, each checked.
#[derive(Default)]
struct UiTyping {
    pending: Option<(char, Instant, u64)>,
    last_n: u64,
    next: u64,
    ok: u64,
    /// Not applied while offline or around a fault (blocked or unconfirmed).
    blocked: u64,
    /// Not applied while online, away from any fault.
    missing: u64,
    doubled: u64,
    wrong: u64,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Drop,
    Outage(u64),
    Cut,
    Delay,
}

impl Fault {
    fn name(self) -> String {
        match self {
            Fault::Drop => "drop".into(),
            Fault::Outage(s) => format!("outage {s} s"),
            Fault::Cut => "cut mid-frame".into(),
            Fault::Delay => "300 ms delay for 10 s".into(),
        }
    }
}

const SCHEDULE: [Fault; 5] = [Fault::Drop, Fault::Outage(5), Fault::Cut, Fault::Delay, Fault::Outage(15)];

/// One injected fault and how the clients came back.
struct FaultRecord {
    kind: Fault,
    at: Duration,
    /// When the network was back (the fault's end).
    back: Option<Instant>,
    typist_reconnect: Option<Duration>,
    /// How long the protocol client was offline.
    typist_offline: Option<Duration>,
    typist_resync: Option<Duration>,
    typist_reconnects_before: usize,
    ui_offline_seen: bool,
    ui_reconnect: Option<Duration>,
    done: bool,
}

struct Sample {
    at: f64,
    client_mb: f64,
    service_mb: f64,
    client_handles: u32,
    service_handles: u32,
}

fn handles(pid: u32) -> u32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain Win32 calls with a handle closed right after.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return 0;
        }
        let mut count = 0;
        let ok = GetProcessHandleCount(handle, &mut count);
        CloseHandle(handle);
        if ok != 0 { count } else { 0 }
    }
}

fn megabytes(pid: u32) -> f64 {
    crate::win::process_usage(Some(pid)).private as f64 / 1_048_576.0
}

/// Least-squares slope of `y` over time, per hour.
fn slope_per_hour(samples: &[(f64, f64)]) -> f64 {
    if samples.len() < 3 {
        return 0.0;
    }
    let n = samples.len() as f64;
    let (mx, my) = (samples.iter().map(|s| s.0).sum::<f64>() / n, samples.iter().map(|s| s.1).sum::<f64>() / n);
    let num: f64 = samples.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = samples.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    if den == 0.0 { 0.0 } else { num / den * 3600.0 }
}

fn ms(d: Option<Duration>) -> Value {
    json!(d.map(|d| (d.as_secs_f64() * 10_000.0).round() / 10.0))
}

fn round(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub fn run(options: Options) -> Result<()> {
    let exe = std::env::current_exe()?;
    let root = std::env::temp_dir().join(format!("copilot-canvas-soak-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    std::fs::create_dir_all(root.join("work"))?;
    let instance = Instance::new(Some(format!("soak{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let result = soak(&exe, &instance, &root, &options);
    if let Ok(conn) = conn::try_connect(&instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(3));
    }
    match &result {
        Ok(report) => {
            let text = serde_json::to_string_pretty(report)?;
            println!("{text}");
            if let Some(path) = &options.json {
                std::fs::write(path, &text).with_context(|| format!("cannot write {}", path.display()))?;
            }
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            anyhow::ensure!(report["pass"] == true, "the soak failed: {}", report["failures"]);
            println!("PASS");
            Ok(())
        }
        Err(error) => {
            eprintln!("artifacts kept in {}", root.display());
            Err(anyhow::anyhow!("{error:#}"))
        }
    }
}

fn soak(exe: &Path, instance: &Instance, root: &Path, options: &Options) -> Result<Value> {
    let client_name = if options.gui { "gui" } else { "tui" };
    println!(
        "Copilot Canvas remote soak: {} min, a fault every {} s, the {} client and a protocol client through a fault-injecting proxy",
        options.minutes,
        options.drop_every,
        client_name.to_uppercase()
    );
    conn::spawn_service_exe(exe, instance, &[("COPILOT_CANVAS_LISTEN", "127.0.0.1:0")])?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let port = loop {
        if let Some(port) = conn::listening_port(instance) {
            break port;
        }
        anyhow::ensure!(Instant::now() < deadline, "the service did not publish its TCP port; see {}", instance.log_path().display());
        std::thread::sleep(Duration::from_millis(100));
    };
    let token = canvas_common::auth::read_token(&instance.token_path())?;
    let proxy = Proxy::start(format!("127.0.0.1:{port}"))?;
    let target = ConnTarget::Tcp { addr: proxy.addr.clone(), remote_id: None, secret: token.clone() };
    let mut typist = Typist::connect(target)?;

    // The terminals: two echoes and three streaming ones.
    let cwd = root.join("work").to_string_lossy().into_owned();
    let program = exe.to_string_lossy().into_owned();
    let launch = |conn: &Conn, id: &str, args: Vec<String>, rows: u16| {
        conn.tx.send(ClientMsg::Launch { id: id.into(), kind: LaunchKind::Command { program: program.clone(), args }, cwd: cwd.clone(), cols: 60, rows }).map_err(|_| anyhow::anyhow!("the service connection closed"))
    };
    {
        let conn = typist.conn.as_ref().context("connected")?;
        launch(conn, ECHO, vec!["echo".into()], 50)?;
        launch(conn, UI_ECHO, vec!["echo".into(), "--tag".into(), "u".into()], 30)?;
        for i in 0..NOISE {
            launch(conn, &format!("noise-{i}"), vec!["noise".into(), "--rate".into(), "20".into()], 30)?;
        }
        conn.tx.send(ClientMsg::Stats)?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !typist.view.lines.iter().any(|l| l.iter().any(|r| r.text.contains("echo ready"))) {
        anyhow::ensure!(Instant::now() < deadline, "the echo terminal did not start");
        typist.pump(Duration::from_millis(50))?;
    }
    let service_pid = {
        let conn = typist.conn.as_ref().context("connected")?;
        conn.tx.send(ClientMsg::Stats)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pid = None;
        while pid.is_none() && Instant::now() < deadline {
            if let Ok(ServiceMsg::Stats(stats)) = conn.rx.recv_timeout(Duration::from_millis(100)) {
                pid = Some(stats.pid);
            }
        }
        pid.context("the service's pid")?
    };

    // The canvas, connected through the proxy, showing four busy tiles.
    let mut doc = Doc::default();
    let node = |id: &str, name: &str, y: i32| Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x: 2,
        y,
        cwd: Some(cwd.clone()),
        parent: None,
        agent_key: None,
        source: None,
        command: Some(vec!["cmd".into()]),
        fresh: false,
        automation: None,
        synced_name: None,
        reviewed_ms: None,
        mcp_off: None,
        passive: None,
        no_messages: false,
        agent: None,
        agent_session: None, last_used_ms: None, stacked: false,
    };
    doc.nodes.push(node(UI_ECHO, "Soak echo", 2));
    for i in 0..NOISE {
        doc.nodes.push(node(&format!("noise-{i}"), &format!("Soak stream {i}"), 8 + i as i32 * 6));
    }
    doc.tiles = std::iter::once(UI_ECHO.to_owned()).chain((0..NOISE).map(|i| format!("noise-{i}"))).collect();
    doc.ui.tiled = true;
    doc.selected = Some(UI_ECHO.into());
    let canvas = root.join("soak-canvas.json");
    Store::open(&canvas)?.save(&doc)?;
    let client_home = root.join("client-home");
    std::fs::create_dir_all(&client_home)?;
    let args: Vec<String> = vec![
        "--instance".into(),
        "soakclient".into(),
        "--home".into(),
        client_home.to_string_lossy().into_owned(),
        "--cwd".into(),
        cwd.clone(),
        "--canvas".into(),
        canvas.to_string_lossy().into_owned(),
        "--remote".into(),
        proxy.addr.clone(),
    ];
    let secret = crate::paths::hex(&token);
    let env = [("COPILOT_CANVAS_REMOTE_SECRET", Path::new(&secret))];
    let mut client = if options.gui {
        Client::Gui(Box::new(GuiDriver::start(&args, &env, (170, 46), &root.join("gui-stderr.log"))?))
    } else {
        Client::Tui(Box::new(Console::start_with_env(exe, &args, &env, 170, 46)?))
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !client.screen().contains("echo ready") {
        anyhow::ensure!(Instant::now() < deadline, "the canvas did not show the echo terminal\n{}", client.screen());
        typist.pump(Duration::from_millis(100))?;
    }
    // Keyboard to the echo tile; the first key proves it.
    client.send(b"\x1d")?;
    let mut ui = UiTyping { next: 0, ..Default::default() };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        client.send(b"a")?;
        typist.pump(Duration::from_millis(400))?;
        if let Some((n, _)) = ui_echo(&client.screen()) {
            ui.last_n = n;
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "typing in the canvas does not reach the echo terminal\n{}", client.screen());
        client.send(b"\x1d")?;
    }
    let client_pid = client.pid().context("the client's pid")?;
    println!("  · service pid {service_pid}, {client_name} client pid {client_pid}, proxy {}", proxy.addr);

    let started = Instant::now();
    let total = Duration::from_secs_f64(options.minutes * 60.0);
    let every = Duration::from_secs(options.drop_every.max(10));
    let mut faults: Vec<FaultRecord> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    let mut next_fault = every;
    let mut next_sample = Duration::ZERO;
    let mut next_minute = Duration::from_secs(60);
    let mut last_type = Instant::now();
    let mut last_ui_type = Instant::now();
    let mut active: Option<(Fault, Instant)> = None;
    let mut bytes_at_minute = (proxy.shared.bytes_down.load(Ordering::Relaxed), typist.wire_bytes(), Instant::now());
    let mut bandwidth: Vec<(f64, f64)> = Vec::new();
    let mut key_index = 0u64;
    let mut ui_online = true;
    let mut ui_checked = Instant::now();
    while started.elapsed() < total {
        let now = Instant::now();
        let elapsed = started.elapsed();
        // Inject the next fault.
        if active.is_none() && elapsed >= next_fault {
            let kind = SCHEDULE[faults.len() % SCHEDULE.len()];
            match kind {
                Fault::Drop => proxy.drop_all(),
                Fault::Outage(_) => {
                    proxy.set_down(true);
                    proxy.drop_all();
                }
                Fault::Cut => {
                    // The next frame of at least 32 bytes (the streams send them all the time).
                    proxy.cut_next_frame(32);
                }
                Fault::Delay => proxy.set_delay(Duration::from_millis(300)),
            }
            faults.push(FaultRecord {
                kind,
                at: elapsed,
                back: matches!(kind, Fault::Drop | Fault::Cut).then_some(now),
                typist_reconnect: None,
                typist_offline: None,
                typist_resync: None,
                typist_reconnects_before: typist.reconnects.len(),
                ui_offline_seen: false,
                ui_reconnect: None,
                done: false,
            });
            active = Some((kind, now));
            next_fault += every;
        }
        // End it.
        if let Some((kind, since)) = active {
            let lasted = since.elapsed();
            let record = faults.last_mut().expect("a fault");
            match kind {
                Fault::Outage(s) if lasted >= Duration::from_secs(s) && record.back.is_none() => {
                    proxy.set_down(false);
                    record.back = Some(Instant::now());
                }
                Fault::Delay if lasted >= Duration::from_secs(10) && record.back.is_none() => {
                    proxy.set_delay(Duration::ZERO);
                    record.back = Some(Instant::now());
                }
                _ => {}
            }
            // Measured once both clients are back (or 20 s after the network returned).
            if let Some(back) = record.back {
                let typist_back = typist.reconnects.get(record.typist_reconnects_before..).and_then(|r| r.first()).filter(|r| r.back_at >= since);
                if let Some(r) = typist_back {
                    record.typist_reconnect = Some(r.back_at.saturating_duration_since(back));
                    record.typist_offline = Some(r.back_at.saturating_duration_since(r.lost_at));
                    record.typist_resync = r.resync;
                }
                let typist_done = matches!(kind, Fault::Delay | Fault::Cut) || typist_back.is_some_and(|r| r.resync.is_some());
                let ui_done = record.ui_reconnect.is_some() || (!record.ui_offline_seen && back.elapsed() > Duration::from_secs(3));
                if (typist_done && ui_done && typist.online()) || back.elapsed() > Duration::from_secs(20) {
                    record.done = true;
                    active = None;
                }
            }
        }
        // The canvas's connection state (every 50 ms while a fault is open).
        if (active.is_some() || !ui_online) && ui_checked.elapsed() >= Duration::from_millis(50) {
            ui_checked = Instant::now();
            let online = client.connected();
            if let Some(record) = faults.last_mut().filter(|r| !r.done) {
                if !online && !record.ui_offline_seen {
                    record.ui_offline_seen = true;
                }
                if online && record.ui_offline_seen && record.ui_reconnect.is_none() {
                    let back = record.back.unwrap_or_else(Instant::now);
                    record.ui_reconnect = Some(Instant::now().saturating_duration_since(back));
                }
            }
            ui_online = online;
        }
        // The protocol client types.
        if now.duration_since(last_type) >= TYPE_EVERY {
            last_type = now;
            key_index += 1;
            typist.type_key((b'a' + (key_index % 26) as u8) as char);
        }
        // The canvas types, one key at a time.
        if now.duration_since(last_ui_type) >= UI_TYPE_EVERY {
            last_ui_type = now;
            let echoed = ui_echo(&client.screen());
            if let Some((c, typed_at, before)) = ui.pending {
                let (n, last) = echoed.unwrap_or((ui.last_n, ' '));
                let near_fault = faults.iter().any(|f| {
                    let at = started + f.at;
                    let end = f.back.unwrap_or_else(Instant::now) + Duration::from_secs(3);
                    typed_at + Duration::from_secs(1) >= at && typed_at <= end
                });
                if n == before + 1 {
                    if last == c { ui.ok += 1 } else { ui.wrong += 1 }
                    ui.last_n = n;
                    ui.pending = None;
                } else if n > before + 1 {
                    ui.doubled += n - before - 1;
                    ui.last_n = n;
                    ui.pending = None;
                } else if typed_at.elapsed() > Duration::from_millis(2500) {
                    if near_fault || !ui_online { ui.blocked += 1 } else { ui.missing += 1 }
                    ui.pending = None;
                }
            }
            if ui.pending.is_none() {
                ui.next += 1;
                let c = (b'a' + (ui.next % 26) as u8) as char;
                client.send(c.to_string().as_bytes())?;
                ui.pending = Some((c, Instant::now(), ui.last_n));
            }
        }
        // Memory and handles.
        if elapsed >= next_sample {
            next_sample += Duration::from_secs(20);
            samples.push(Sample {
                at: elapsed.as_secs_f64(),
                client_mb: megabytes(client_pid),
                service_mb: megabytes(service_pid),
                client_handles: handles(client_pid),
                service_handles: handles(service_pid),
            });
        }
        if elapsed >= next_minute {
            next_minute += Duration::from_secs(60);
            let (total_bytes, typist_bytes, at) = (proxy.shared.bytes_down.load(Ordering::Relaxed), typist.wire_bytes(), Instant::now());
            let secs = at.duration_since(bytes_at_minute.2).as_secs_f64();
            let all = (total_bytes - bytes_at_minute.0) as f64 / 1024.0 / secs;
            let mine = (typist_bytes.saturating_sub(bytes_at_minute.1)) as f64 / 1024.0 / secs;
            bandwidth.push((all, (all - mine).max(0.0)));
            bytes_at_minute = (total_bytes, typist_bytes, at);
            let (ok, wrong, _, extra) = typist.verify();
            let worst = faults.iter().filter_map(|f| f.typist_reconnect).max();
            let worst_ui = faults.iter().filter_map(|f| f.ui_reconnect).max();
            let last = samples.last();
            println!(
                "  {:>4.0} s · {} faults · typed {} (applied {ok}, wrong {wrong}, extra {extra}, unconfirmed {}, blocked {}) · canvas keys ok {} blocked {} missing {} doubled {} · reconnect max {:.2} s (canvas {:.2} s) · client {:.0} MB {} handles · service {:.0} MB {} handles · {:.1} KB/s ({:.1} KB/s canvas)",
                elapsed.as_secs_f64(),
                faults.len(),
                typist.keys.len(),
                typist.unconfirmed(),
                typist.blocked,
                ui.ok,
                ui.blocked,
                ui.missing,
                ui.doubled,
                worst.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                worst_ui.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                last.map(|s| s.client_mb).unwrap_or(0.0),
                last.map(|s| s.client_handles).unwrap_or(0),
                last.map(|s| s.service_mb).unwrap_or(0.0),
                last.map(|s| s.service_handles).unwrap_or(0),
                all,
                (all - mine).max(0.0),
            );
        }
        typist.pump(Duration::from_millis(10))?;
    }
    // End any open fault and let everything settle.
    proxy.set_down(false);
    proxy.set_delay(Duration::ZERO);
    let deadline = Instant::now() + Duration::from_secs(15);
    while (!typist.online() || typist.session.acked() < typist.session.sent() || typist.seen.keys().next_back().copied().unwrap_or(0) < typist.applied().len() as u64) && Instant::now() < deadline {
        typist.pump(Duration::from_millis(50))?;
    }
    let (ok, wrong, unseen, extra) = typist.verify();
    let running = match &mut client {
        Client::Gui(g) => g.running(),
        Client::Tui(_) => true,
    };
    client.quit();

    // The report.
    let warm = samples.iter().filter(|s| s.at >= (total.as_secs_f64() * 0.2).min(120.0)).collect::<Vec<_>>();
    let series = |f: &dyn Fn(&Sample) -> f64| warm.iter().map(|s| (s.at, f(s))).collect::<Vec<_>>();
    let client_slope = slope_per_hour(&series(&|s| s.client_mb));
    let service_slope = slope_per_hour(&series(&|s| s.service_mb));
    let client_handle_slope = slope_per_hour(&series(&|s| s.client_handles as f64));
    let service_handle_slope = slope_per_hour(&series(&|s| s.service_handles as f64));
    let growth = |f: &dyn Fn(&Sample) -> f64| match (warm.first(), warm.last()) {
        (Some(a), Some(b)) => f(b) - f(a),
        _ => 0.0,
    };
    let reconnects: Vec<Duration> = faults.iter().filter_map(|f| f.typist_reconnect).collect();
    let resyncs: Vec<Duration> = faults.iter().filter_map(|f| f.typist_resync).collect();
    let ui_reconnects: Vec<Duration> = faults.iter().filter_map(|f| f.ui_reconnect).collect();
    let max = |v: &[Duration]| v.iter().max().copied();
    let mut failures: Vec<String> = Vec::new();
    if wrong > 0 || extra > 0 || typist.conflicts > 0 {
        failures.push(format!("protocol client: {wrong} keystrokes out of order or changed, {extra} extra, {} conflicting echo lines", typist.conflicts));
    }
    if ui.doubled > 0 || ui.wrong > 0 || ui.missing > 0 {
        failures.push(format!("canvas: {} doubled, {} wrong, {} lost while online", ui.doubled, ui.wrong, ui.missing));
    }
    if let Some(worst) = max(&reconnects).filter(|d| *d > RECONNECT_BUDGET) {
        failures.push(format!("protocol client reconnected {:.2} s after the network returned (budget 2 s)", worst.as_secs_f64()));
    }
    if let Some(worst) = max(&ui_reconnects).filter(|d| *d > RECONNECT_BUDGET + Duration::from_millis(100)) {
        failures.push(format!("the canvas reconnected {:.2} s after the network returned (budget 2 s; polled every 50 ms)", worst.as_secs_f64()));
    }
    if let Some(worst) = max(&resyncs).filter(|d| *d > RESYNC_BUDGET) {
        failures.push(format!("full resync took {:.0} ms (budget 500 ms)", worst.as_secs_f64() * 1000.0));
    }
    let outages_without_reconnect = faults.iter().filter(|f| matches!(f.kind, Fault::Drop | Fault::Outage(_)) && f.done && f.typist_reconnect.is_none()).count();
    if outages_without_reconnect > 0 {
        failures.push(format!("{outages_without_reconnect} drops or outages without a measured reconnect"));
    }
    // A leak: steady growth over the warm part of the run, beyond noise.
    let minutes = total.as_secs_f64() / 60.0;
    if minutes >= 10.0 {
        if client_slope > 20.0 && growth(&|s| s.client_mb) > 15.0 {
            failures.push(format!("client memory grows {client_slope:.1} MB/h"));
        }
        if service_slope > 20.0 && growth(&|s| s.service_mb) > 15.0 {
            failures.push(format!("service memory grows {service_slope:.1} MB/h"));
        }
        if growth(&|s| s.client_handles as f64) > 100.0 || growth(&|s| s.service_handles as f64) > 200.0 {
            failures.push("handle counts keep growing".into());
        }
    }
    if !running {
        failures.push("the canvas exited during the soak".into());
    }
    let avg = |v: &[(f64, f64)], f: fn(&(f64, f64)) -> f64| if v.is_empty() { 0.0 } else { v.iter().map(f).sum::<f64>() / v.len() as f64 };
    let canvas_bandwidth = avg(&bandwidth, |b| b.1);
    if canvas_bandwidth > 200.0 {
        failures.push(format!("the canvas received {canvas_bandwidth:.0} KB/s for 4 busy tiles (budget 200 KB/s)"));
    }
    let fault_list: Vec<Value> = faults
        .iter()
        .map(|f| {
            json!({
                "at_s": round(f.at.as_secs_f64()), "kind": f.kind.name(), "protocol_reconnect_ms": ms(f.typist_reconnect), "protocol_offline_ms": ms(f.typist_offline), "resync_ms": ms(f.typist_resync),
                "canvas_offline_seen": f.ui_offline_seen, "canvas_reconnect_ms": ms(f.ui_reconnect),
            })
        })
        .collect();
    let stats = |v: &[Duration]| {
        let mut v: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        v.sort_by(f64::total_cmp);
        let at = |p: f64| v.get(((v.len().max(1) - 1) as f64 * p).round() as usize).map(|x| x.round());
        json!({ "n": v.len(), "p50": at(0.5), "p95": at(0.95), "max": at(1.0) })
    };
    Ok(json!({
        "pass": failures.is_empty(),
        "failures": failures,
        "client": client_name,
        "minutes": options.minutes,
        "drop_every_s": options.drop_every,
        "faults_injected": faults.len(),
        "faults_by_kind": SCHEDULE.iter().map(|k| json!({ "kind": k.name(), "count": faults.iter().filter(|f| f.kind.name() == k.name()).count() })).collect::<Vec<_>>(),
        "proxy_connections": proxy.shared.connections.load(Ordering::Relaxed),
        "protocol_client": {
            "keystrokes_handed_to_transport": typist.keys.len(),
            "applied_exactly_once_in_order": ok,
            "out_of_order_or_changed": wrong,
            "extra_or_doubled": extra,
            "unconfirmed_reported_not_resent": typist.unconfirmed(),
            "blocked_while_offline": typist.blocked,
            "echo_lines_not_observed": unseen,
            "reconnects": typist.reconnects.len(),
            "reconnect_after_network_returns_ms": stats(&reconnects),
            "full_resync_ms": stats(&resyncs),
        },
        "canvas": {
            "keys_typed": ui.next,
            "applied_once": ui.ok,
            "blocked_or_unconfirmed_near_a_fault": ui.blocked,
            "lost_while_online": ui.missing,
            "doubled": ui.doubled,
            "wrong": ui.wrong,
            "reconnect_after_network_returns_ms": stats(&ui_reconnects),
            "bandwidth_kb_s_4_busy_tiles": round(canvas_bandwidth),
        },
        "bandwidth_kb_s_all_clients": round(avg(&bandwidth, |b| b.0)),
        "memory": {
            "client_private_mb": { "first": samples.first().map(|s| round(s.client_mb)), "last": samples.last().map(|s| round(s.client_mb)), "max": round(samples.iter().map(|s| s.client_mb).fold(0.0, f64::max)), "slope_mb_per_hour_after_warmup": round(client_slope) },
            "service_private_mb": { "first": samples.first().map(|s| round(s.service_mb)), "last": samples.last().map(|s| round(s.service_mb)), "max": round(samples.iter().map(|s| s.service_mb).fold(0.0, f64::max)), "slope_mb_per_hour_after_warmup": round(service_slope) },
            "client_handles": { "first": samples.first().map(|s| s.client_handles), "last": samples.last().map(|s| s.client_handles), "slope_per_hour_after_warmup": round(client_handle_slope) },
            "service_handles": { "first": samples.first().map(|s| s.service_handles), "last": samples.last().map(|s| s.service_handles), "slope_per_hour_after_warmup": round(service_handle_slope) },
            "samples": samples.len(),
        },
        "faults": fault_list,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_echo_lines_and_slopes() {
        assert_eq!(ui_echo("PS> u1:a\n  u12:b   │ step 3 u: x\n"), Some((12, 'b')));
        assert_eq!(ui_echo("no echo here: menu"), None);
        let flat: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 60.0, 100.0)).collect();
        assert!(slope_per_hour(&flat).abs() < 1e-9);
        let growing: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 60.0, 100.0 + i as f64)).collect();
        assert!((slope_per_hour(&growing) - 60.0).abs() < 1e-6, "1 MB a minute is 60 MB an hour");
    }
}
