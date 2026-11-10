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
    let result = read_loop(&mut reader, terms, &heard);
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

fn read_loop(reader: &mut FrameReader, terms: &Arc<Terms>, heard: &AtomicU64) -> Result<()> {
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
            // Files, notes, MCP and controls come with Phase 3.
            _ => {}
        }
    }
}
