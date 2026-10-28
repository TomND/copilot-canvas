//! Client connection to the terminal service, starting it when needed.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::paths::{Instance, build_id};
use crate::pipe;
use crate::protocol::{self, ClientMsg, PROTOCOL, ServiceMsg, read_frame, write_frame};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

pub struct Conn {
    pub tx: Sender<ClientMsg>,
    pub rx: Receiver<ServiceMsg>,
    pub service_build: String,
    pub protocol_mismatch: bool,
}

#[derive(Debug)]
pub struct AuthError;

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the process on the service pipe failed authentication")
    }
}
impl std::error::Error for AuthError {}

pub fn try_connect(instance: &Instance) -> Result<Conn> {
    let pipe = pipe::connect(&instance.pipe_name(), Duration::from_secs(2)).context("service is not running")?;
    let token = crate::service::read_token(&instance.token_path())?;
    let (mut reader, mut writer, closer) = pipe.split()?;
    let nonce = protocol::nonce();
    write_frame(&mut writer, &ClientMsg::Hello { protocol: PROTOCOL, build: build_id().to_owned(), nonce: nonce.clone() })?;
    let Some(ServiceMsg::Challenge { protocol: service_protocol, build, nonce: server_nonce, proof }) = read_frame(&mut reader)? else {
        bail!("unexpected service handshake");
    };
    if !protocol::verify(&token, "server", &nonce, &proof) {
        return Err(AuthError.into());
    }
    write_frame(&mut writer, &ClientMsg::Auth { proof: protocol::prove(&token, "client", &server_nonce) })?;

    let (in_tx, in_rx) = unbounded::<ServiceMsg>();
    let (out_tx, out_rx) = bounded::<ClientMsg>(4096);
    let reader_closer = closer.clone();
    // A service of another protocol (about to be upgraded) sends messages this
    // build cannot decode; skip them so its answers (stats) still arrive.
    let lenient = service_protocol != PROTOCOL;
    std::thread::Builder::new().name("service-reader".into()).spawn(move || {
        while let Ok(Some(frame)) = protocol::read_frame_lenient::<_, ServiceMsg>(&mut reader) {
            let message = match frame {
                Ok(message) => message,
                Err(body) if lenient => match protocol::decode_v3_terminals(&body) {
                    Some(infos) => ServiceMsg::Terminals(infos),
                    None => continue,
                },
                Err(_) => break,
            };
            if in_tx.send(message).is_err() {
                break;
            }
        }
        reader_closer.close();
    })?;
    std::thread::Builder::new().name("service-writer".into()).spawn(move || {
        for message in out_rx {
            if write_frame(&mut writer, &message).is_err() {
                break;
            }
        }
        closer.close();
    })?;
    Ok(Conn { tx: out_tx, rx: in_rx, service_build: build, protocol_mismatch: service_protocol != PROTOCOL })
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
    let exe = service_executable(instance)?;
    let mut command = Command::new(&exe);
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

pub fn connect(instance: &Instance, start: bool) -> Result<Conn> {
    match try_connect(instance) {
        Ok(conn) => return Ok(conn),
        Err(error) if error.is::<AuthError>() => return Err(error),
        Err(error) if !start => return Err(error),
        Err(_) => {}
    }
    spawn_service(instance)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(80));
        match try_connect(instance) {
            Ok(conn) => return Ok(conn),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("timed out"))).context(format!(
        "the terminal service did not start; see {}",
        instance.log_path().display()
    ))
}
