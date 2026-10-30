//! A stand-in Copilot Canvas service (docs/IOS-PLAN.md, phase 2): the iPhone
//! client can be developed and tested on a Mac. It speaks the real protocol
//! (framing, zstd, the HMAC handshake, pairing) over plain TCP and over
//! WebSockets on one port, keeps a canvas document with patches, and runs
//! real shells in pseudo-terminals through the service's own terminal
//! engine (alacritty) and key encoder. Agent launches start a shell that
//! announces the agent; typing Enter in one makes its session "work" for a
//! few seconds, so statuses (working, done · review, needs you) can be seen.
//!
//! With `--tunnel ID` it stands in for the Cloud PC behind a real private Dev
//! Tunnel (`ios/cloud-mac.sh`): it issues Dev Tunnels connect tokens with this
//! machine's signed-in `devtunnel` CLI, sends them to phones
//! (`ServiceMsg::TunnelAccess`) and renews them, like the real service. With
//! `--real-agents`, sessions started from the phone run the agent CLIs found
//! on this machine (OpenCode, Claude Code, Copilot) instead of a shell.
//!
//! Not a security boundary: it is a development tool. A paired phone gets a
//! shell on this machine.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::handshake::{self, Credential, Opening, PairGrant, Pairing};
use canvas_common::paths::now_ms;
use canvas_common::protocol::{
    Activity, AgentStatus, AgentSummary, CAP_COMPRESS, ClientMsg, Frame8, Input, LaunchKind, MAX_FRAME, PRE_AUTH_FRAME, Run8, ServiceMsg, SessionSummary, TermInfo,
};
use canvas_common::sizing::SizePolicy;
use canvas_common::wire::{Beat, FrameReader, FrameWriter, Liveness};
use canvas_common::{pairing, tcp, ws};
use canvas_host::emulator::{self, Collected, Engine, Scroll, Snapshot};
use canvas_model::doc::{Doc, Group, Node, NodeKind};
use canvas_model::rpc::{BackendInfo, PathInfo, Request, Response};
use canvas_model::sync::{self, DocReply, Op};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const BUILD: &str = concat!("mock-", env!("CARGO_PKG_VERSION"));

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Config {
    /// `127.0.0.1:47600`, or `0.0.0.0:47600` to reach it from a phone on the LAN.
    pub listen: String,
    /// The open pairing code (canonical form).
    pub code: String,
    /// Where paired clients are remembered (survives restarts).
    pub state_dir: PathBuf,
    /// The folder new sessions start in.
    pub cwd: String,
    /// Log what happens.
    pub verbose: bool,
    /// The private Dev Tunnel in front of the port: tokens for phones.
    pub tunnel: Option<String>,
    /// The tunnel port's web address (found with `devtunnel show` when unset).
    pub web_url: Option<String>,
    /// New agent sessions run the agent CLIs installed here.
    pub real_agents: bool,
    /// The APNs key: phones away from the app get notifications.
    pub apns: Option<canvas_push::ApnsKey>,
}

impl Config {
    /// A local mock: loopback, the fixed test code, a temporary state folder.
    pub fn local(listen: &str, cwd: &str) -> Self {
        Config {
            listen: listen.into(),
            code: "ABCDEFGHJKMN".into(),
            state_dir: std::env::temp_dir().join("canvas-mock"),
            cwd: cwd.into(),
            verbose: false,
            tunnel: None,
            web_url: None,
            real_agents: false,
            apns: None,
        }
    }
}

/// A tunnel access token and the address it opens.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Access {
    pub url: String,
    pub token: String,
    pub expires_ms: u64,
}

/// One connected client.
struct Client {
    id: u64,
    /// The paired client this connection authenticated as.
    remote_id: Option<String>,
    out: Sender<ServiceMsg>,
    subs: Mutex<HashMap<String, Sent>>,
    focused: Mutex<Option<String>>,
    closer: canvas_common::wire::Closer,
}

/// What a client last got of a terminal (row hashes, cursor, size).
#[derive(Default, Clone, PartialEq)]
struct Sent {
    cols: u16,
    rows: u16,
    hashes: Vec<u64>,
    cursor: Option<(u16, u16)>,
    shape: u8,
    modes: u32,
    offset: u32,
    at: Option<Instant>,
}

enum Ctl {
    Input(Input),
    Resize(u16, u16),
    Scroll(Scroll),
    Wheel { up: bool, col: u16, row: u16, mods: u8 },
    Click { col: u16, row: u16, mods: u8 },
    /// Send frames to subscribers that are due.
    Pump,
    Stop,
}

struct Term {
    ctl: Sender<Ctl>,
    info: TermInfo,
}

struct Mock {
    config: Config,
    next_client: AtomicU64,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    terms: Mutex<HashMap<String, Term>>,
    doc: Mutex<(Doc, u64)>,
    sessions: Mutex<HashMap<String, SessionSummary>>,
    sizes: Mutex<HashMap<String, SizePolicy>>,
    /// Paired clients: id -> secret.
    paired: Mutex<HashMap<String, Vec<u8>>>,
    /// The current tunnel access token (`--tunnel`).
    access: Mutex<Option<Access>>,
    /// Phones registered for notifications, and the sender (`--apns-key`).
    devices: canvas_push::Devices,
    sender: Option<Arc<canvas_push::Sender>>,
    /// Terminals that run a real agent CLI (statuses come from what it reports).
    real: Mutex<std::collections::HashSet<String>>,
}

pub struct Handle {
    pub addr: SocketAddr,
    mock: Arc<Mock>,
}

impl Handle {
    /// Issues a tunnel access token now (`--tunnel`); the service keeps it and
    /// sends it to phones.
    pub fn tunnel_access(&self) -> Result<Access> {
        renew_access(&self.mock)
    }
}

fn log(mock: &Mock, message: &str) {
    if mock.config.verbose {
        eprintln!("[mock {}] {message}", now_ms() % 100_000);
    }
}

