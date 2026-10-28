//! Remote-mode fault injection (`copilot-canvas selftest --remote faults`).
//!
//! A TCP proxy sits between a protocol-8 client (`canvas_core::conn`, driven
//! like the canvas drives it) and a real service. While keystrokes stream to
//! an `echo` terminal the proxy drops the connection, cuts it in the middle of
//! a frame, and delays it by 300 ms each way. Checks: the client reconnects
//! within 5 s and gets full frames again; every acknowledged keystroke reached
//! the terminal exactly once and in order; nothing arrived twice; keystrokes
//! the service never confirmed are reported, never resent.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, bounded, unbounded};

use crate::client::app::TermView;
use crate::client::conn::{self, ClientSession, Conn, ConnOptions, ConnTarget};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, Input, Key, LaunchKind, ServiceMsg};
use canvas_common::tcp::{self, TcpCloser};
use canvas_common::wire::COMPRESSED;

const ECHO: &str = "echo";
/// Reconnecting must take less than this (PLAN §9 asks ≤ 2 s once the network is back).
const RECONNECT_LIMIT: Duration = Duration::from_secs(5);

/// A TCP proxy that can drop, cut and delay its connections.
struct Proxy {
    addr: String,
    shared: Arc<ProxyShared>,
}

struct ProxyShared {
    upstream: String,
    links: Mutex<Vec<(TcpCloser, TcpCloser)>>,
    /// Added latency per direction.
    delay_ms: AtomicU64,
    /// Cut the next service frame of at least `cut_min` bytes in its middle.
    cut: AtomicBool,
    cut_min: AtomicU64,
    /// Size of the last frame that was cut, and whether it was compressed.
    cut_size: AtomicU64,
    cut_compressed: AtomicBool,
    connections: AtomicU64,
    down: AtomicBool,
}

