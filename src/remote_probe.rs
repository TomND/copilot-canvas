//! Remote-mode fault injection (`atelier selftest --remote faults`).
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
pub(crate) struct Proxy {
    pub(crate) addr: String,
    pub(crate) shared: Arc<ProxyShared>,
}

pub(crate) struct ProxyShared {
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
    pub(crate) connections: AtomicU64,
    down: AtomicBool,
    /// Relay nothing either way (a client that froze: it neither reads nor writes).
    stalled: AtomicBool,
    /// Bytes relayed from the service to clients.
    pub(crate) bytes_down: AtomicU64,
}

impl Proxy {
    pub(crate) fn start(upstream: String) -> Result<Self> {
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
            stalled: AtomicBool::new(false),
            bytes_down: AtomicU64::new(0),
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
    pub(crate) fn drop_all(&self) {
        for (client, service) in lock(&self.shared.links).drain(..) {
            client.close();
            service.close();
        }
    }

    pub(crate) fn cut_next_frame(&self, min_size: u64) {
        self.shared.cut_min.store(min_size, Ordering::Release);
        self.shared.cut.store(true, Ordering::Release);
    }

    /// Accept nothing (connections close at once), like a network that is down.
    pub(crate) fn set_down(&self, down: bool) {
        self.shared.down.store(down, Ordering::Release);
    }

    pub(crate) fn set_delay(&self, delay: Duration) {
        self.shared.delay_ms.store(delay.as_millis() as u64, Ordering::Release);
    }

    /// Stops relaying either way: the service's writes pile up until its
    /// socket's buffers are full, and the client's heartbeats stop.
    pub(crate) fn set_stalled(&self, stalled: bool) {
        self.shared.stalled.store(stalled, Ordering::Release);
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
            // What was read before a freeze is held too.
            while writer_shared.stalled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(20));
            }
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
            while shared.stalled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(20));
            }
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
                shared.bytes_down.fetch_add(packet.len() as u64, Ordering::Relaxed);
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
    /// Frames of the echo terminal received.
    frames: u64,
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
            frames: 0,
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
            self.frames += 1;
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
    let token = canvas_sys::token::read_token(&instance.token_path())?;
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
    let root = std::env::temp_dir().join(format!("atelier-faults-{}", uuid::Uuid::new_v4()));
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
    println!("Atelier remote fault injection (TCP proxy between a protocol-8 client and the service)");
    // The stand-in Copilot (`fake_copilot.rs`) for the agent steps: nothing reaches a real agent.
    let fake_cli = root.join("fake-cli");
    std::fs::create_dir_all(&fake_cli)?;
    let fake_copilot = fake_cli.join(canvas_sys::host::exe_name("copilot"));
    if std::fs::hard_link(exe, &fake_copilot).is_err() {
        std::fs::copy(exe, &fake_copilot)?;
    }
    let copilot_home = root.join("copilot-home");
    std::fs::create_dir_all(&copilot_home)?;
    let (fake_copilot, copilot_home) = (fake_copilot.to_string_lossy().into_owned(), copilot_home.to_string_lossy().into_owned());
    conn::spawn_service_exe(
        exe,
        instance,
        &[("ATELIER_LISTEN", "127.0.0.1:0"), ("ATELIER_COPILOT", &fake_copilot), (crate::fake_copilot::ENV, "1"), ("COPILOT_HOME", &copilot_home)],
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let port = loop {
        if let Some(port) = conn::listening_port(instance) {
            break port;
        }
        anyhow::ensure!(Instant::now() < deadline, "the service did not publish its TCP port; see {}", instance.log_path().display());
        std::thread::sleep(Duration::from_millis(100));
    };
    let token = canvas_sys::token::read_token(&instance.token_path())?;
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

    step("a client that freezes mid-stream (stops reading and writing) never blocks the service");
    let direct = ConnTarget::Tcp { addr: format!("127.0.0.1:{port}"), remote_id: None, secret: canvas_sys::token::read_token(&instance.token_path())? };
    let frozen = frozen_client(exe, instance, root, &direct, &mut typist)?;
    println!("{frozen}");

    step("after the freeze a new agent session shows its UI within 5 s");
    let shown = agent_after_freeze(instance, root, &direct)?;
    println!("    the stand-in agent drew its screen {:.2} s after the launch", shown.as_secs_f64());

    step("two connections resuming one session at once start exactly one process");
    let once = one_process_per_session(instance, root, &direct)?;
    println!("{once}");

    step("a protocol-7 client of the new service gets protocol-7 messages only");
    let (count, frames) = protocol7_client(instance)?;
    println!("    {count} messages ({frames} protocol-7 frames), none of a protocol-8 variant");

    step("the canvas over a dropped link: banner, input blocked while offline, reconnect and resync");
    let back = tui_over_dropped_link(exe, instance, root, &proxy)?;
    println!("    reconnected {:.2} s after the network came back; the pane shows its output again; offline typing never reached the shell", back.as_secs_f64());
    println!("PASS (probe reconnects: {})", times.join(", "));
    Ok(())
}