/// Starts the mock service; it serves until the process exits.
pub fn serve(config: Config) -> Result<Handle> {
    std::fs::create_dir_all(&config.state_dir)?;
    let listener = TcpListener::bind(&config.listen).with_context(|| format!("cannot listen on {}", config.listen))?;
    let addr = listener.local_addr()?;
    let paired: HashMap<String, Vec<u8>> = std::fs::read(config.state_dir.join("clients.json")).ok().and_then(|b| serde_json::from_slice::<HashMap<String, String>>(&b).ok()).map(|m| m.into_iter().filter_map(|(k, v)| Some((k, canvas_common::paths::unhex(&v)?))).collect()).unwrap_or_default();
    // The canvas survives restarts (new sessions from the phone stay).
    let doc = std::fs::read(config.state_dir.join("canvas.json")).ok().and_then(|bytes| serde_json::from_slice::<Doc>(&bytes).ok()).unwrap_or_else(|| demo_doc(&config.cwd));
    let sessions = demo_sessions(&doc);
    let (config_state, config_apns) = (config.state_dir.clone(), config.apns.clone());
    let mock = Arc::new(Mock {
        config,
        next_client: AtomicU64::new(1),
        clients: Mutex::new(HashMap::new()),
        terms: Mutex::new(HashMap::new()),
        doc: Mutex::new((doc, 1)),
        sessions: Mutex::new(sessions),
        sizes: Mutex::new(HashMap::new()),
        paired: Mutex::new(paired),
        access: Mutex::new(None),
        devices: canvas_push::Devices::open(&config_state),
        sender: config_apns.and_then(|key| canvas_push::Sender::new(key).ok()).map(Arc::new),
        real: Mutex::new(std::collections::HashSet::new()),
    });
    let accept = mock.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mock = accept.clone();
            std::thread::spawn(move || {
                if let Err(error) = serve_client(&mock, stream) {
                    log(&mock, &format!("client ended: {error:#}"));
                }
            });
        }
    });
    // Simulated status changes for local development; behind a tunnel the
    // statuses are the real ones (notifications should mean something).
    if mock.config.tunnel.is_none() {
        let ticker = mock.clone();
        std::thread::spawn(move || tick_loop(ticker));
    }
    let watcher = mock.clone();
    std::thread::spawn(move || push_loop(watcher));
    // A few sessions already run, in different states (as on a busy Cloud PC).
    for (id, agent) in [("mock-fix", "copilot"), ("mock-review", "opencode"), ("mock-docs", "copilot")] {
        let Some(name) = lock(&mock.doc).0.node(id).map(|n| n.name.clone()) else { continue };
        let launch = canvas_common::protocol::AgentLaunch { agent: agent.into(), resume: true, name: Some(name), ..Default::default() };
        if let Err(error) = launch_with(&mock, id, LaunchKind::Agent(Box::new(launch)), &mock.config.cwd.clone(), 100, 30, transcript(id)) {
            log(&mock, &format!("cannot start {id}: {error:#}"));
        }
    }
    set_activity(&mock, "mock-fix", |s| s.activity = Activity::Working);
    set_activity(&mock, "mock-review", |s| {
        s.activity = Activity::Working;
        s.pending = 1;
    });
    set_activity(&mock, "mock-docs", |s| {
        s.activity = Activity::Idle;
        s.finished_ms = Some(now_ms());
    });
    if mock.config.tunnel.is_some() {
        let renewer = mock.clone();
        std::thread::spawn(move || access_loop(renewer));
    }
    Ok(Handle { addr, mock })
}

// ---------- Dev Tunnels access ----------

/// Runs `devtunnel ARGS` (stdout and stderr together).
fn devtunnel(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("devtunnel").args(args).stdin(std::process::Stdio::null()).output().context("cannot run devtunnel (install the Dev Tunnels CLI)")?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    anyhow::ensure!(output.status.success(), "devtunnel {}: {}", args.first().unwrap_or(&""), text.trim().lines().last().unwrap_or("failed"));
    Ok(text)
}

/// A fresh connect token for the tunnel, and the port's web address.
fn renew_access(mock: &Arc<Mock>) -> Result<Access> {
    let tunnel = mock.config.tunnel.clone().context("no tunnel")?;
    let port = mock.config.listen.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(0);
    let output = devtunnel(&["token", &tunnel, "--scopes", "connect"])?;
    let token = canvas_common::remote::parse_token_output(&output).with_context(|| format!("`devtunnel token` printed no token:\n{output}"))?;
    let url = match &mock.config.web_url {
        Some(url) => url.clone(),
        None => devtunnel(&["show", &tunnel])
            .ok()
            .and_then(|text| canvas_common::remote::web_url_from_show(&text, port))
            .or_else(|| {
                let claims = canvas_common::remote::jwt_claims(&token.token)?;
                let cluster = claims.get("clusterId")?.as_str()?.to_owned();
                canvas_common::remote::web_url_for(&format!("{tunnel}.{cluster}"), port)
            })
            .unwrap_or_default(),
    };
    let access = Access { url, token: token.token, expires_ms: token.expires_ms };
    *lock(&mock.access) = Some(access.clone());
    push_access(mock, &access);
    broadcast(mock, ServiceMsg::TunnelAccess { url: access.url.clone(), token: access.token.clone(), expires_ms: access.expires_ms });
    log(mock, &format!("tunnel access token valid until {} for {}", canvas_common::paths::format_rfc3339_ms(access.expires_ms), access.url));
    Ok(access)
}

/// Renews the token half a day before it expires (every 10 minutes after a failure).
fn access_loop(mock: Arc<Mock>) {
    loop {
        let wait = match lock(&mock.access).as_ref() {
            Some(access) if access.expires_ms > 0 => Duration::from_millis(access.expires_ms.saturating_sub(now_ms() + 12 * 3600 * 1000).max(60_000)),
            Some(_) => Duration::from_secs(12 * 3600),
            None => Duration::from_secs(600),
        };
        std::thread::sleep(wait);
        if let Err(error) = renew_access(&mock) {
            log(&mock, &format!("cannot renew the tunnel access token: {error:#}"));
        }
    }
}

/// A picture from the phone on this machine's clipboard (the agent reads it on Ctrl+V).
fn set_clipboard_image(dir: &std::path::Path, png: &[u8]) -> Result<()> {
    anyhow::ensure!(png.starts_with(b"\x89PNG"), "not a PNG picture");
    let path = dir.join("pasted.png");
    std::fs::write(&path, png)?;
    #[cfg(target_os = "macos")]
    {
        let script = format!("set the clipboard to (read (POSIX file \"{}\") as «class PNGf»)", path.display());
        let status = std::process::Command::new("osascript").arg("-e").arg(script).status()?;
        anyhow::ensure!(status.success(), "osascript could not set the clipboard");
    }
    Ok(())
}

// ---------- notifications ----------

/// Paired phones connected now.
fn connected_phones(mock: &Mock) -> std::collections::HashSet<String> {
    lock(&mock.clients).values().filter_map(|c| c.remote_id.clone()).collect()
}