impl Proxy {
    fn start(upstream: String) -> Result<Self> {
        let listener = tcp::listen_loopback("127.0.0.1:0")?;
        let addr = listener.local_addr()?.to_string();
        let shared = Arc::new(ProxyShared {
            upstream,
            links: Mutex::new(Vec::new()),
            delay_ms: AtomicU64::new(0),
            cut: AtomicBool::new(false),
            cut_min: AtomicU64::new(16),
            cut_size: AtomicU64::new(0),
            cut_compressed: AtomicBool::new(false),
            connections: AtomicU64::new(0),
            down: AtomicBool::new(false),
        });
        let accept = shared.clone();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                if accept.down.load(Ordering::Acquire) {
                    drop(client);
                    continue;
                }
                let Ok(upstream) = tcp::connect(&accept.upstream, Duration::from_secs(2)) else { continue };
                let (Ok((cr, cw, cc)), Ok((ur, uw, uc))) = (tcp::split(client), tcp::split(upstream)) else { continue };
                accept.connections.fetch_add(1, Ordering::Relaxed);
                lock(&accept.links).push((cc.clone(), uc.clone()));
                relay(Box::new(cr), Box::new(uw), [cc.clone(), uc.clone()], accept.clone(), false);
                relay(Box::new(ur), Box::new(cw), [cc, uc], accept.clone(), true);
            }
        });
        Ok(Self { addr, shared })
    }

    /// Closes every connection (both sides see it end).
    fn drop_all(&self) {
        for (client, service) in lock(&self.shared.links).drain(..) {
            client.close();
            service.close();
        }
    }

    fn cut_next_frame(&self, min_size: u64) {
        self.shared.cut_min.store(min_size, Ordering::Release);
        self.shared.cut.store(true, Ordering::Release);
    }

    /// Accept nothing (connections close at once), like a network that is down.
    fn set_down(&self, down: bool) {
        self.shared.down.store(down, Ordering::Release);
    }

    fn set_delay(&self, delay: Duration) {
        self.shared.delay_ms.store(delay.as_millis() as u64, Ordering::Release);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Copies one direction. Chunks are forwarded `delay` after they arrived (pure
/// latency, no throughput limit). `frames`: the service-to-client direction,
/// read frame by frame so a frame can be cut in its middle.
fn relay(mut from: Box<dyn Read + Send>, mut to: Box<dyn Write + Send>, ends: [TcpCloser; 2], shared: Arc<ProxyShared>, frames: bool) {
    let (tx, rx) = unbounded::<(Instant, Vec<u8>, bool)>();
    let (writer_ends, writer_shared) = (ends.clone(), shared.clone());
    std::thread::spawn(move || {
        for (arrived, bytes, last) in rx {
            let due = arrived + Duration::from_millis(writer_shared.delay_ms.load(Ordering::Acquire));
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if to.write_all(&bytes).is_err() || last {
                break;
            }
        }
        for end in &writer_ends {
            end.close();
        }
    });
    std::thread::spawn(move || {
        loop {
            if frames {
                let mut header = [0u8; 4];
                if from.read_exact(&mut header).is_err() {
                    break;
                }
                let len = (u32::from_le_bytes(header) & !COMPRESSED) as usize;
                let mut body = vec![0u8; len];
                if from.read_exact(&mut body).is_err() {
                    break;
                }
                let mut packet = header.to_vec();
                let compressed = u32::from_le_bytes(header) & COMPRESSED != 0;
                if len as u64 >= shared.cut_min.load(Ordering::Acquire) && shared.cut.swap(false, Ordering::AcqRel) {
                    packet.extend_from_slice(&body[..len / 2]);
                    shared.cut_size.store(len as u64 + 4, Ordering::Release);
                    shared.cut_compressed.store(compressed, Ordering::Release);
                    let _ = tx.send((Instant::now(), packet, true));
                    break;
                }
                packet.extend_from_slice(&body);
                if tx.send((Instant::now(), packet, false)).is_err() {
                    break;
                }
            } else {
                let mut buf = vec![0u8; 64 * 1024];
                match from.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send((Instant::now(), buf, false)).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        // The writer forwards what is queued, then closes both sides.
    });
}

/// A protocol-8 client that types into the echo terminal and reconnects like
/// the canvas does: input is blocked (not queued) while offline.
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
    /// Seqs the service never confirmed (reported at reconnect, never resent).
    lost: Vec<(u64, u64)>,
    /// Keystrokes typed while offline (dropped, as the canvas does).
    blocked: u64,
    reconnects: Vec<Duration>,
    reconnected_at: Vec<Instant>,
    /// For each reconnect: whether the first echo frame afterwards was full.
    full_after: Vec<bool>,
    awaiting_full: bool,
}

impl Typist {
    fn connect(target: ConnTarget) -> Result<Self> {
        let options = ConnOptions { name: "faults".into(), ..Default::default() };
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
            reconnects: Vec::new(),
            reconnected_at: Vec::new(),
            full_after: Vec::new(),
            awaiting_full: false,
        })
    }

    fn online(&self) -> bool {
        self.conn.is_some()
    }

    /// Handles messages (and reconnects) for `wait`.
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
        if let ServiceMsg::Frame8(frame) = message
            && frame.id == ECHO
        {
            if std::mem::take(&mut self.awaiting_full) {
                self.full_after.push(frame.full);
            }
            self.view.apply8(frame);
        }
    }

    fn lost_connection(&mut self) {
        self.conn = None;
        self.offline_since = Some(Instant::now());
        let (tx, rx) = bounded(1);
        let (target, options, session) = (self.target.clone(), self.options.clone(), self.session.clone());
        std::thread::spawn(move || {
            let _ = tx.send(conn::reconnect(&target, &options, &session, |attempt, _| attempt < 10));
        });
        self.reconnect = Some(rx);
    }

    fn swap(&mut self, conn: Conn) -> Result<()> {
        let (ready, sent) = (self.session.acked(), self.session.sent());
        let unconfirmed = self.session.settle();
        anyhow::ensure!(unconfirmed == sent.saturating_sub(ready), "settle reports what Ready8 did not confirm");
        if unconfirmed > 0 {
            self.lost.push((ready + 1, sent));
        }
        self.reconnects.push(self.offline_since.take().map(|t| t.elapsed()).unwrap_or_default());
        self.reconnected_at.push(Instant::now());
        // The service starts this client from scratch: subscribe again.
        conn.tx.send(ClientMsg::Subscribe8 { ids: vec![ECHO.into()], focused: Some(ECHO.into()) })?;
        self.awaiting_full = true;
        self.conn = Some(conn);
        Ok(())
    }

    /// Types one key; offline it is dropped (never queued).
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
        // The connection numbers the key when it writes it.
        let deadline = Instant::now() + Duration::from_millis(300);
        while self.session.sent() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        match self.session.sent() {
            seq if seq > before => {
                self.keys.insert(seq, key);
            }
            // The connection died with the key still queued: never sent.
            _ => self.blocked += 1,
        }
    }

    /// Types `count` keys `every` apart; `at` runs before key `index`.
    fn type_keys(&mut self, count: usize, every: Duration, mut at: impl FnMut(usize, &mut Self)) -> Result<()> {
        for index in 0..count {
            at(index, self);
            let typed = self.keys.len() as u64 + self.blocked;
            self.type_key((b'a' + (typed % 26) as u8) as char);
            self.pump(every)?;
        }
        Ok(())
    }

    fn wait_online(&mut self) -> Result<Duration> {
        let deadline = Instant::now() + RECONNECT_LIMIT + Duration::from_secs(5);
        while !self.online() || self.awaiting_full {
            anyhow::ensure!(Instant::now() < deadline, "no reconnect");
            self.pump(Duration::from_millis(20))?;
        }
        let took = *self.reconnects.last().context("a reconnect")?;
        anyhow::ensure!(took < RECONNECT_LIMIT, "reconnecting took {:.2} s", took.as_secs_f64());
        anyhow::ensure!(self.full_after.last() == Some(&true), "the first frame after reconnecting is full");
        Ok(took)
    }

    /// Keys the service applied: all handed out, minus unconfirmed ranges.
    fn applied(&self) -> Vec<char> {
        self.keys.iter().filter(|(seq, _)| !self.lost.iter().any(|(a, b)| (a..=b).contains(seq))).map(|(_, c)| *c).collect()
    }

    fn unconfirmed(&self) -> u64 {
        self.lost.iter().map(|(a, b)| b - a + 1).sum()
    }

    /// `n:c` lines the echo terminal printed, in order.
    fn echoed(&self) -> Vec<(u64, char)> {
        self.view
            .lines
            .iter()
            .filter_map(|runs| {
                let text: String = runs.iter().map(|r| r.text.as_str()).collect();
                let (n, c) = text.trim().split_once(':')?;
                let mut chars = c.chars();
                Some((n.parse().ok()?, chars.next().filter(|_| chars.next().is_none())?))
            })
            .collect()
    }
}

