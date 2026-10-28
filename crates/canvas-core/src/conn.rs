//! Client connection to the terminal service: the local named pipe, or TCP
//! (the local service on loopback, or a remote service behind a tunnel).
//! Handles the handshake, heartbeats, input sequence numbers and reconnecting.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};

use crate::paths::{Instance, build_id};
use crate::pipe;
use crate::protocol::{self, CAP_COMPRESS, CAP_FRAME8, ClientInfo, ClientMsg, Input, PROTOCOL, PROTOCOL_MIN, ServiceMsg};
pub use canvas_common::handshake::{AuthError, NoChallenge};
use canvas_common::wire::{self, Beat, Closer, FrameReader, FrameWriter, Liveness, WireStats};
use canvas_common::{handshake, tcp};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
/// A service that has not completed the handshake by then is given up on.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a started service may take to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the service is.
#[derive(Clone)]
pub enum ConnTarget {
    /// The local service over its named pipe (started when needed).
    Local(Instance),
    /// The local service over TCP on loopback (remote mode without a tunnel);
    /// started with listening enabled when needed; authenticates with the token.
    Loopback(Instance),
    /// A service at `HOST:PORT` (a forwarded tunnel port).
    Tcp { addr: String, remote_id: Option<String>, secret: Vec<u8> },
}