/// Sends to every registered phone that is away (on a thread).
fn push_away(mock: &Arc<Mock>, make: impl Fn(&canvas_push::Device) -> Option<(serde_json::Value, bool, Option<String>, String)>) {
    let Some(sender) = mock.sender.clone() else { return };
    let online = connected_phones(mock);
    let jobs: Vec<_> = mock.devices.list().into_iter().filter(|d| !online.contains(&d.client_id) && lock(&mock.paired).contains_key(&d.client_id)).filter_map(|d| make(&d).map(|job| (d, job))).collect();
    if jobs.is_empty() {
        return;
    }
    let mock = mock.clone();
    std::thread::spawn(move || {
        for (device, (payload, background, collapse, what)) in jobs {
            match sender.send(&device, &payload, background, collapse.as_deref()) {
                canvas_push::Outcome::Sent => log(&mock, &format!("push: sent {what} to {}", device.client_id)),
                canvas_push::Outcome::Unregistered => {
                    log(&mock, &format!("push: {} unregistered by APNs", device.client_id));
                    mock.devices.remove(None, Some(&device.token));
                }
                canvas_push::Outcome::Failed(error) => log(&mock, &format!("push: {what} to {} failed: {error}", device.client_id)),
            }
        }
    });
}

/// A renewed tunnel token, sealed for each phone that is away.
fn push_access(mock: &Arc<Mock>, access: &Access) {
    push_away(mock, |device| {
        let secret = lock(&mock.paired).get(&device.client_id).cloned()?;
        Some((canvas_push::access_payload(device, &secret, &access.url, &access.token, access.expires_ms), true, None, "a renewed tunnel token".into()))
    });
}

/// Watches the statuses and tells phones that are away when a session needs
/// them or finished a turn (the service's rules, `canvas_push::Attention`).
fn push_loop(mock: Arc<Mock>) {
    let mut attention = canvas_push::Attention::default();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let doc = lock(&mock.doc).0.clone();
        let sessions = lock(&mock.sessions).clone();
        let terms: HashMap<String, TermInfo> = lock(&mock.terms).iter().map(|(id, t)| (id.clone(), t.info.clone())).collect();
        let now = doc
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Session && n.passive.is_none())
            .map(|n| (n.id.clone(), n.name.clone(), canvas_model::status::session_status(sessions.get(&n.id), terms.get(&n.id), n.reviewed_ms)))
            .collect::<Vec<_>>();
        let events = attention.update(now);
        for event in events {
            let what = if event.status == canvas_model::status::Status::Waiting { "needs you" } else { "finished" };
            log(&mock, &format!("{} {what}", event.name));
            push_away(&mock, |device| Some((canvas_push::alert_payload(device, &event), false, Some(event.id.clone()), format!("{what:?} for {:?}", event.name))));
        }
    }
}

// ---------- agents on this machine ----------

/// The agent CLIs installed here (`--real-agents`), Copilot first like the service.
fn installed_agents() -> Vec<canvas_model::agent::AgentKind> {
    use canvas_model::agent::AgentKind;
    [(AgentKind::Copilot, "copilot"), (AgentKind::OpenCode, "opencode"), (AgentKind::Claude, "claude")]
        .into_iter()
        .filter(|(_, program)| find_program(program).is_some())
        .map(|(kind, _)| kind)
        .collect()
}