/// The silence after which the service drops a protocol-8 client.
const SILENCE: Duration = canvas_common::wire::DEAD_AFTER;

/// A protocol-8 client of its own that keeps every terminal's frames (and
/// what else it was told) apart.
struct Viewer {
    conn: Conn,
    views: std::collections::HashMap<String, TermView>,
    terminals: std::collections::HashMap<String, crate::protocol::TermInfo>,
    errors: Vec<(Option<String>, String)>,
}

impl Viewer {
    fn open(target: &ConnTarget, name: &str) -> Result<Self> {
        let options = ConnOptions { name: name.into(), ..Default::default() };
        let conn = conn::open(target, &options, &ClientSession::new())?;
        Ok(Self { conn, views: Default::default(), terminals: Default::default(), errors: Vec::new() })
    }

    fn subscribe(&self, ids: &[&str]) -> Result<()> {
        self.conn.tx.send(ClientMsg::Subscribe8 { ids: ids.iter().map(|id| (*id).to_owned()).collect(), focused: ids.first().map(|id| (*id).to_owned()) })?;
        Ok(())
    }

    fn send(&self, message: ClientMsg) -> Result<()> {
        self.conn.tx.send(message)?;
        Ok(())
    }

    fn pump(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.conn.rx.recv_timeout(left) {
                Ok(ServiceMsg::Frame8(frame)) => self.views.entry(frame.id.clone()).or_default().apply8(frame),
                Ok(ServiceMsg::Terminals(infos)) => {
                    for info in infos {
                        self.terminals.insert(info.id.clone(), info);
                    }
                }
                Ok(ServiceMsg::Error { id, message }) => self.errors.push((id, message)),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn text(&self, id: &str) -> String {
        self.views.get(id).map(|view| view.lines.iter().map(|runs| runs.iter().map(|r| r.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }
}

/// What `atelier service status` does: connect over the pipe and ask for the stats. Returns how long it took.
fn status_round_trip(instance: &Instance) -> Result<Duration> {
    let started = Instant::now();
    let conn = conn::try_connect(instance)?;
    conn.tx.send(ClientMsg::Stats)?;
    let deadline = started + Duration::from_secs(5);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ServiceMsg::Stats(_)) = conn.rx.recv_timeout(left) {
            return Ok(started.elapsed());
        }
    }
    bail!("the service did not answer a status request within 5 s")
}

/// The service log's lines as (ms, message).
fn log_lines(instance: &Instance) -> Vec<(u64, String)> {
    std::fs::read_to_string(instance.log_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (ms, rest) = line.split_once(' ')?;
            let message = rest.split_once("] ").map_or(rest, |(_, m)| m);
            Some((ms.parse().ok()?, message.to_owned()))
        })
        .collect()
}

/// A remote client that stops reading (and heartbeating) in the middle of a
/// stream of large frames, while another client types and the service is
/// asked for its status. Checks: every status answer under 1 s; the other
/// client's keystrokes keep coming back; the frozen client is dropped
/// within the silence timeout plus 1 s, closed once (logged once).
fn frozen_client(exe: &Path, instance: &Instance, root: &Path, direct: &ConnTarget, typist: &mut Typist) -> Result<String> {
    const NOISE: &str = "noise";
    let mut launcher = Viewer::open(direct, "noise-launcher")?;
    launcher.send(ClientMsg::Launch {
        id: NOISE.into(),
        kind: LaunchKind::Command { program: exe.to_string_lossy().into_owned(), args: vec!["noise".into(), "--rate".into(), "2000".into()] },
        cwd: root.to_string_lossy().into_owned(),
        cols: 220,
        rows: 70,
    })?;
    let stall_proxy = Proxy::start(match direct {
        ConnTarget::Tcp { addr, .. } => addr.clone(),
        _ => bail!("a TCP target"),
    })?;
    let token = canvas_sys::token::read_token(&instance.token_path())?;
    let mut frozen = Viewer::open(&ConnTarget::Tcp { addr: stall_proxy.addr.clone(), remote_id: None, secret: token }, "frozen-probe")?;
    frozen.subscribe(&[NOISE, ECHO])?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frozen.text(NOISE).contains("step") {
        anyhow::ensure!(Instant::now() < deadline, "the frozen client never got the noise terminal's frames");
        frozen.pump(Duration::from_millis(50));
        launcher.pump(Duration::ZERO);
    }
    let id: u64 = log_lines(instance)
        .iter()
        .rev()
        .find_map(|(_, line)| line.strip_prefix("remote client ").filter(|rest| rest.contains("connected (frozen-probe"))?.split_whitespace().next()?.parse().ok())
        .context("the service logged the frozen client's connection")?;
    // Freeze it in the middle of the stream: the service's writes back up.
    let frozen_at = crate::paths::now_ms();
    stall_proxy.set_stalled(true);
    let (mut worst, mut asked) = (Duration::ZERO, 0);
    let (frames_before, echoed_before) = (typist.frames, typist.echoed().len());
    let mut gone_at = None;
    let until = Instant::now() + SILENCE + Duration::from_secs(3);
    while Instant::now() < until {
        let took = status_round_trip(instance)?;
        anyhow::ensure!(took < Duration::from_secs(1), "service status took {:.2} s while a client was frozen", took.as_secs_f64());
        worst = worst.max(took);
        asked += 1;
        typist.type_keys(3, Duration::from_millis(60), |_, _| {})?;
        if gone_at.is_none() {
            gone_at = log_lines(instance).iter().find(|(ms, line)| *ms >= frozen_at && *line == format!("remote client {id} disconnected")).map(|(ms, _)| *ms);
        }
    }
    typist.pump(Duration::from_millis(300))?;
    let gone_at = gone_at.with_context(|| format!("the frozen client {id} was never dropped; see {}", instance.log_path().display()))?;
    let after = Duration::from_millis(gone_at.saturating_sub(frozen_at));
    anyhow::ensure!(after <= SILENCE + Duration::from_secs(1), "the frozen client was dropped {:.2} s after it froze (silence timeout {} s)", after.as_secs_f64(), SILENCE.as_secs());
    let closings = log_lines(instance).iter().filter(|(_, line)| line.starts_with(&format!("client {id} silent for "))).count();
    anyhow::ensure!(closings == 1, "the frozen client's close was logged {closings} times (once expected)");
    let (frames, echoed) = (typist.frames - frames_before, typist.echoed().len() - echoed_before);
    anyhow::ensure!(frames > 0 && echoed > 0, "the other client got {frames} frames and {echoed} echoed keys while a client was frozen");
    stall_proxy.set_stalled(false);
    stall_proxy.drop_all();
    launcher.send(ClientMsg::Stop { id: NOISE.into(), force: true })?;
    launcher.pump(Duration::from_millis(200));
    Ok(format!(
        "    client {id} froze mid-stream; dropped {:.2} s later (closing logged once); {asked} status requests answered (slowest {:.0} ms); the other client got {frames} frames and {echoed} echoed keys meanwhile",
        after.as_secs_f64(),
        worst.as_secs_f64() * 1000.0
    ))
}

/// Launches the stand-in Copilot after the freeze: its screen must be there within 5 s.
fn agent_after_freeze(instance: &Instance, root: &Path, direct: &ConnTarget) -> Result<Duration> {
    let _ = instance;
    let mut viewer = Viewer::open(direct, "agent-after-freeze")?;
    let id = uuid::Uuid::new_v4().to_string();
    viewer.subscribe(&[&id])?;
    let launched = Instant::now();
    viewer.send(ClientMsg::Launch { id: id.clone(), kind: LaunchKind::Copilot { new_name: None, prompt: None, resume: false }, cwd: root.to_string_lossy().into_owned(), cols: 100, rows: 30 })?;
    while !viewer.text(&id).contains("Fake Copilot") {
        if let Some((_, error)) = viewer.errors.first() {
            bail!("the launch failed: {error}");
        }
        anyhow::ensure!(launched.elapsed() < Duration::from_secs(5), "the new agent's box stayed blank for 5 s after the freeze:\n{}", viewer.text(&id));
        viewer.pump(Duration::from_millis(50));
    }
    let shown = launched.elapsed();
    viewer.send(ClientMsg::Stop { id, force: true })?;
    viewer.pump(Duration::from_millis(100));
    Ok(shown)
}

/// Two connections send `Launch` for one session at the same moment (a
/// frozen client's replayed resume and its reconnection's): exactly one
/// process starts. A third box resuming the same agent session is refused.
fn one_process_per_session(instance: &Instance, root: &Path, direct: &ConnTarget) -> Result<String> {
    let session = uuid::Uuid::new_v4().to_string();
    let launch = |native: Option<String>| LaunchKind::Agent(Box::new(crate::protocol::AgentLaunch { agent: "copilot".into(), resume: true, native_id: native, ..Default::default() }));
    let mut first = Viewer::open(direct, "resume-a")?;
    let mut second = Viewer::open(direct, "resume-b")?;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let senders: Vec<_> = [&first, &second]
        .iter()
        .map(|viewer| {
            let (tx, barrier, session, kind, cwd) = (viewer.conn.tx.clone(), barrier.clone(), session.clone(), launch(None), root.to_string_lossy().into_owned());
            std::thread::spawn(move || {
                barrier.wait();
                tx.send(ClientMsg::Launch { id: session, kind, cwd, cols: 100, rows: 30 }).is_ok()
            })
        })
        .collect();
    for sender in senders {
        anyhow::ensure!(sender.join().unwrap_or(false), "a launch could not be sent");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !first.terminals.get(&session).is_some_and(|t| t.alive) {
        anyhow::ensure!(Instant::now() < deadline, "the resumed session did not start: {:?} {:?}", first.errors, second.errors);
        first.pump(Duration::from_millis(50));
        second.pump(Duration::ZERO);
    }
    // Time for a second launch to show up if there were one.
    first.pump(Duration::from_millis(1500));
    second.pump(Duration::from_millis(100));
    let launched = log_lines(instance).iter().filter(|(_, line)| line.starts_with(&format!("launched {session} "))).count();
    anyhow::ensure!(launched == 1, "{launched} processes were started for one session (one expected)");
    // Another box resuming the same agent session while it runs: refused.
    let other = format!("other-{}", &session[..8]);
    first.send(ClientMsg::Launch { id: other.clone(), kind: launch(Some(session.clone())), cwd: root.to_string_lossy().into_owned(), cols: 100, rows: 30 })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !first.errors.iter().any(|(id, message)| id.as_deref() == Some(other.as_str()) && message.contains("already running")) {
        anyhow::ensure!(Instant::now() < deadline, "a second box resuming a running session was not refused: {:?}", first.errors);
        first.pump(Duration::from_millis(50));
    }
    let refused = log_lines(instance).iter().any(|(_, line)| line.starts_with(&format!("launched {other} ")));
    anyhow::ensure!(!refused, "the second box started a process");
    first.send(ClientMsg::Stop { id: session, force: true })?;
    first.pump(Duration::from_millis(200));
    Ok("    two concurrent resumes from two connections: one process; a third box resuming the running session was refused (\"already running\")".to_owned())
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
    let secret = crate::paths::hex(&canvas_sys::token::read_token(&instance.token_path())?);
    let mut ui = crate::selftest::Console::start_with_env(exe, &args, &[("ATELIER_REMOTE_SECRET", Path::new(&secret))], 170, 46)?;
    ui.wait("the canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
    ui.send(b"\r")?;
    ui.wait("PowerShell prompt", Duration::from_secs(20), |t| t.contains("PS "))?;
    ui.send(crate::test_shell::print("BEFORE_", "DROP").as_bytes())?;
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
    let after = crate::test_shell::print("AFTER_", "DROP");
    ui.send(after.as_bytes())?;
    ui.wait("typing works again", Duration::from_secs(10), |t| t.contains("AFTER_DROP"))?;
    let typed = format!("x{}", after.split_whitespace().next().unwrap_or_default());
    anyhow::ensure!(!ui.text().contains(&typed), "a key typed while offline reached the shell\n{}", ui.text());
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
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("release").join(canvas_sys::host::exe_name("atelier"))
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