impl ConnTarget {
    /// The `--remote` option: none (local pipe), `loopback`, or `HOST:PORT`
    /// with credentials from `COPILOT_CANVAS_REMOTE_ID` (optional) and
    /// `COPILOT_CANVAS_REMOTE_SECRET` (hex).
    pub fn parse(remote: Option<&str>, instance: &Instance) -> Result<Self> {
        match remote.map(str::trim) {
            None | Some("") => Ok(ConnTarget::Local(instance.clone())),
            Some("loopback") => Ok(ConnTarget::Loopback(instance.clone())),
            Some(addr) => {
                anyhow::ensure!(
                    addr.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()),
                    "--remote takes `loopback` or HOST:PORT, not {addr:?}"
                );
                let secret = std::env::var("COPILOT_CANVAS_REMOTE_SECRET").context("set COPILOT_CANVAS_REMOTE_SECRET (hex) to connect to a remote service")?;
                let secret = decode_hex(secret.trim()).context("COPILOT_CANVAS_REMOTE_SECRET must be hex")?;
                let remote_id = std::env::var("COPILOT_CANVAS_REMOTE_ID").ok().filter(|v| !v.trim().is_empty());
                Ok(ConnTarget::Tcp { addr: addr.to_owned(), remote_id, secret })
            }
        }
    }

    /// Over TCP: never restarts or upgrades the service.
    pub fn is_remote(&self) -> bool {
        !matches!(self, ConnTarget::Local(_))
    }

    pub fn describe(&self) -> String {
        match self {
            ConnTarget::Local(instance) => format!("the local service ({})", instance.name),
            ConnTarget::Loopback(instance) => format!("the local service over loopback TCP ({})", instance.name),
            ConnTarget::Tcp { addr, .. } => format!("the service at {addr}"),
        }
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

/// This client across reconnects: its session id and keyboard input numbering.
pub struct ClientSession {
    /// 16 random bytes (`ClientInfo::session`).
    pub id: Vec<u8>,
    /// Highest input seq handed to a connection.
    sent: AtomicU64,
    /// Highest input seq the service confirmed applied (or settled).
    acked: AtomicU64,
}

impl ClientSession {
    pub fn new() -> Arc<Self> {
        let mut id = vec![0u8; 16];
        getrandom_fill(&mut id);
        Arc::new(Self { id, sent: AtomicU64::new(0), acked: AtomicU64::new(0) })
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    pub fn acked(&self) -> u64 {
        self.acked.load(Ordering::Acquire)
    }

    pub(crate) fn ack(&self, seq: u64) {
        self.acked.fetch_max(seq, Ordering::AcqRel);
    }

    pub(crate) fn next_seq(&self) -> u64 {
        self.sent.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// After a reconnect: inputs sent but never confirmed applied. They are
    /// never resent; from now on they count as settled.
    pub fn settle(&self) -> u64 {
        let sent = self.sent();
        let lost = sent.saturating_sub(self.acked());
        self.ack(sent);
        lost
    }
}

fn getrandom_fill(bytes: &mut [u8]) {
    let nonce = protocol::nonce();
    for (byte, random) in bytes.iter_mut().zip(nonce) {
        *byte = random;
    }
}

/// How to connect.
#[derive(Clone)]
pub struct ConnOptions {
    /// Start the local service when it is not running.
    pub start: bool,
    /// Ask for compressed frames (TCP only; `remote.compress`).
    pub compress: bool,
    /// Front end name sent to the service.
    pub name: String,
    pub heartbeat: Duration,
    pub dead_after: Duration,
}

impl Default for ConnOptions {
    fn default() -> Self {
        Self { start: false, compress: true, name: "client".into(), heartbeat: wire::HEARTBEAT, dead_after: wire::DEAD_AFTER }
    }
}

pub struct Conn {
    pub tx: Sender<ClientMsg>,
    pub rx: Receiver<ServiceMsg>,
    pub service_build: String,
    /// The service speaks a protocol this client cannot use.
    pub protocol_mismatch: bool,
    /// Negotiated protocol (7 with an older service).
    pub protocol: u32,
    /// Over TCP (loopback or remote).
    pub remote: bool,
    pub session: Arc<ClientSession>,
    /// Bytes and frames on the wire.
    pub wire: Arc<WireStats>,
}

impl Conn {
    /// A connection to nothing (benchmarks and tests drive the channels).
    pub fn detached(tx: Sender<ClientMsg>, rx: Receiver<ServiceMsg>) -> Self {
        Self {
            tx,
            rx,
            service_build: build_id().to_owned(),
            protocol_mismatch: false,
            protocol: PROTOCOL,
            remote: false,
            session: ClientSession::new(),
            wire: Arc::default(),
        }
    }
}

/// Connects to `target` as `session`.
pub fn open(target: &ConnTarget, options: &ConnOptions, session: &Arc<ClientSession>) -> Result<Conn> {
    match target {
        ConnTarget::Local(instance) => open_local(instance, options, session),
        ConnTarget::Loopback(instance) => open_loopback(instance, options, session),
        ConnTarget::Tcp { addr, remote_id, secret } => {
            let stream = tcp::connect(addr, Duration::from_secs(5)).with_context(|| format!("cannot reach {addr}"))?;
            open_stream(stream, secret, remote_id.clone(), options, session)
        }
    }
}

/// The local service over the pipe, without starting it.
pub fn try_connect(instance: &Instance) -> Result<Conn> {
    open_pipe(instance, &ConnOptions::default(), &ClientSession::new())
}

/// The local service over the pipe; `start`: start it when it is not running.
pub fn connect(instance: &Instance, start: bool) -> Result<Conn> {
    open_local(instance, &ConnOptions { start, ..ConnOptions::default() }, &ClientSession::new())
}

/// Delay before reconnect attempt `n` (from 0): 0.5 s, 1 s, 2 s, then every 5 s.
pub fn backoff(attempt: u32) -> Duration {
    match attempt {
        0 => Duration::from_millis(500),
        1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

/// Reconnects with backoff until a connection succeeds, the service fails
/// authentication, or `retry` (shown each failed attempt) returns false.
pub fn reconnect(target: &ConnTarget, options: &ConnOptions, session: &Arc<ClientSession>, mut retry: impl FnMut(u32, &anyhow::Error) -> bool) -> Result<Conn> {
    let mut attempt = 0;
    loop {
        std::thread::sleep(backoff(attempt));
        match open(target, options, session) {
            Ok(conn) => return Ok(conn),
            Err(error) if error.is::<AuthError>() => return Err(error),
            Err(error) => {
                if !retry(attempt, &error) {
                    return Err(error);
                }
            }
        }
        attempt += 1;
    }
}

type Halves = (Box<dyn std::io::Read + Send>, Box<dyn std::io::Write + Send>, Closer);

fn pipe_halves(pipe: pipe::Pipe) -> Result<Halves> {
    let (reader, writer, closer) = pipe.split()?;
    Ok((Box::new(reader), Box::new(writer), Closer::new(move || closer.close())))
}

fn open_pipe(instance: &Instance, options: &ConnOptions, session: &Arc<ClientSession>) -> Result<Conn> {
    let pipe = pipe::connect(&instance.pipe_name(), Duration::from_secs(2)).context("service is not running")?;
    let token = canvas_common::auth::read_token(&instance.token_path())?;
    match establish(pipe_halves(pipe)?, &token, None, options, session, false, true) {
        // A service older than protocol 8 cannot read `Hello8`: say `Hello`.
        Err(error) if error.is::<NoChallenge>() => {
            let pipe = pipe::connect(&instance.pipe_name(), Duration::from_secs(2)).context("service is not running")?;
            establish(pipe_halves(pipe)?, &token, None, options, session, false, false)
        }
        other => other,
    }
}

fn open_local(instance: &Instance, options: &ConnOptions, session: &Arc<ClientSession>) -> Result<Conn> {
    match open_pipe(instance, options, session) {
        Ok(conn) => return Ok(conn),
        Err(error) if error.is::<AuthError>() => return Err(error),
        Err(error) if !options.start => return Err(error),
        Err(_) => {}
    }
    spawn_service(instance)?;
    wait_started(instance, || open_pipe(instance, options, session))
}

fn wait_started(instance: &Instance, mut attempt: impl FnMut() -> Result<Conn>) -> Result<Conn> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut last = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(80));
        match attempt() {
            Ok(conn) => return Ok(conn),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("timed out")))
        .context(format!("the terminal service did not start; see {}", instance.log_path().display()))
}

/// The port a local service listens on for remote clients.
pub fn listening_port(instance: &Instance) -> Option<u16> {
    std::fs::read_to_string(instance.port_path()).ok()?.trim().parse().ok()
}

fn open_loopback(instance: &Instance, options: &ConnOptions, session: &Arc<ClientSession>) -> Result<Conn> {
    let attempt = || -> Result<Conn> {
        let port = listening_port(instance).context("the local service is not listening for remote clients")?;
        let token = canvas_common::auth::read_token(&instance.token_path())?;
        let stream = tcp::connect(&format!("127.0.0.1:{port}"), Duration::from_secs(2))?;
        open_stream(stream, &token, None, options, session)
    };
    match attempt() {
        Ok(conn) => return Ok(conn),
        Err(error) if error.is::<AuthError>() => return Err(error),
        Err(error) if !options.start => return Err(error),
        Err(_) => {}
    }
    // A service is running: give a starting one a moment to publish its port.
    if pipe::connect(&instance.pipe_name(), Duration::from_millis(300)).is_ok() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            if listening_port(instance).is_some() {
                return wait_started(instance, attempt);
            }
        }
        bail!(
            "the local service runs without remote listening; stop it (`copilot-canvas service stop`) or set remote.listen in {}",
            canvas_common::settings::path(&instance.home).display()
        );
    }
    let configured = canvas_common::settings::Settings::load(&instance.home).listen().is_some();
    let env: &[(&str, &str)] = if configured { &[] } else { &[("COPILOT_CANVAS_LISTEN", "127.0.0.1:0")] };
    spawn_service_with(instance, env)?;
    wait_started(instance, attempt)
}