/// A program on PATH or in the usual per-user install folders.
fn find_program(name: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend([home.join(".local/bin"), home.join(".opencode/bin"), home.join(".npm-global/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")]);
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// The command line of a real agent session. Permissions are left to the
/// agent (it asks; answer from the phone): this is your own Mac.
fn agent_command(launch: &canvas_common::protocol::AgentLaunch, id: &str) -> Option<CommandBuilder> {
    let uuid = uuid::Uuid::parse_str(id).is_ok();
    let prompt = launch.prompt.clone().filter(|p| !p.trim().is_empty());
    let (program, args): (&str, Vec<String>) = match launch.agent.as_str() {
        "opencode" => {
            let mut args = Vec::new();
            if launch.resume {
                args.push("--continue".into());
            } else if let Some(prompt) = prompt {
                args.extend(["--prompt".into(), prompt]);
            }
            ("opencode", args)
        }
        "claude" if launch.fork_from.is_some() && uuid => {
            // Claude Code forks the parent's conversation itself.
            let mut args = vec!["--resume".into(), launch.fork_from.clone().unwrap_or_default(), "--fork-session".into(), "--session-id".into(), id.into()];
            if let Some(prompt) = prompt {
                args.push(prompt);
            }
            ("claude", args)
        }
        "claude" => {
            let mut args = Vec::new();
            match (launch.resume, uuid) {
                (true, true) => args.extend(["--resume".into(), id.into()]),
                (true, false) => args.push("--continue".into()),
                (false, true) => args.extend(["--session-id".into(), id.into()]),
                (false, false) => {}
            }
            if !launch.resume && let Some(prompt) = prompt {
                args.push(prompt);
            }
            ("claude", args)
        }
        _ => {
            let mut args = Vec::new();
            if uuid {
                args.extend(["--session-id".into(), id.into()]);
            }
            if !launch.resume && let Some(prompt) = prompt {
                args.extend(["-i".into(), prompt]);
            }
            ("copilot", args)
        }
    };
    let path = find_program(program)?;
    let mut command = CommandBuilder::new(path);
    command.args(args);
    Some(command)
}

/// What an agent's screen shows in the mock before its shell prompt: an
/// agent-like transcript (your prompt, tool calls, a diff, a permission
/// question) so the phone's renderer has real colours, boxes and widths.
fn transcript(id: &str) -> String {
    let user = "\x1b[38;5;252m ❯ \x1b[0m";
    match id {
        "mock-fix" => [
            "\x1b[1;38;5;75m╭──────────────────────────────────────────────╮\x1b[0m",
            "\x1b[1;38;5;75m│\x1b[0m  \x1b[1mGitHub Copilot\x1b[0m \x1b[2mv1.0 · claude-sonnet · allow-all\x1b[0m      \x1b[1;38;5;75m│\x1b[0m",
            "\x1b[1;38;5;75m╰──────────────────────────────────────────────╯\x1b[0m",
            "",
            &format!("{user}Fix the flaky payment test in ci/payments.test.ts"),
            "   it fails about 1 in 10 runs on main",
            "",
            " \x1b[38;5;75m●\x1b[0m I'll look at the test and its timers first.",
            "",
            " \x1b[38;5;114m✓\x1b[0m \x1b[1mRead\x1b[0m ci/payments.test.ts \x1b[2m(142 lines)\x1b[0m",
            " \x1b[38;5;114m✓\x1b[0m \x1b[1mBash\x1b[0m npm test -- payments \x1b[2m--runInBand\x1b[0m",
            " \x1b[38;5;75m●\x1b[0m The test waits on a real \x1b[3msetTimeout\x1b[0m; under load the",
            "   retry fires late. Using fake timers:",
            "",
            "   \x1b[48;5;52m\x1b[38;5;210m- await sleep(1500);                         \x1b[0m",
            "   \x1b[48;5;22m\x1b[38;5;157m+ await vi.advanceTimersByTimeAsync(1500);  \x1b[0m",
            "",
            " \x1b[38;5;220m⠹\x1b[0m \x1b[2mRunning the suite 20 times… (esc to interrupt)\x1b[0m",
            "",
        ]
        .join("\r\n"),
        "mock-review" => [
            "\x1b[1;38;5;141m  opencode\x1b[0m \x1b[2m· review · deepseek-v4\x1b[0m",
            "",
            &format!("{user}Review PR #42 (refund flow) for security issues"),
            "",
            " \x1b[38;5;141m▍\x1b[0m Reading 14 changed files…",
            " \x1b[38;5;141m▍\x1b[0m \x1b[4;58;5;214mrefund.ts:88\x1b[0m amount is not checked against the charge",
            "",
            "\x1b[38;5;214m┏━ Permission required ━━━━━━━━━━━━━━━━━━━━━━━┓\x1b[0m",
            "\x1b[38;5;214m┃\x1b[0m bash: \x1b[1mgh pr diff 42 --patch\x1b[0m                   \x1b[38;5;214m┃\x1b[0m",
            "\x1b[38;5;214m┃\x1b[0m  \x1b[7m 1 Allow once \x1b[0m  2 Always  3 Reject           \x1b[38;5;214m┃\x1b[0m",
            "\x1b[38;5;214m┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛\x1b[0m",
            "",
        ]
        .join("\r\n"),
        _ => [
            &format!("{user}Update the API docs for the v2 endpoints"),
            "",
            " \x1b[38;5;114m✓\x1b[0m Updated docs/api/v2.md, docs/api/errors.md",
            " \x1b[38;5;114m✓\x1b[0m Links checked: 48 ok · see \x1b]8;;https://github.com/TomND/copilot-canvas\x1b\\the PR\x1b]8;;\x1b\\",
            " \x1b[38;5;75m●\x1b[0m Done. 2 files changed, +120 −34 — emoji ok 🎉, 日本語 ok",
            "",
        ]
        .join("\r\n"),
    }
}

impl Pairing for Mock {
    fn wait(&self) -> Duration {
        Duration::ZERO
    }
    fn codes(&self) -> Vec<String> {
        vec![self.config.code.clone()]
    }
    fn grant(&self, _code: &str, name: &str) -> Result<PairGrant, String> {
        let client_id = format!("pc-{:016x}", u64::from_le_bytes(pairing::new_secret()[..8].try_into().unwrap()));
        let secret = pairing::new_secret();
        let mut paired = lock(&self.paired);
        paired.insert(client_id.clone(), secret.clone());
        let file: HashMap<&String, String> = paired.iter().map(|(k, v)| (k, canvas_common::paths::hex(v))).collect();
        let _ = std::fs::write(self.config.state_dir.join("clients.json"), serde_json::to_vec_pretty(&file).unwrap_or_default());
        log(self, &format!("paired {client_id} ({name})"));
        Ok(PairGrant { client_id, secret })
    }
    fn failed(&self) {}
}

fn serve_client(mock: &Arc<Mock>, stream: TcpStream) -> Result<()> {
    let (reader, writer, closer) = tcp::split(stream)?;
    closer.set_deadline(Some(Duration::from_secs(10)));
    let mut reader: Box<dyn Read + Send> = Box::new(reader);
    let mut writer: Box<dyn Write + Send> = Box::new(writer);
    let mut first = [0u8; 4];
    reader.read_exact(&mut first)?;
    let websocket = ws::is_http_prefix(&first);
    let (reader, writer): (Box<dyn Read + Send>, Box<dyn Write + Send>) = if websocket {
        ws::accept(&first, &mut reader, &mut writer)?;
        let (r, w) = ws::split(ws::Role::Server, reader, writer);
        (Box::new(r), Box::new(w))
    } else {
        (Box::new(std::io::Cursor::new(first.to_vec()).chain(reader)), writer)
    };
    let mut reader = FrameReader::new(reader).with_limit(PRE_AUTH_FRAME);
    let mut writer = FrameWriter::new(writer);
    let opening = handshake::serve(
        &mut reader,
        &mut writer,
        true,
        BUILD,
        |id| match id.and_then(|id| lock(&mock.paired).get(id).cloned()) {
            Some(secret) => Credential::Secret(secret),
            None => Credential::Refused,
        },
        Some(&**mock as &dyn Pairing),
    );
    closer.set_deadline(None);
    let accepted = match opening? {
        Opening::Client(accepted) => accepted,
        Opening::Paired { .. } => return Ok(()),
    };
    let info = accepted.client.unwrap_or_default();
    reader.set_limit(MAX_FRAME);
    reader.accept_compressed()?;
    if info.caps & CAP_COMPRESS != 0 {
        writer.compress()?;
    }
    let id = mock.next_client.fetch_add(1, Ordering::Relaxed);
    let (out_tx, out_rx) = bounded::<ServiceMsg>(4096);
    let tcp_closer = closer.clone();
    let client = Arc::new(Client { id, remote_id: info.remote_id.clone(), out: out_tx, subs: Mutex::new(HashMap::new()), focused: Mutex::new(None), closer: canvas_common::wire::Closer::new(move || tcp_closer.close()) });
    log(mock, &format!("client {id} connected ({}{})", info.name, if websocket { ", WebSocket" } else { "" }));
    let liveness = Arc::new(Liveness::standard());
    liveness.received();
    // Writer: queued messages and heartbeats.
    {
        let (closer, liveness) = (client.closer.clone(), liveness.clone());
        std::thread::spawn(move || {
            let mut beat = Beat::new(Duration::from_secs(2));
            loop {
                match out_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(message) => {
                        if writer.send(&message).is_err_and(|e| e.kind() != std::io::ErrorKind::InvalidData) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if beat.due(Instant::now()) && writer.send(&ServiceMsg::Heartbeat { input_ack: 0 }).is_err() {
                    break;
                }
                if liveness.dead() {
                    break;
                }
            }
            closer.close();
        });
    }
    let _ = client.out.send(ServiceMsg::Ready8 { pid: std::process::id(), input_ack: 0 });
    let terms: Vec<TermInfo> = lock(&mock.terms).values().map(|t| t.info.clone()).collect();
    let _ = client.out.send(ServiceMsg::Terminals(terms));
    let sessions: Vec<SessionSummary> = lock(&mock.sessions).values().cloned().collect();
    let _ = client.out.send(ServiceMsg::Sessions { full: true, upsert: sessions, remove: Vec::new() });
    let _ = client.out.send(ServiceMsg::Bus { json: serde_json::to_string(&canvas_model::bus::Snapshot::default())? });
    if info.caps & canvas_common::protocol::CAP_TUNNEL_ACCESS != 0
        && let Some(access) = lock(&mock.access).clone()
    {
        let _ = client.out.send(ServiceMsg::TunnelAccess { url: access.url, token: access.token, expires_ms: access.expires_ms });
    }
    lock(&mock.clients).insert(id, client.clone());
    let result = (|| -> Result<()> {
        while let Some(message) = reader.recv::<ClientMsg>()? {
            liveness.received();
            if let Err(error) = dispatch(mock, &client, message) {
                let _ = client.out.send(ServiceMsg::Error { id: None, message: format!("{error:#}") });
            }
        }
        Ok(())
    })();
    lock(&mock.clients).remove(&id);
    client.closer.close();
    let targets: Vec<(String, Option<(u16, u16)>)> = lock(&mock.sizes).iter_mut().map(|(t, p)| (t.clone(), p.leave(id))).collect();
    for (term, target) in targets {
        apply_size(mock, &term, target);
    }
    log(mock, &format!("client {id} disconnected"));
    result
}

fn broadcast(mock: &Mock, message: ServiceMsg) {
    for client in lock(&mock.clients).values() {
        let _ = client.out.try_send(message.clone());
    }
}

fn apply_size(mock: &Mock, term: &str, size: Option<(u16, u16)>) {
    if let (Some((cols, rows)), Some(t)) = (size, lock(&mock.terms).get(term)) {
        let _ = t.ctl.send(Ctl::Resize(cols, rows));
    }
}

fn claim(mock: &Mock, client: u64, term: &str) {
    let target = {
        let mut sizes = lock(&mock.sizes);
        let policy = sizes.entry(term.to_owned()).or_default();
        if policy.owner() == Some(client) {
            return;
        }
        policy.claim(client)
    };
    apply_size(mock, term, target);
}

fn term_ctl(mock: &Mock, id: &str) -> Result<Sender<Ctl>> {
    lock(&mock.terms).get(id).map(|t| t.ctl.clone()).with_context(|| format!("no terminal {id}"))
}

fn dispatch(mock: &Arc<Mock>, client: &Arc<Client>, message: ClientMsg) -> Result<()> {
    match message {
        ClientMsg::Heartbeat | ClientMsg::Track { .. } | ClientMsg::Routing { .. } | ClientMsg::Stats | ClientMsg::RefreshSessions => {}
        ClientMsg::DocOpen { .. } => {
            let (doc, version) = lock(&mock.doc).clone();
            let reply = DocReply { path: "/mock/canvas.json".into(), client: client.id, doc: Some(doc), error: None };
            let _ = client.out.send(ServiceMsg::Doc { key: "canvas-mock".into(), version, json: serde_json::to_string(&reply)? });
        }
        ClientMsg::DocEdit { key, ops_json, .. } => {
            let ops: Vec<Op> = serde_json::from_str(&ops_json)?;
            let mut guard = lock(&mock.doc);
            let mut next = guard.0.clone();
            sync::apply_all(&mut next, &ops)?;
            next.validate()?;
            guard.0 = next;
            guard.1 += 1;
            let version = guard.1;
            let saved = serde_json::to_vec_pretty(&guard.0).unwrap_or_default();
            drop(guard);
            let _ = canvas_common::paths::write_atomic(&mock.config.state_dir.join("canvas.json"), &saved);
            broadcast(mock, ServiceMsg::DocPatch { key, version, origin: client.id, ops_json });
        }
        ClientMsg::Subscribe8 { ids, focused } => {
            let hidden: Vec<String> = {
                let mut subs = lock(&client.subs);
                let hidden = subs.keys().filter(|k| !ids.contains(k)).cloned().collect();
                subs.retain(|k, _| ids.contains(k));
                for id in ids.iter().take(16) {
                    subs.entry(id.clone()).or_default();
                }
                hidden
            };
            *lock(&client.focused) = focused;
            for id in hidden {
                let target = lock(&mock.sizes).get_mut(&id).and_then(|p| p.leave(client.id));
                apply_size(mock, &id, target);
            }
            for id in ids {
                if let Ok(ctl) = term_ctl(mock, &id) {
                    let _ = ctl.send(Ctl::Pump);
                }
            }
        }
        ClientMsg::Subscribe { ids } => return dispatch(mock, client, ClientMsg::Subscribe8 { ids, focused: None }),
        ClientMsg::Resize { id, cols, rows } => {
            term_ctl(mock, &id)?;
            let target = lock(&mock.sizes).entry(id.clone()).or_default().wish(client.id, cols, rows);
            apply_size(mock, &id, target);
        }
        ClientMsg::Claim { id } => {
            term_ctl(mock, &id)?;
            claim(mock, client.id, &id);
        }
        ClientMsg::Input { id, input } | ClientMsg::Input8 { id, input, .. } => {
            let ctl = term_ctl(mock, &id)?;
            if !matches!(input, Input::Focus(_)) {
                claim(mock, client.id, &id);
            }
            let enter = matches!(input, Input::Key { key: canvas_common::protocol::Key::Enter, .. });
            ctl.send(Ctl::Input(input))?;
            if enter {
                simulate_turn(mock, &id);
            }
        }
        ClientMsg::Scroll { id, delta } => {
            let scroll = match delta {
                i32::MAX => Scroll::Top,
                i32::MIN => Scroll::Bottom,
                d => Scroll::Delta(d),
            };
            term_ctl(mock, &id)?.send(Ctl::Scroll(scroll))?;
        }
        ClientMsg::Wheel { id, up, col, row, mods } => term_ctl(mock, &id)?.send(Ctl::Wheel { up, col, row, mods })?,
        ClientMsg::Click { id, col, row, mods, .. } => {
            claim(mock, client.id, &id);
            term_ctl(mock, &id)?.send(Ctl::Click { col, row, mods })?;
        }
        ClientMsg::Launch { id, kind, cwd, cols, rows } => {
            if lock(&mock.terms).get(&id).is_some_and(|t| t.info.alive) {
                return Ok(());
            }
            if let Err(error) = launch(mock, &id, kind, &cwd, cols, rows) {
                let _ = client.out.send(ServiceMsg::Error { id: Some(id), message: format!("{error:#}") });
                return Ok(());
            }
            let mut sizes = lock(&mock.sizes);
            let policy = sizes.entry(id).or_default();
            policy.wish(client.id, cols, rows);
            policy.claim(client.id);
        }
        ClientMsg::Stop { id, .. } => term_ctl(mock, &id)?.send(Ctl::Stop)?,
        ClientMsg::Forget { id } => {
            let gone = lock(&mock.terms).remove(&id).is_some();
            lock(&mock.sizes).remove(&id);
            if gone {
                broadcast(mock, ServiceMsg::TerminalGone(id));
            }
        }
        ClientMsg::Rpc { id, json } => {
            let response = match serde_json::from_str::<Request>(&json) {
                Ok(request) => rpc(mock, request),
                Err(error) => Response::Error(format!("the mock does not support this request: {error}")),
            };
            let _ = client.out.send(ServiceMsg::RpcReply { id, json: serde_json::to_string(&response)? });
        }
        ClientMsg::Shutdown => bail!("remote clients cannot shut the service down"),
        ClientMsg::PushRegister { token, environment, bundle, label } => {
            let client_id = client.remote_id.clone().context("only paired phones receive notifications")?;
            if token.is_empty() {
                mock.devices.remove(Some(&client_id), None);
            } else if mock.devices.register(canvas_push::Device::new(&client_id, &token, &environment, &bundle, &label)?) {
                let key = if mock.sender.is_some() { "" } else { " (no APNs key: pass --apns-key)" };
                log(mock, &format!("push: {client_id} registered as {label:?} ({environment}){key}"));
            }
        }
        ClientMsg::Bus { .. } => {}
        other => bail!("the mock does not handle {other:?}"),
    }
    Ok(())
}

fn rpc(mock: &Mock, request: Request) -> Response {
    match request {
        Request::Info => Response::Info(BackendInfo {
            home: mock.config.state_dir.display().to_string(),
            user_home: std::env::var("HOME").unwrap_or_else(|_| mock.config.cwd.clone()),
            sessions_dir: String::new(),
            automation_db: String::new(),
            default_agent: if mock.config.real_agents { installed_agents().first().copied().unwrap_or_default() } else { canvas_model::agent::AgentKind::Copilot },
            installed: if mock.config.real_agents { installed_agents() } else { canvas_model::agent::ALL.to_vec() },
        }),
        Request::ListDir { dir } => {
            let dir = dir.unwrap_or_else(|| "/".into());
            let mut entries: Vec<String> = std::fs::read_dir(&dir)
                .map(|it| it.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect())
                .unwrap_or_default();
            entries.sort_by_key(|n| n.to_lowercase());
            entries.insert(0, "..".into());
            Response::Texts(entries)
        }
        Request::PathInfo { path } => {
            let p = std::path::Path::new(&path);
            let absolute = std::fs::canonicalize(p).map(|a| a.display().to_string()).unwrap_or(path.clone());
            Response::Path(PathInfo { exists: p.exists(), dir: p.is_dir(), file: p.is_file(), absolute, occupied: p.read_dir().map(|mut d| d.next().is_some()).unwrap_or(p.is_file()) })
        }
        Request::ReadText { path, max_bytes } => match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                let _ = file.take(max_bytes.min(4 << 20)).read_to_end(&mut bytes);
                Response::Text(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(error) => Response::Error(format!("{path}: {error}")),
        },
        Request::GitProbe { .. } => Response::Git(None),
        // Copilot's history is not here; forks are the agents' own (Claude Code).
        Request::HasHistory { .. } => Response::Flag(false),
        Request::SetClipboardImage { png } => match set_clipboard_image(&mock.config.state_dir, &png) {
            Ok(()) => Response::Done,
            Err(error) => Response::Error(format!("{error:#}")),
        },
        Request::DirsExist { paths } => Response::Flags(paths.iter().map(|p| std::path::Path::new(p).is_dir()).collect()),
        other => Response::Error(format!("the mock service does not support {}", other.name())),
    }
}

// ---------- terminals ----------

/// What a launch runs in the mock: a login shell (agents announce themselves).
fn launch(mock: &Arc<Mock>, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16) -> Result<()> {
    launch_with(mock, id, kind, cwd, cols, rows, String::new())
}

fn launch_with(mock: &Arc<Mock>, id: &str, kind: LaunchKind, cwd: &str, cols: u16, rows: u16, transcript: String) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let (mut command, banner, agent) = match kind {
        LaunchKind::Command { program, args } => {
            let mut c = CommandBuilder::new(program);
            c.args(args);
            (c, String::new(), false)
        }
        LaunchKind::Agent(launch) if mock.config.real_agents && transcript.is_empty() && agent_command(&launch, id).is_some() => {
            let command = agent_command(&launch, id).expect("checked");
            lock(&mock.real).insert(id.to_owned());
            log(mock, &format!("{id}: real {} ({})", launch.agent, if launch.resume { "continue" } else { "new" }));
            // A real agent: no stand-in transcript, no simulated turns.
            (command, String::new(), false)
        }
        LaunchKind::Agent(launch) => {
            let mut c = CommandBuilder::new(&shell);
            c.arg("-l");
            let how = if launch.resume { "resumed" } else { "new session" };
            let prompt = launch.prompt.map(|p| format!("\r\n\x1b[2mfirst prompt:\x1b[0m {p}")).unwrap_or_default();
            let intro = if transcript.is_empty() {
                format!("\x1b[1;38;5;75m● {} (mock)\x1b[0m · {how} · {}{prompt}\r\n", launch.agent, launch.name.unwrap_or_default())
            } else {
                format!("{transcript}\r\n")
            };
            (c, format!("{intro}\x1b[2m(mock: a shell stands in for the agent; Enter makes it work for 4 s)\x1b[0m\r\n"), true)
        }
        other => bail!("the mock does not launch {other:?}"),
    };
    let folder = if std::path::Path::new(cwd).is_dir() { cwd.to_owned() } else { mock.config.cwd.clone() };
    command.cwd(&folder);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let (cols, rows) = (cols.clamp(20, 1000), rows.clamp(5, 500));
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut child = pair.slave.spawn_command(command).map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow::anyhow!("{e}"))?;
    let writer = pair.master.take_writer().map_err(|e| anyhow::anyhow!("{e}"))?;
    let (out_tx, out_rx) = unbounded::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || out_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let (ctl_tx, ctl_rx) = unbounded::<Ctl>();
    let info = TermInfo { id: id.to_owned(), alive: true, pid: child.process_id().unwrap_or(0), started_ms: now_ms(), copilot: agent, ..Default::default() };
    lock(&mock.terms).insert(id.to_owned(), Term { ctl: ctl_tx, info: info.clone() });
    broadcast(mock, ServiceMsg::Terminals(vec![info]));
    if agent || lock(&mock.real).contains(id) {
        set_activity(mock, id, |s| {
            s.live = true;
            s.activity = Activity::Idle;
            s.pending = 0;
        });
    }
    let (mock2, id2) = (mock.clone(), id.to_owned());
    std::thread::spawn(move || {
        terminal_loop(&mock2, &id2, cols, rows, banner, pair.master, writer, out_rx, ctl_rx);
        let code = child.wait().ok().map(|s| s.exit_code()).unwrap_or(1);
        let _ = child.kill();
        let info = lock(&mock2.terms).get_mut(&id2).map(|t| {
            t.info.alive = false;
            t.info.exit_code = Some(code);
            t.info.clone()
        });
        if let Some(info) = info {
            broadcast(&mock2, ServiceMsg::Terminals(vec![info]));
        }
        set_activity(&mock2, &id2, |s| s.live = false);
        log(&mock2, &format!("terminal {id2} exited ({code})"));
    });
    log(mock, &format!("launched {id} in {folder}"));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn terminal_loop(mock: &Arc<Mock>, id: &str, cols: u16, rows: u16, banner: String, master: Box<dyn MasterPty + Send>, mut writer: Box<dyn Write + Send>, output: Receiver<Vec<u8>>, ctl: Receiver<Ctl>) {
    let mut emu = emulator::create(Engine::Alacritty, &emulator::Config { cols, rows, scrollback: 5000, kitty_keyboard: true });
    let mut events = Collected::default();
    if !banner.is_empty() {
        emu.feed(banner.as_bytes(), &mut events);
    }
    loop {
        crossbeam_channel::select! {
            recv(output) -> bytes => match bytes {
                Ok(bytes) => {
                    let mut events = Collected::default();
                    emu.feed(&bytes, &mut events);
                    for reply in events.replies {
                        let _ = writer.write_all(&reply);
                    }
                    let mut changed = false;
                    if let Some(info) = lock(&mock.terms).get_mut(id) {
                        info.info.last_output_ms = now_ms();
                        if let Some(title) = events.title {
                            info.info.title = title.unwrap_or_default();
                            changed = true;
                        }
                        if events.bells > 0 {
                            info.info.bells += events.bells;
                            changed = true;
                        }
                        // OSC 9;4 progress (agents report turns with it): drives "working".
                        if let Some(&progress) = events.progress.last() {
                            info.info.progress = progress;
                            info.info.progress_seen = true;
                            info.info.progress_ms = now_ms();
                            changed = true;
                        }
                    }
                    if changed && let Some(info) = lock(&mock.terms).get(id).map(|t| t.info.clone()) {
                        broadcast(mock, ServiceMsg::Terminals(vec![info]));
                    }
                    // A real agent's turns: its progress report (Claude Code and
                    // Copilot send OSC 9;4), and a bell when it waits for you.
                    if lock(&mock.real).contains(id) {
                        if let Some(&progress) = events.progress.last() {
                            let busy = matches!(progress, 1 | 3);
                            set_activity(mock, id, |s| {
                                if busy {
                                    s.activity = Activity::Working;
                                    s.finished_ms = None;
                                } else if s.activity == Activity::Working {
                                    s.activity = Activity::Idle;
                                    s.finished_ms = Some(now_ms());
                                }
                            });
                        }
                        if events.bells > 0 || events.notifications > 0 {
                            set_activity(mock, id, |s| s.pending = 1);
                        }
                    }
                    for text in events.clipboard {
                        broadcast(mock, ServiceMsg::Clipboard(text));
                    }
                }
                Err(_) => break,
            },
            recv(ctl) -> command => match command {
                Ok(Ctl::Input(input)) => {
                    // Typing answers what the agent was waiting for.
                    if lock(&mock.real).contains(id) && lock(&mock.sessions).get(id).is_some_and(|s| s.pending > 0) {
                        set_activity(mock, id, |s| s.pending = 0);
                    }
                    emu.scroll(Scroll::Bottom);
                    let bytes = emu.encode(&input);
                    let _ = writer.write_all(&bytes);
                }
                Ok(Ctl::Resize(c, r)) => {
                    let _ = master.resize(PtySize { rows: r.clamp(5, 500), cols: c.clamp(20, 1000), pixel_width: 0, pixel_height: 0 });
                    emu.resize(c.clamp(20, 1000), r.clamp(5, 500));
                }
                Ok(Ctl::Scroll(scroll)) => emu.scroll(scroll),
                Ok(Ctl::Wheel { up, col, row, mods }) => match emu.encode_wheel(up, col, row, mods) {
                    Some(bytes) => {
                        let _ = writer.write_all(&bytes);
                    }
                    None => emu.scroll(Scroll::Delta(if up { 3 } else { -3 })),
                },
                Ok(Ctl::Click { col, row, mods }) => {
                    if let Some(bytes) = emu.encode_click(0, col, row, mods) {
                        let _ = writer.write_all(&bytes);
                    }
                }
                Ok(Ctl::Pump) => {}
                Ok(Ctl::Stop) | Err(_) => {
                    // Ctrl+D, then the shell is gone with its pty.
                    let _ = writer.write_all(b"\x04");
                    drop(writer);
                    drop(master);
                    return;
                }
            },
            default(Duration::from_millis(16)) => {}
        }
        send_frames(mock, id, &emu.snapshot());
    }
}

fn row_hash(row: &[Run8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    row.hash(&mut hasher);
    hasher.finish()
}

/// Sends each subscriber the rows that changed (at most every 16 ms for the focused tile, 100 ms otherwise).
fn send_frames(mock: &Mock, id: &str, snap: &Snapshot) {
    let hashes: Vec<u64> = snap.lines.iter().map(|r| row_hash(r)).collect();
    let clients: Vec<Arc<Client>> = lock(&mock.clients).values().cloned().collect();
    for client in clients {
        let focused = lock(&client.focused).as_deref() == Some(id);
        let mut subs = lock(&client.subs);
        let Some(sent) = subs.get_mut(id) else { continue };
        let interval = if focused { Duration::from_millis(16) } else { Duration::from_millis(100) };
        if sent.at.is_some_and(|at| at.elapsed() < interval) {
            continue;
        }
        let full = sent.cols != snap.cols || sent.rows != snap.rows || sent.hashes.len() != hashes.len();
        let changed: Vec<(u16, Vec<Run8>)> = snap.lines.iter().enumerate().filter(|(y, _)| full || sent.hashes[*y] != hashes[*y]).map(|(y, r)| (y as u16, r.clone())).collect();
        let same = changed.is_empty() && sent.cursor == snap.cursor && sent.shape == snap.cursor_shape && sent.modes == snap.modes && sent.offset == snap.display_offset;
        if same {
            continue;
        }
        let frame = Frame8 {
            id: id.to_owned(),
            cols: snap.cols,
            rows: snap.rows,
            full,
            changed,
            cursor: snap.cursor,
            cursor_shape: snap.cursor_shape,
            cursor_blink: snap.cursor_blink,
            display_offset: snap.display_offset,
            history: snap.history,
            modes: snap.modes,
            links: snap.links.clone(),
        };
        if client.out.try_send(ServiceMsg::Frame8(frame)).is_ok() {
            *sent = Sent { cols: snap.cols, rows: snap.rows, hashes: hashes.clone(), cursor: snap.cursor, shape: snap.cursor_shape, modes: snap.modes, offset: snap.display_offset, at: Some(Instant::now()) };
        }
    }
}

// ---------- sessions and statuses ----------

fn set_activity(mock: &Mock, id: &str, change: impl FnOnce(&mut SessionSummary)) {
    let summary = {
        let mut sessions = lock(&mock.sessions);
        let summary = sessions.entry(id.to_owned()).or_insert_with(|| SessionSummary { id: id.to_owned(), tracked: true, ..Default::default() });
        change(summary);
        summary.last_event_ms = now_ms();
        summary.clone()
    };
    broadcast(mock, ServiceMsg::Sessions { full: false, upsert: vec![summary], remove: Vec::new() });
}

/// Enter in an agent's terminal: the "agent" works for 4 s, then finishes its turn.
fn simulate_turn(mock: &Arc<Mock>, id: &str) {
    if !lock(&mock.terms).get(id).is_some_and(|t| t.info.copilot) {
        return;
    }
    // Answering (Enter) clears a pending question.
    set_activity(mock, id, |s| {
        s.activity = Activity::Working;
        s.pending = 0;
        s.finished_ms = None;
    });
    let (mock, id) = (mock.clone(), id.to_owned());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        set_activity(&mock, &id, |s| {
            s.activity = Activity::Idle;
            s.finished_ms = Some(now_ms());
        });
    });
}