/// Speaks protocol 7 (`Hello`) over the pipe: subscribes to the echo terminal,
/// types a key and checks that every message is a protocol-7 variant for 3 s
/// (longer than a heartbeat interval). Returns (messages, frames).
fn protocol7_client(instance: &Instance) -> Result<(usize, usize)> {
    use canvas_common::protocol::SERVICE_V7_VARIANTS;
    use canvas_common::wire::{FrameReader, FrameWriter};
    let pipe = crate::client::pipe::connect(&instance.pipe_name(), Duration::from_secs(2))?;
    let token = canvas_common::auth::read_token(&instance.token_path())?;
    let (reader, writer, closer) = pipe.split()?;
    let (mut reader, mut writer) = (FrameReader::new(Box::new(reader)), FrameWriter::new(Box::new(writer)));
    let greeting = canvas_common::handshake::open(&mut reader, &mut writer, &token, "protocol-7 probe", None)?;
    anyhow::ensure!(greeting.protocol == 7, "the service answered protocol {} to Hello", greeting.protocol);
    writer.send(&ClientMsg::Subscribe { ids: vec![ECHO.into()] })?;
    writer.send(&ClientMsg::Input { id: ECHO.into(), input: Input::Key { key: Key::Char('z'), mods: 0 } })?;
    let (tx, rx) = unbounded::<ServiceMsg>();
    std::thread::spawn(move || {
        while let Ok(Some(message)) = reader.recv::<ServiceMsg>() {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let (mut count, mut frames, mut ready) = (0, 0, false);
    while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
        let Ok(message) = rx.recv_timeout(wait) else { break };
        let index = postcard::to_stdvec(&message)?[0];
        anyhow::ensure!(index < SERVICE_V7_VARIANTS, "a protocol-7 client got {message:?}");
        count += 1;
        frames += matches!(message, ServiceMsg::Frame(_)) as usize;
        ready |= matches!(message, ServiceMsg::Ready { .. });
    }
    closer.close();
    anyhow::ensure!(ready && frames > 0, "a protocol-7 client gets Ready and frames ({count} messages, {frames} frames)");
    Ok((count, frames))
}

pub fn faults(exe: &Path) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-faults-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    let instance = Instance::new(Some(format!("faults{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let result = scenario(exe, &instance, &root);
    if let Ok(conn) = conn::try_connect(&instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_millis(1500));
    }
    match &result {
        Ok(()) => {
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err(_) => eprintln!("artifacts kept in {}", root.display()),
    }
    result
}

fn step(text: &str) {
    println!("  · {text}");
}

fn scenario(exe: &Path, instance: &Instance, root: &Path) -> Result<()> {
    println!("Copilot Canvas remote fault injection (TCP proxy between a protocol-8 client and the service)");
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
    let target = ConnTarget::Tcp { addr: proxy.addr.clone(), remote_id: None, secret: token };
    let mut typist = Typist::connect(target)?;
    let launch = ClientMsg::Launch {
        id: ECHO.into(),
        kind: LaunchKind::Command { program: exe.to_string_lossy().into_owned(), args: vec!["echo".into()] },
        cwd: root.to_string_lossy().into_owned(),
        cols: 40,
        rows: 400,
    };
    typist.conn.as_ref().context("connected")?.tx.send(launch)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !typist.view.lines.iter().any(|l| l.iter().any(|r| r.text.contains("echo ready"))) {
        anyhow::ensure!(Instant::now() < deadline, "the echo terminal did not start");
        typist.pump(Duration::from_millis(50))?;
    }
    typist.type_keys(15, Duration::from_millis(10), |_, _| {})?;

    step("drop the connection while keystrokes stream");
    let mut drop_at = None;
    typist.type_keys(40, Duration::from_millis(8), |index, _| {
        if index == 20 {
            proxy.drop_all();
            drop_at = Some(Instant::now());
        }
    })?;
    let took = typist.wait_online()?;
    let since_drop = match (drop_at, typist.reconnected_at.last()) {
        (Some(dropped), Some(back)) => back.duration_since(dropped),
        _ => took,
    };
    anyhow::ensure!(since_drop < RECONNECT_LIMIT, "reconnected {:.2} s after the drop", since_drop.as_secs_f64());
    println!(
        "    reconnected {:.2} s after the drop; full frame resent; {} keystroke(s) unconfirmed (reported, not resent); {} typed while offline (blocked, not queued)",
        since_drop.as_secs_f64(),
        typist.unconfirmed(),
        typist.blocked
    );
    typist.type_keys(15, Duration::from_millis(10), |_, _| {})?;

    step("cut the connection in the middle of a frame");
    let before = typist.reconnects.len();
    typist.type_keys(30, Duration::from_millis(10), |index, t| {
        if index == 10 {
            // Resubscribing makes the service send a full frame: cut that one
            // (or a keystroke's frame if it comes first; heartbeats are smaller).
            proxy.cut_next_frame(32);
            if let Some(conn) = &t.conn {
                let _ = conn.tx.send(ClientMsg::Subscribe8 { ids: Vec::new(), focused: None });
                let _ = conn.tx.send(ClientMsg::Subscribe8 { ids: vec![ECHO.into()], focused: Some(ECHO.into()) });
            }
        }
    })?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while typist.reconnects.len() == before || !typist.online() || typist.awaiting_full {
        anyhow::ensure!(Instant::now() < deadline, "the client did not notice the cut frame");
        typist.pump(Duration::from_millis(20))?;
    }
    let took = typist.wait_online()?;
    println!(
        "    a {}-byte {}frame was cut after half; the partial frame was discarded; reconnected in {:.2} s; full frame resent; {} keystroke(s) unconfirmed so far",
        proxy.shared.cut_size.load(Ordering::Acquire),
        if proxy.shared.cut_compressed.load(Ordering::Acquire) { "compressed " } else { "" },
        took.as_secs_f64(),
        typist.unconfirmed()
    );
    typist.type_keys(10, Duration::from_millis(10), |_, _| {})?;

    step("300 ms delay each way while typing");
    proxy.set_delay(Duration::from_millis(300));
    let before = typist.reconnects.len();
    let started = typist.session.sent();
    typist.type_keys(20, Duration::from_millis(50), |_, _| {})?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while typist.session.acked() < typist.session.sent() {
        anyhow::ensure!(Instant::now() < deadline, "keystrokes over the delayed link were not acknowledged");
        typist.pump(Duration::from_millis(50))?;
    }
    anyhow::ensure!(typist.reconnects.len() == before, "a 300 ms delay must not drop the connection");
    proxy.set_delay(Duration::ZERO);
    println!("    {} keystrokes acknowledged over the delayed link; no reconnect", typist.session.sent() - started);

    step("every acknowledged keystroke reached the terminal exactly once, in order");
    let deadline = Instant::now() + Duration::from_secs(8);
    while typist.session.acked() < typist.session.sent() || typist.echoed().len() < typist.applied().len() {
        if Instant::now() >= deadline {
            break;
        }
        typist.pump(Duration::from_millis(50))?;
    }
    let applied = typist.applied();
    let echoed = typist.echoed();
    let numbers_ok = echoed.iter().enumerate().all(|(i, (n, _))| *n == i as u64 + 1);
    let keys: Vec<char> = echoed.iter().map(|(_, c)| *c).collect();
    if keys != applied || !numbers_ok {
        bail!(
            "the echo terminal does not show exactly the applied keystrokes\napplied ({}): {}\nechoed  ({}): {}",
            applied.len(),
            applied.iter().collect::<String>(),
            keys.len(),
            keys.iter().collect::<String>()
        );
    }
    println!(
        "    {} keystrokes handed to the transport, {} applied exactly once, 0 duplicates, {} unconfirmed and reported (none resent), {} blocked while offline; {} connections through the proxy",
        typist.keys.len(),
        applied.len(),
        typist.unconfirmed(),
        typist.blocked,
        proxy.shared.connections.load(Ordering::Relaxed)
    );
    let times: Vec<String> = typist.reconnects.iter().map(|d| format!("{:.2} s", d.as_secs_f64())).collect();

    step("a protocol-7 client of the new service gets protocol-7 messages only");
    let (count, frames) = protocol7_client(instance)?;
    println!("    {count} messages ({frames} protocol-7 frames), none of a protocol-8 variant");

    step("the canvas over a dropped link: banner, input blocked while offline, reconnect and resync");
    let back = tui_over_dropped_link(exe, instance, root, &proxy)?;
    println!("    reconnected {:.2} s after the network came back; the pane shows its output again; offline typing never reached the shell", back.as_secs_f64());
    println!("PASS (probe reconnects: {})", times.join(", "));
    Ok(())
}

/// Runs the canvas (`--demo --remote HOST:PORT` through the proxy) in a hidden
/// console, takes the network down and up again. Returns how long after the
/// network came back the canvas reconnected.
fn tui_over_dropped_link(exe: &Path, instance: &Instance, root: &Path, proxy: &Proxy) -> Result<Duration> {
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    let args: Vec<String> = vec![
        "--demo".into(),
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--sessions-dir".into(),
        instance.sessions_dir.to_string_lossy().into_owned(),
        "--cwd".into(),
        work.to_string_lossy().into_owned(),
        "--remote".into(),
        proxy.addr.clone(),
    ];
    let secret = crate::paths::hex(&canvas_common::auth::read_token(&instance.token_path())?);
    let mut ui = crate::selftest::Console::start_with_env(exe, &args, &[("COPILOT_CANVAS_REMOTE_SECRET", Path::new(&secret))], 170, 46)?;
    ui.wait("the canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
    ui.send(b"\r")?;
    ui.wait("PowerShell prompt", Duration::from_secs(20), |t| t.contains("PS "))?;
    ui.send(b"Write-Output ('BEFORE_' + 'DROP')\r")?;
    ui.wait("output before the drop", Duration::from_secs(10), |t| t.contains("BEFORE_DROP"))?;
    proxy.set_down(true);
    proxy.drop_all();
    ui.wait("offline banner", Duration::from_secs(10), |t| t.contains("Offline — reconnecting"))?;
    ui.send(b"x")?;
    ui.wait("offline input blocked", Duration::from_secs(5), |t| t.contains("Offline: input not sent"))?;
    let up = Instant::now();
    proxy.set_down(false);
    ui.wait("reconnected", RECONNECT_LIMIT + Duration::from_secs(1), |t| t.contains("Reconnected") && !t.contains("Offline — reconnecting"))?;
    let back = up.elapsed();
    anyhow::ensure!(back < RECONNECT_LIMIT, "the canvas reconnected {:.2} s after the network came back", back.as_secs_f64());
    ui.wait("the pane resynced", Duration::from_secs(5), |t| t.contains("BEFORE_DROP"))?;
    ui.send(b"Write-Output ('AFTER_' + 'DROP')\r")?;
    ui.wait("typing works again", Duration::from_secs(10), |t| t.contains("AFTER_DROP"))?;
    anyhow::ensure!(!ui.text().contains("xWrite-Output"), "a key typed while offline reached the shell\n{}", ui.text());
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
    ui.send(b"\r")?;
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "the canvas exited with {code}");
    Ok(back)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    fn release_exe() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe")
    }

    /// Needs the release build: `cargo build --release`, then
    /// `cargo test --release fault_injection -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn fault_injection_probe() {
        super::faults(&release_exe()).unwrap();
    }

    /// The local canvas (pipe) when its service goes away: banner, a new
    /// service is started, the canvas reconnects and works again.
    /// `cargo test --release local_reconnect -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn local_reconnect_probe() {
        let root = std::env::temp_dir().join(format!("cc-local-reconnect-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("reconnect{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions"))).unwrap();
        let args: Vec<String> = vec![
            "--demo".into(),
            "--instance".into(),
            instance.name.clone(),
            "--home".into(),
            instance.home.to_string_lossy().into_owned(),
            "--sessions-dir".into(),
            instance.sessions_dir.to_string_lossy().into_owned(),
            "--cwd".into(),
            work.to_string_lossy().into_owned(),
        ];
        let mut ui = crate::selftest::Console::start(&release_exe(), &args, 170, 46).unwrap();
        ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Build session canvas")).unwrap();
        ui.send(b"\r").unwrap();
        ui.wait("prompt", Duration::from_secs(20), |t| t.contains("PS ")).unwrap();
        let old = crate::client::conn::try_connect(&instance).unwrap();
        old.tx.send(crate::protocol::ClientMsg::Shutdown).unwrap();
        ui.wait("offline banner", Duration::from_secs(10), |t| t.contains("Offline — reconnecting")).unwrap();
        ui.wait("reconnected to a new service", Duration::from_secs(20), |t| t.contains("Reconnected")).unwrap();
        ui.send(b"\x1d").unwrap();
        ui.send(b"\r").unwrap();
        ui.wait("the session starts again", Duration::from_secs(20), |t| t.contains("PS ")).unwrap();
        ui.send(b"\x1d").unwrap();
        ui.send(b"q").unwrap();
        ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)")).unwrap();
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