fn open_stream(stream: std::net::TcpStream, secret: &[u8], remote_id: Option<String>, options: &ConnOptions, session: &Arc<ClientSession>) -> Result<Conn> {
    let (reader, writer, closer) = tcp::split(stream)?;
    let halves: Halves = (Box::new(reader), Box::new(writer), Closer::new(move || closer.close()));
    establish(halves, secret, remote_id, options, session, true, true)
}

/// Runs the handshake on a fresh connection and starts its threads.
/// `hello8`: open with `Hello8` (else protocol-7 `Hello`).
fn establish(
    (reader, writer, closer): Halves,
    secret: &[u8],
    remote_id: Option<String>,
    options: &ConnOptions,
    session: &Arc<ClientSession>,
    remote: bool,
    hello8: bool,
) -> Result<Conn> {
    let stats = Arc::new(WireStats::default());
    let mut reader = FrameReader::new(reader).with_stats(stats.clone());
    let mut writer = FrameWriter::new(writer).with_stats(stats.clone());
    let compress = remote && options.compress;
    let info = hello8.then(|| ClientInfo {
        name: options.name.clone(),
        caps: CAP_FRAME8 | if compress { CAP_COMPRESS } else { 0 },
        session: session.id.clone(),
        remote_id,
    });
    // A service that never answers must not hang the client.
    let done = Arc::new(AtomicBool::new(false));
    {
        let (done, closer) = (done.clone(), closer.clone());
        std::thread::Builder::new().name("handshake-timeout".into()).spawn(move || {
            let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
            while Instant::now() < deadline && !done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50));
            }
            if !done.load(Ordering::Acquire) {
                closer.close();
            }
        })?;
    }
    let result = (|| -> Result<(handshake::Greeting, bool)> {
        let greeting = handshake::open(&mut reader, &mut writer, secret, build_id(), info)?;
        let usable = (PROTOCOL_MIN..=PROTOCOL).contains(&greeting.protocol);
        let v8 = usable && greeting.protocol >= 8;
        if v8 {
            // Ready8 comes first: how much of this session's input was applied.
            match reader.recv::<ServiceMsg>() {
                Ok(Some(ServiceMsg::Ready8 { input_ack, .. })) => session.ack(input_ack),
                _ => bail!("the service refused this client after the handshake"),
            }
            if remote {
                reader.accept_compressed()?;
                if compress {
                    writer.compress()?;
                }
            }
        }
        Ok((greeting, v8))
    })();
    done.store(true, Ordering::Release);
    let (greeting, v8) = match result {
        Ok(ok) => ok,
        Err(error) => {
            closer.close();
            return Err(error);
        }
    };
    let protocol = greeting.protocol;

    let (in_tx, in_rx) = unbounded::<ServiceMsg>();
    let (out_tx, out_rx) = bounded::<ClientMsg>(4096);
    let liveness = Arc::new(Liveness::new(options.heartbeat, options.dead_after));
    liveness.received();
    let reading = Arc::new(AtomicBool::new(true));
    // A service of another protocol (about to be upgraded) sends messages this
    // build cannot decode; skip them so its answers (stats) still arrive.
    let lenient = protocol != PROTOCOL;
    {
        let (liveness, reading, closer, session) = (liveness.clone(), reading.clone(), closer.clone(), session.clone());
        std::thread::Builder::new().name("service-reader".into()).spawn(move || {
            while let Ok(Some(frame)) = reader.recv_lenient::<ServiceMsg>() {
                liveness.received();
                let message = match frame {
                    Ok(message) => message,
                    Err(body) if lenient => match protocol::decode_v3_terminals(&body) {
                        Some(infos) => ServiceMsg::Terminals(infos),
                        None => continue,
                    },
                    Err(_) => break,
                };
                if let ServiceMsg::Heartbeat { input_ack } = message {
                    session.ack(input_ack);
                    continue;
                }
                if in_tx.send(message).is_err() {
                    break;
                }
            }
            reading.store(false, Ordering::Release);
            closer.close();
        })?;
    }
    {
        let (closer, session) = (closer.clone(), session.clone());
        let heartbeat = v8.then_some(options.heartbeat);
        std::thread::Builder::new().name("service-writer".into()).spawn(move || {
            let mut beat = heartbeat.map(Beat::new);
            loop {
                let message = match &beat {
                    Some(beat) => match out_rx.recv_deadline(beat.next()) {
                        Ok(message) => Some(message),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match out_rx.recv() {
                        Ok(message) => Some(message),
                        Err(_) => break,
                    },
                };
                if let Some(message) = message.and_then(|m| adapt(m, v8, &session))
                    && writer.send(&message).is_err()
                {
                    break;
                }
                if beat.as_mut().is_some_and(|b| b.due(Instant::now())) && writer.send(&ClientMsg::Heartbeat).is_err() {
                    break;
                }
            }
            closer.close();
        })?;
    }
    if v8 {
        // A connection silent past `dead_after` is closed; the reader then ends.
        let slice = (options.dead_after / 4).min(Duration::from_millis(250));
        std::thread::Builder::new().name("service-watchdog".into()).spawn(move || {
            while reading.load(Ordering::Acquire) {
                std::thread::sleep(slice);
                if liveness.dead() {
                    closer.close();
                }
            }
        })?;
    }
    Ok(Conn {
        tx: out_tx,
        rx: in_rx,
        service_build: greeting.build,
        protocol_mismatch: !(PROTOCOL_MIN..=PROTOCOL).contains(&protocol),
        protocol,
        remote,
        session: session.clone(),
        wire: stats,
    })
}