/// Statuses that change over time: the "Review PR" session asks for a
/// permission every minute, and its subagent finishes.
fn tick_loop(mock: Arc<Mock>) {
    let mut n = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(20));
        n += 1;
        set_activity(&mock, "mock-review", |s| {
            s.pending = u16::from(n.is_multiple_of(3));
            s.activity = if n.is_multiple_of(3) { Activity::Working } else { Activity::Idle };
            if n % 3 == 1 {
                s.finished_ms = Some(now_ms());
            }
        });
    }
}

fn session(id: &str, name: &str, x: i32, y: i32, cwd: &str, agent: Option<&str>) -> Node {
    serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": x, "y": y, "cwd": cwd, "agent": agent })).expect("a node")
}

/// The mock's canvas: two groups, a few sessions in each state, subagents.
pub fn demo_doc(cwd: &str) -> Doc {
    let mut doc = Doc::default();
    doc.groups.push(Group { id: "g-payments".into(), name: "Payments".into(), x: 0, y: 0, w: 72, h: 24, color: 0, folder: None, source: None, defaults: None });
    doc.groups.push(Group { id: "g-docs".into(), name: "Docs site".into(), x: 76, y: 0, w: 38, h: 16, color: 2, folder: None, source: None, defaults: None });
    doc.nodes.push(session("mock-fix", "Fix flaky test", 2, 2, cwd, None));
    let mut agent: Node = session("mock-fix:explore", "Explore test runner", 6, 7, cwd, None);
    agent.kind = NodeKind::Agent;
    agent.parent = Some("mock-fix".into());
    agent.agent_key = Some("explore".into());
    doc.nodes.push(agent);
    doc.nodes.push(session("mock-review", "Review PR #42", 36, 2, cwd, Some("opencode")));
    let mut sub = session("mock-tests", "Write tests", 36, 12, cwd, Some("claude"));
    sub.parent = Some("mock-review".into());
    doc.nodes.push(sub);
    doc.nodes.push(session("mock-docs", "Update API docs", 78, 2, cwd, None));
    doc.nodes.push(session("mock-loose", "Scratch", 2, 28, cwd, None));
    // A passive session (runs on events and schedules; its actions are the bus's).
    let mut drift = session("mock-drift", "Drift guard", 2, 18, cwd, None);
    drift.passive = Some(serde_json::from_value(serde_json::json!({ "key": "drift", "group": "g-payments", "slug": "drift-guard", "every_minutes": 60 })).expect("a passive spec"));
    doc.nodes.push(drift);
    // Looked at a while ago: turns finished since then wait for review.
    let seen = now_ms() - 120_000;
    for node in doc.nodes.iter_mut().filter(|n| n.kind == NodeKind::Session) {
        node.reviewed_ms = Some(seen);
    }
    doc
}

fn demo_sessions(doc: &Doc) -> HashMap<String, SessionSummary> {
    let now = now_ms();
    let mut sessions = HashMap::new();
    for node in doc.nodes.iter().filter(|n| n.kind == NodeKind::Session) {
        sessions.insert(node.id.clone(), SessionSummary { id: node.id.clone(), name: node.name.clone(), cwd: node.cwd.clone().unwrap_or_default(), branch: "main".into(), tracked: true, activity: Activity::Idle, updated_ms: now, last_event_ms: now - 60_000, ..Default::default() });
    }
    if let Some(s) = sessions.get_mut("mock-fix") {
        s.agents.push(AgentSummary { key: "explore".into(), name: "Explore test runner".into(), description: "explore".into(), status: AgentStatus::Finished, finished_ms: Some(now - 30_000) });
    }
    if let Some(s) = sessions.get_mut("mock-docs") {
        s.finished_ms = Some(now - 5_000);
        s.loop_ms = 15 * 60_000;
    }
    sessions
}
