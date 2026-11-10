//! `atelier satellite`: the guest side of a VM session (docs/VM-SESSIONS.md
//! §6, plan VM-23). It reads `~/.atelier/satellite.json` (written by the
//! hub's launcher at a claim; a pool member waits for it), dials the hub
//! ([`dial`]), authenticates with its `sat-…` credential, and serves the
//! terminals the hub asks for ([`terms`]): their PTYs run here, their
//! screens are drawn on the hub. It reconnects after a drop (0.5 s, 1 s,
//! then every 1.5 s) and keeps the terminals running meanwhile; it ends
//! only when the hub proves the credential was revoked (the VM is deleted).

pub mod dial;
pub mod ring;
pub mod terms;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use canvas_common::handshake;
use canvas_common::protocol::{CAP_COMPRESS, CAP_FRAME8, CAP_SATELLITE, ClientInfo, ClientMsg, ServiceMsg};
use canvas_common::sat::{HelloInfo, SAT_PROTOCOL, SatDown, SatUp, WelcomeInfo};
use canvas_common::wire::{self, FrameReader, FrameWriter};
use crossbeam_channel::{Receiver, bounded, select_biased};
use serde::{Deserialize, Serialize};

use self::terms::{Lanes, SpawnRequest, Terms};

/// A link with nothing from the hub for this long is dropped (its heartbeat is every 2 s).
pub const SILENCE: Duration = Duration::from_secs(6);
const HEARTBEAT: Duration = Duration::from_secs(2);
const HANDSHAKE: Duration = Duration::from_secs(20);

/// `~/.atelier/satellite.json`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Config {
    /// The hub: `wss://…` (a Dev Tunnel's web address), `ws://…` or `tcp://host:port`.
    pub hub: String,
    /// The credential: `sat-…` and its secret (hex).
    pub id: String,
    pub secret: String,
    pub sandbox: String,
    /// A PEM file of extra roots for `wss://` (the pack's CA).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca: String,
    /// Extra headers on the WebSocket upgrade.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

pub fn config_path() -> PathBuf {
    super::guest::guest_home().join(".atelier").join("satellite.json")
}