/// The message in the negotiated protocol: keyboard input is numbered on
/// protocol 8; protocol-8 shapes become protocol 7 for an older service.
fn adapt(message: ClientMsg, v8: bool, session: &ClientSession) -> Option<ClientMsg> {
    Some(match message {
        ClientMsg::Input { id, input } | ClientMsg::Input8 { id, input, .. } if v8 && !matches!(input, Input::Focus(_)) => {
            ClientMsg::Input8 { id, input, seq: session.next_seq() }
        }
        ClientMsg::Input8 { id, input, .. } => ClientMsg::Input { id, input },
        ClientMsg::Subscribe8 { ids, .. } if !v8 => ClientMsg::Subscribe { ids },
        ClientMsg::Heartbeat if !v8 => return None,
        other => other,
    })
}

/// Runs the service from a per-build copy so rebuilding or upgrading the
/// client never fights a running service for the executable file.
fn service_executable(instance: &Instance) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = instance.bin_dir();
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("copilot-canvas-service-{}.exe", build_id()));
    if !target.is_file() {
        let temp = target.with_extension("exe.tmp");
        std::fs::copy(&exe, &temp).context("cannot stage the service executable")?;
        std::fs::rename(&temp, &target).or_else(|e| if target.is_file() { Ok(()) } else { Err(e) })?;
    }
    // Old builds that are no longer running can be removed; in-use files are skipped.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.path() != target && entry.file_name().to_string_lossy().starts_with("copilot-canvas-service-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(target)
}

pub fn spawn_service(instance: &Instance) -> Result<()> {
    spawn_service_with(instance, &[])
}

/// Starts the service (from this build's per-build copy) with extra environment.
pub fn spawn_service_with(instance: &Instance, env: &[(&str, &str)]) -> Result<()> {
    spawn_service_exe(&service_executable(instance)?, instance, env)
}

/// Starts the service from `exe` (probes and tests run a given build).
pub fn spawn_service_exe(exe: &Path, instance: &Instance, env: &[(&str, &str)]) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .arg("service")
        .arg("--instance")
        .arg(&instance.name)
        .arg("--home")
        .arg(&instance.home)
        .arg("--sessions-dir")
        .arg(&instance.sessions_dir)
        .current_dir(&instance.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    // Outlive the launching terminal (and its job) where the job permits it.
    let spawned = command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).spawn();
    match spawned {
        Ok(_) => Ok(()),
        Err(_) => command
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())
            .context("cannot start the terminal service"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Key;

    #[test]
    fn input_is_numbered_on_protocol8_and_downgraded_for_protocol7() {
        let session = ClientSession::new();
        let key = || Input::Key { key: Key::Char('a'), mods: 0 };
        let first = adapt(ClientMsg::Input { id: "t".into(), input: key() }, true, &session);
        let second = adapt(ClientMsg::Input { id: "t".into(), input: key() }, true, &session);
        assert_eq!(first, Some(ClientMsg::Input8 { id: "t".into(), input: key(), seq: 1 }));
        assert_eq!(second, Some(ClientMsg::Input8 { id: "t".into(), input: key(), seq: 2 }));
        assert_eq!(adapt(ClientMsg::Input { id: "t".into(), input: Input::Focus(true) }, true, &session), Some(ClientMsg::Input { id: "t".into(), input: Input::Focus(true) }), "focus reports are not keystrokes");
        assert_eq!(adapt(ClientMsg::Input8 { id: "t".into(), input: key(), seq: 9 }, false, &session), Some(ClientMsg::Input { id: "t".into(), input: key() }));
        assert_eq!(adapt(ClientMsg::Subscribe8 { ids: vec!["t".into()], focused: None }, false, &session), Some(ClientMsg::Subscribe { ids: vec!["t".into()] }));
        assert_eq!(adapt(ClientMsg::Heartbeat, false, &session), None);
        assert_eq!(session.sent(), 2);
    }

    #[test]
    fn unconfirmed_input_is_reported_once_and_never_resent() {
        let session = ClientSession::new();
        for _ in 0..5 {
            session.next_seq();
        }
        session.ack(3);
        assert_eq!(session.settle(), 2, "seq 4 and 5 were never confirmed");
        assert_eq!(session.settle(), 0, "reported once");
        assert_eq!(session.next_seq(), 6, "numbering continues: nothing is resent");
        session.ack(2);
        assert_eq!(session.acked(), 5, "acks never go back");
    }

    #[test]
    fn backoff_grows_to_five_seconds() {
        let delays: Vec<u64> = (0..6).map(|n| backoff(n).as_millis() as u64).collect();
        assert_eq!(delays, vec![500, 1000, 2000, 5000, 5000, 5000]);
    }

    #[test]
    fn heartbeats_keep_a_connection_and_silence_ends_it() {
        let listener = tcp::listen_loopback("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let beating = Duration::from_millis(400);
        // A minimal service: handshake, Ready8, heartbeats for a while, then silence.
        let service = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (reader, writer, closer) = tcp::split(stream).unwrap();
            let (mut reader, mut writer) = (FrameReader::new(Box::new(reader)), FrameWriter::new(Box::new(writer)));
            let accepted = handshake::accept(&mut reader, &mut writer, true, "svc", |_| Some(b"secret".to_vec())).unwrap();
            writer.send(&ServiceMsg::Ready8 { pid: 1, input_ack: 7 }).unwrap();
            let beats = std::thread::spawn(move || {
                let started = Instant::now();
                while started.elapsed() < beating {
                    writer.send(&ServiceMsg::Heartbeat { input_ack: 9 }).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }
                writer
            });
            let mut heartbeats = 0;
            while let Ok(Some(message)) = reader.recv::<ClientMsg>() {
                if message == ClientMsg::Heartbeat {
                    heartbeats += 1;
                }
            }
            let _writer = beats.join().unwrap();
            closer.close();
            (accepted.client.unwrap().caps, heartbeats)
        });
        let options = ConnOptions { heartbeat: Duration::from_millis(50), dead_after: Duration::from_millis(300), ..ConnOptions::default() };
        let session = ClientSession::new();
        let target = ConnTarget::Tcp { addr, remote_id: None, secret: b"secret".to_vec() };
        let conn = open(&target, &options, &session).unwrap();
        assert_eq!((conn.protocol, conn.remote), (PROTOCOL, true));
        assert_eq!(session.acked(), 7, "Ready8 reports the applied input");
        let started = Instant::now();
        assert_eq!(conn.rx.recv_timeout(Duration::from_secs(5)).err(), Some(RecvTimeoutError::Disconnected));
        let alive = started.elapsed();
        assert!(alive >= beating, "heartbeats kept the connection open ({alive:?})");
        assert!(alive < beating + Duration::from_millis(900), "silence ended it soon after the timeout ({alive:?})");
        assert_eq!(session.acked(), 9, "heartbeats carry the input ack");
        let (caps, heartbeats) = service.join().unwrap();
        assert_eq!(caps, CAP_FRAME8 | CAP_COMPRESS);
        assert!(heartbeats >= 4, "the client sent heartbeats ({heartbeats})");
    }

    #[test]
    fn remote_targets_parse() {
        let home = std::env::temp_dir().join(format!("cc-target-{}", uuid::Uuid::new_v4()));
        let instance = Instance::new(Some("t".into()), Some(home.clone()), Some(home.clone())).unwrap();
        assert!(matches!(ConnTarget::parse(None, &instance).unwrap(), ConnTarget::Local(_)));
        assert!(matches!(ConnTarget::parse(Some("loopback"), &instance).unwrap(), ConnTarget::Loopback(_)));
        assert!(ConnTarget::parse(Some("nonsense"), &instance).is_err());
        assert_eq!(decode_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(decode_hex("0g"), None);
        let _ = std::fs::remove_dir_all(home);
    }
}