/// The configuration once it is there and usable (a pool member waits here).
fn wait_config() -> Config {
    let mut said = false;
    loop {
        if let Some(config) = std::fs::read(config_path()).ok().and_then(|b| serde_json::from_slice::<Config>(&b).ok()).filter(|c| !c.hub.is_empty() && !c.id.is_empty() && !c.secret.is_empty()) {
            return config;
        }
        if !said {
            eprintln!("satellite: waiting for {}", config_path().display());
            said = true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn lock_path(home: &Path) -> PathBuf {
    home.join(".atelier").join("satellite.lock")
}

/// Whether a satellite runs for the guest home `home` (it holds its lock).
pub fn running(home: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path(home)) else { return false };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// `atelier satellite --start`: starts the satellite in the background
/// (a session of its own, its output in `~/.atelier/satellite.log`) unless
/// one runs; prints one JSON object.
pub fn start() -> Result<serde_json::Value> {
    let home = super::guest::guest_home();
    std::fs::create_dir_all(home.join(".atelier"))?;
    if running(&home) {
        return Ok(serde_json::json!({ "ok": true, "state": "running" }));
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(home.join(".atelier").join("satellite.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("satellite").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    canvas_sys::spawn::CommandExt::spawn_flags(&mut cmd, canvas_sys::spawn::DETACHED);
    let child = cmd.spawn().context("cannot start the satellite")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !running(&home) {
        anyhow::ensure!(Instant::now() < deadline, "the satellite did not start (~/.atelier/satellite.log)");
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(serde_json::json!({ "ok": true, "state": "started", "pid": child.id() }))
}

/// `atelier satellite`: runs until the credential is revoked (one per guest home).
pub fn run() -> Result<()> {
    let home = super::guest::guest_home();
    std::fs::create_dir_all(home.join(".atelier"))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path(&home))?;
    anyhow::ensure!(lock.try_lock().is_ok(), "a satellite runs for {} already", home.display());
    let terms = Arc::new(Terms::default());
    let epochs = AtomicU64::new(0);
    let mut failures = 0u32;
    loop {
        let config = wait_config();
        let epoch = epochs.fetch_add(1, Ordering::Relaxed) + 1;
        let started = Instant::now();
        match session(&config, &terms, epoch) {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<handshake::Refused>().is_some_and(|r| r.verified) => {
                eprintln!("satellite: the hub revoked this VM's credential; ending its terminals");
                terms.stop_all();
                return Ok(());
            }
            Err(error) => eprintln!("satellite: {error:#}"),
        }
        // A link that held a while starts the backoff over.
        failures = if started.elapsed() > Duration::from_secs(30) { 0 } else { failures + 1 };
        std::thread::sleep(Duration::from_millis(match failures {
            0 | 1 => 500,
            2 => 1000,
            _ => 1500,
        }));
    }
}

/// One link: dial, authenticate, `Hello`, then serve until it drops.
pub fn session(config: &Config, terms: &Arc<Terms>, epoch: u64) -> Result<()> {
    let secret = canvas_common::paths::unhex(&config.secret).context("the satellite's secret is not hex")?;
    let roots = if config.ca.is_empty() { Vec::new() } else { super::tls::pem_certificates(&std::fs::read(&config.ca).context("the pack's CA file")?)? };
    let dialed = dial::dial(&config.hub, roots, &config.headers)?;
    let closer = dialed.closer.clone();
    let mut reader = FrameReader::new(dialed.reader).with_limit(wire::PRE_AUTH_FRAME);
    let mut writer = FrameWriter::new(dialed.writer);
    // A hub that never answers must not hold the satellite.
    let (done_tx, done_rx) = bounded::<()>(1);
    {
        let closer = closer.clone();
        std::thread::Builder::new().name("sat-handshake".into()).spawn(move || {
            if done_rx.recv_timeout(HANDSHAKE).is_err() {
                closer.close();
            }
        })?;
    }
    let session_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let info = ClientInfo { name: "satellite".into(), caps: CAP_FRAME8 | CAP_SATELLITE | CAP_COMPRESS, session: session_id, remote_id: Some(config.id.clone()) };
    let opened = handshake::open(&mut reader, &mut writer, &secret, canvas_common::paths::build_id(), Some(info));
    let _ = done_tx.send(());
    if let Err(error) = opened {
        closer.close();
        return Err(error);
    }
    reader.set_limit(wire::MAX_FRAME);
    reader.accept_compressed()?;
    writer.compress()?;

    let (io_tx, io_rx) = bounded::<SatUp>(1024);
    let (other_tx, other_rx) = bounded::<SatUp>(1024);
    let hello = HelloInfo {
        sandbox: config.sandbox.clone(),
        build: canvas_common::paths::build_id().to_owned(),
        platform: std::env::consts::OS.to_owned(),
        home: super::guest::guest_home().display().to_string(),
        terms: terms.held(),
    };
    other_tx.send(SatUp::Hello { version: SAT_PROTOCOL, caps: 0, json: serde_json::to_string(&hello)? })?;
    let (beat_tx, beat_rx) = bounded::<()>(1);
    let writer_closer = closer.clone();
    let writing = std::thread::Builder::new().name("sat-writer".into()).spawn(move || write_lanes(writer, io_rx, other_rx, beat_rx, writer_closer))?;
    terms.attach(Lanes { io: io_tx.clone(), other: other_tx.clone(), epoch });

    let heard = Arc::new(AtomicU64::new(now()));
    let (stop_tx, stop_rx) = bounded::<()>(1);
    {
        let (heard, closer) = (heard.clone(), closer.clone());
        std::thread::Builder::new().name("sat-heartbeat".into()).spawn(move || {
            while stop_rx.recv_timeout(HEARTBEAT).is_err() {
                if now().saturating_sub(heard.load(Ordering::Relaxed)) > SILENCE.as_millis() as u64 {
                    closer.close();
                    return;
                }
                let _ = beat_tx.try_send(());
            }
        })?;
    }
    let result = read_loop(&mut reader, terms, &heard, &other_tx);
    terms.detach(epoch);
    let _ = stop_tx.send(());
    closer.close();
    drop((io_tx, other_tx));
    let _ = writing.join();
    result
}

fn now() -> u64 {
    canvas_common::paths::now_ms()
}

/// Terminal I/O first, then everything else, then the heartbeat.
fn write_lanes(mut writer: FrameWriter, io: Receiver<SatUp>, other: Receiver<SatUp>, beat: Receiver<()>, closer: wire::Closer) {
    loop {
        let frame = select_biased! {
            recv(io) -> m => m.ok().map(|m| ClientMsg::Sat(Box::new(m))),
            recv(other) -> m => m.ok().map(|m| ClientMsg::Sat(Box::new(m))),
            recv(beat) -> m => m.ok().map(|()| ClientMsg::Heartbeat),
        };
        let Some(frame) = frame else { break };
        if writer.send(&frame).is_err() {
            break;
        }
    }
    closer.close();
}

/// A control from the hub, answered on the link (`SatUp::ControlResult`):
/// `{"drain": DrainRequest}` runs the thin lane's drain here (the agents'
/// history the hub lacks, relayed commands, their answers stored).
fn control(json: &str) -> serde_json::Value {
    let request: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let answer = (|| -> Result<serde_json::Value> {
        let drain: super::lite::DrainRequest = serde_json::from_value(request.get("drain").cloned().context("an unknown control")?)?;
        let home = crate::paths::Instance::new(None, None, None)?.home;
        Ok(serde_json::to_value(super::lite::drain(&home, &super::lite::copilot_root(), &drain)?)?)
    })();
    match answer {
        Ok(value) => serde_json::json!({ "ok": true, "drain": value }),
        Err(error) => serde_json::json!({ "ok": false, "error": format!("{error:#}") }),
    }
}

fn read_loop(reader: &mut FrameReader, terms: &Arc<Terms>, heard: &AtomicU64, other: &crossbeam_channel::Sender<SatUp>) -> Result<()> {
    loop {
        let message = match reader.recv::<ServiceMsg>() {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(()),
            Err(error) => return Err(anyhow!("the link to the hub dropped: {error}")),
        };
        heard.store(now(), Ordering::Relaxed);
        let ServiceMsg::Sat(down) = message else { continue };
        match *down {
            SatDown::Welcome { json } => {
                let welcome: WelcomeInfo = serde_json::from_str(&json).unwrap_or_default();
                for term in welcome.terms {
                    terms.resync(&term.term, term.generation, term.have);
                }
            }
            SatDown::Spawn { term, generation, argv, env, cwd, cols, rows } => {
                let request = SpawnRequest { term: term.clone(), generation, argv, env, cwd, cols, rows };
                match terms.spawn(request) {
                    // A new program: its output goes out from its first byte.
                    Ok(true) => terms.resync(&term, generation, 0),
                    Ok(false) => {}
                    Err(error) => terms.refuse(&term, generation, format!("{error:#}")),
                }
            }
            SatDown::Input { term, generation, seq, bytes } => terms.input(&term, generation, seq, bytes),
            SatDown::Resize { term, generation, cols, rows } => terms.resize(&term, generation, cols, rows),
            SatDown::Signal { term, generation, signal } => terms.signal(&term, generation, signal),
            SatDown::Stop { term, generation, .. } => terms.stop(&term, generation),
            SatDown::Resync { term, generation, have } => terms.resync(&term, generation, have),
            SatDown::Control { id, json } => {
                let other = other.clone();
                std::thread::Builder::new().name("sat-control".into()).spawn(move || {
                    let _ = other.send(SatUp::ControlResult { id, json: control(&json).to_string() });
                })?;
            }
            // Files, notes and MCP come with Phase 3.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The satellite and the hub's link end to end in one process: a real
    //! handshake over loopback through a relay the test can cut.

    use super::*;
    use crate::vm::launch;
    use canvas_common::handshake::Opening;
    use canvas_host::emulator::Engine;
    use canvas_host::server::SatelliteLink;
    use canvas_host::term::{Hooks, RemoteSpawn, Terminal};
    use canvas_model::vm::VmState;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    const SANDBOX: &str = "sbx-cccccccccccc";
    const BOX: &str = "0b1e5ca1-0000-4000-8000-0000000d4095";
    const LINES: u32 = 3000;

    /// The hub: serves every connection as a satellite link of `links`.
    fn hub(home: &Path, links: &Arc<crate::vm::link::Links>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (home, links) = (home.to_path_buf(), links.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (home, links) = (home.clone(), links.clone());
                std::thread::spawn(move || {
                    let closing = stream.try_clone().unwrap();
                    let mut reader = FrameReader::new(Box::new(stream.try_clone().unwrap()));
                    let mut writer = FrameWriter::new(Box::new(stream));
                    let store = crate::vm::store::Satellites::new(&home);
                    let Ok(Opening::Client(accepted)) = handshake::serve(&mut reader, &mut writer, true, canvas_common::paths::build_id(), |id| store.credential(id.unwrap_or_default()), None) else { return };
                    assert!(accepted.satellite);
                    reader.accept_compressed().unwrap();
                    writer.compress().unwrap();
                    let closer = wire::Closer::new(move || {
                        let _ = closing.shutdown(Shutdown::Both);
                    });
                    let _ = links.serve(SatelliteLink { reader, writer, closer, info: accepted.client.unwrap_or_default(), websocket: false });
                });
            }
        });
        port
    }

    /// A relay to `port` whose connections [`Relay::cut`] ends (it keeps accepting new ones).
    struct Relay {
        port: u16,
        open: Arc<Mutex<Vec<TcpStream>>>,
        paused: Arc<AtomicBool>,
    }

    impl Relay {
        fn start(to: u16) -> Relay {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let open: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
            let paused = Arc::new(AtomicBool::new(false));
            let (kept, held) = (open.clone(), paused.clone());
            std::thread::spawn(move || {
                for client in listener.incoming().flatten() {
                    if held.load(Ordering::Acquire) {
                        let _ = client.shutdown(Shutdown::Both);
                        continue;
                    }
                    let Ok(upstream) = TcpStream::connect(("127.0.0.1", to)) else { continue };
                    kept.lock().unwrap().extend([client.try_clone().unwrap(), upstream.try_clone().unwrap()]);
                    for (mut from, mut into) in [(client.try_clone().unwrap(), upstream.try_clone().unwrap()), (upstream, client)] {
                        std::thread::spawn(move || {
                            let _ = std::io::copy(&mut from, &mut into);
                            let _ = into.shutdown(Shutdown::Both);
                        });
                    }
                }
            });
            Relay { port, open, paused }
        }

        /// Drops every connection; new ones are refused while `paused`.
        fn cut(&self, paused: bool) {
            self.paused.store(paused, Ordering::Release);
            for stream in self.open.lock().unwrap().drain(..) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    fn wait_until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A program printing numbered lines: 100, a pause (the test cuts the
    /// link then), the rest while the hub is away, then waiting.
    fn printer() -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), format!("(for /l %i in (0,1,99) do @echo line-%i) & ping -n 4 127.0.0.1 > nul & (for /l %i in (100,1,{}) do @echo line-%i) & ping -n 60 127.0.0.1 > nul", LINES - 1)]
        } else {
            vec!["sh".into(), "-c".into(), format!("i=0; while [ $i -lt 100 ]; do echo line-$i; i=$((i+1)); done; sleep 3; while [ $i -lt {LINES} ]; do echo line-$i; i=$((i+1)); done; sleep 60")]
        }
    }

    /// The satellite (its terminal already started) linked through `relay`,
    /// the hub's terminal adopted, the hub's log.
    #[allow(clippy::type_complexity)]
    fn setup(ring: usize) -> (PathBuf, Arc<Terms>, Arc<Terminal>, Relay, Arc<AtomicBool>, Arc<Mutex<Vec<String>>>) {
        let home = std::env::temp_dir().join(format!("atelier-sat-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let minted = crate::vm::store::Satellites::new(&home).mint(SANDBOX).unwrap();
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let links = crate::vm::link::Links::new(&home, {
            let log = log.clone();
            Arc::new(move |line| log.lock().unwrap().push(line))
        });
        let mut intent = launch::Intent { r#box: BOX.into(), agent: "copilot".into(), sandbox: SANDBOX.into(), state: VmState::Running, terminal: 7, ..Default::default() };
        launch::save(&home, &mut intent).unwrap();
        let made = Arc::new(Mutex::new(None));
        links.adopt(|id, remote| {
            let (pump, _) = bounded(1);
            let (exited, _) = crossbeam_channel::unbounded();
            let (clipboard, _) = bounded(1);
            let (changed, _) = bounded(16);
            let hooks = Hooks { pump, exited, clipboard, bytes: Default::default(), changed, notify: None, raw: None };
            let (terminal, feed) = Terminal::spawn_remote(RemoteSpawn { id, cols: 60, rows: 12, copilot: false, engine: Engine::Alacritty }, hooks, remote.clone());
            canvas_host::term::RemotePty::started(remote.as_ref(), feed, 60, 12);
            *made.lock().unwrap() = Some(terminal);
        });
        let terminal = made.lock().unwrap().take().unwrap();
        let terms = Arc::new(Terms::with_ring(ring));
        let cwd = std::env::temp_dir().display().to_string();
        terms.spawn(terms::SpawnRequest { term: BOX.into(), generation: 7, argv: printer(), env: Vec::new(), cwd, cols: 60, rows: 12 }).unwrap();
        let relay = Relay::start(hub(&home, &links));
        let config = Config { hub: format!("tcp://127.0.0.1:{}", relay.port), id: minted.id, secret: minted.secret, sandbox: SANDBOX.into(), ..Default::default() };
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (terms, stop) = (terms.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut epoch = 0;
                while !stop.load(Ordering::Acquire) {
                    epoch += 1;
                    let _ = session(&config, &terms, epoch);
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
        (home, terms, terminal, relay, stop, log)
    }

    fn screen(terminal: &Terminal) -> Vec<String> {
        terminal.snapshot().map(|s| s.lines.iter().map(|row| row.iter().map(|r| r.text.as_str()).collect::<String>().trim_end().to_owned()).collect()).unwrap_or_default()
    }

    fn last_line() -> String {
        format!("line-{}", LINES - 1)
    }

    #[test]
    fn a_link_dropped_during_output_is_replayed_exactly() {
        let (home, terms, terminal, relay, stop, log) = setup(0);
        wait_until("the hub got some output", 30, || screen(&terminal).iter().any(|l| l.starts_with("line-")));
        relay.cut(true);
        assert!(!screen(&terminal).iter().any(|l| *l == last_line()), "cut before the end");
        // The program goes on writing while the hub is away.
        wait_until("the program finished its output", 30, || terms.held().iter().any(|h| h.end > 0) && { let a = terms.held()[0].end; std::thread::sleep(Duration::from_millis(300)); terms.held()[0].end == a && a > 3 * LINES as u64 });
        relay.cut(false);
        wait_until("the replay reached the hub", 30, || screen(&terminal).iter().any(|l| *l == last_line()));
        let lines = screen(&terminal);
        let numbers: Vec<u32> = lines.iter().filter_map(|l| l.strip_prefix("line-")?.parse().ok()).collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "no line lost or repeated: {lines:?}");
        assert!(!log.lock().unwrap().iter().any(|l| l.contains("repainted")), "replayed, not repainted");
        stop.store(true, Ordering::Release);
        relay.cut(true);
        terms.stop_all();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_drop_longer_than_the_ring_repaints_the_guests_screen() {
        let (home, terms, terminal, relay, stop, log) = setup(4 << 10);
        wait_until("the hub got some output", 30, || screen(&terminal).iter().any(|l| l.starts_with("line-")));
        relay.cut(true);
        assert!(!screen(&terminal).iter().any(|l| *l == last_line()), "cut before the end");
        wait_until("the program wrote more than the ring holds", 30, || { let a = terms.held()[0].end; std::thread::sleep(Duration::from_millis(300)); terms.held()[0].end == a && a > 3 * LINES as u64 });
        relay.cut(false);
        wait_until("the hub shows the guest's screen", 30, || screen(&terminal).iter().any(|l| *l == last_line()));
        let lines = screen(&terminal);
        let numbers: Vec<u32> = lines.iter().filter_map(|l| l.strip_prefix("line-")?.parse().ok()).collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1) && numbers.last() == Some(&(LINES - 1)), "the guest's last screen: {lines:?}");
        assert!(log.lock().unwrap().iter().any(|l| l.contains("repainted")), "beyond the ring: the guest's screen was repainted");
        stop.store(true, Ordering::Release);
        relay.cut(true);
        terms.stop_all();
        let _ = std::fs::remove_dir_all(home);
    }
}

