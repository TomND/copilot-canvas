//! `copilot-canvas remote …` (PLAN.md D5): on the service's machine `pair`,
//! `clients`, `revoke` and `tunnel-setup`; on the client's machine `add`,
//! `servers` and `forget`. Also the doctor's remote checks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canvas_common::remote::{self, ServerRecord, Servers};
use canvas_common::settings::Settings;
use canvas_common::{dpapi, pairing};

use crate::client::conn::{self, ClientSession, Conn, ConnOptions, ConnTarget};
use crate::client::remote::RemoteBackend;
use crate::client::rpc::Backend;
use crate::client::tunnel::Server;
use crate::paths::{Instance, format_age, now_ms};

/// Names `--remote` already uses for something else.
const RESERVED: [&str; 3] = ["loopback", "faults", "paired"];

/// The local service over its pipe (started when needed), with its backend.
fn service(instance: &Instance) -> Result<(Conn, Arc<RemoteBackend>)> {
    let backend = RemoteBackend::new();
    let options = ConnOptions { start: true, name: "cli".into(), rpc: Some(backend.clone()), ..Default::default() };
    let conn = conn::open(&ConnTarget::Local(instance.clone()), &options, &ClientSession::new()).context("cannot reach the local service")?;
    anyhow::ensure!(conn.protocol >= 8, "the running service is too old for pairing; restart it (`copilot-canvas service stop`)");
    Ok((conn, backend))
}

/// The port of `remote.listen` (or the default).
fn listen_port(settings: &Settings) -> u16 {
    settings.listen().and_then(|a| a.rsplit_once(':').and_then(|(_, p)| p.parse().ok())).filter(|p| *p != 0).unwrap_or(remote::DEFAULT_PORT)
}

fn date(ms: u64) -> String {
    let text = crate::paths::format_rfc3339_ms(ms);
    format!("{} {} UTC", &text[..10], &text[11..16])
}

/// `remote pair [--name NAME]`: a one-time code from the running service.
pub fn pair(instance: &Instance, name: Option<String>) -> Result<()> {
    if let Some(name) = &name {
        anyhow::ensure!(remote::valid_name(name), "--name takes 1-40 characters of letters, digits, '-', '_' and '.'");
    }
    let (_conn, backend) = service(instance)?;
    let backend: &dyn Backend = &*backend;
    let code = backend.pair_code(name.clone())?;
    let settings = Settings::load(&instance.home);
    let tunnel = settings.remote.tunnel.clone().unwrap_or_else(|| remote::DEFAULT_TUNNEL.into());
    let port = code.listen.rsplit_once(':').map(|(_, p)| p.to_owned()).unwrap_or_default();
    let minutes = pairing::CODE_TTL_MS / 60_000;
    println!("Pairing code: {}   (one use, valid for {minutes} minutes)", code.code);
    if let Some(name) = &code.name {
        println!("The client will be listed as {name}.");
    }
    println!("The service listens for remote clients on {} (behind the Dev Tunnel; never exposed directly).", code.listen);
    println!();
    println!("On the home PC run:");
    println!("  copilot-canvas remote add cloudpc --tunnel {tunnel} --port {port} --code {}", code.code);
    println!("Without a tunnel (an address that forwards to this port):");
    println!("  copilot-canvas remote add cloudpc --addr HOST:PORT --code {}", code.code);
    println!("Then open the canvas with: copilot-canvas --remote cloudpc");
    if !canvas_agents::remote::tunnel_manifest_path(&canvas_agents::factory_cli::configured_dir(&instance.home, None)).is_file() {
        println!();
        println!("No tunnel automation yet: set it up with `copilot-canvas remote tunnel-setup`.");
    }
    Ok(())
}

/// `remote clients`: the paired clients.
pub fn clients(instance: &Instance) -> Result<()> {
    let (_conn, backend) = service(instance)?;
    let backend: &dyn Backend = &*backend;
    let list = backend.remote_clients()?;
    if list.is_empty() {
        println!("No paired clients. Pair one with: copilot-canvas remote pair --name NAME");
        return Ok(());
    }
    let now = now_ms();
    println!("{:<20} {:<24} {:<22} {:<12} STATE", "ID", "NAME", "PAIRED", "LAST SEEN");
    for c in &list {
        let seen = if c.last_seen_ms == 0 { "never".to_owned() } else { format!("{} ago", format_age(c.last_seen_ms, now)) };
        let state = match (c.revoked_ms, c.connections) {
            (0, 0) => "active".to_owned(),
            (0, n) => format!("connected ({n})"),
            (at, _) => format!("revoked {}", date(at)),
        };
        println!("{:<20} {:<24} {:<22} {:<12} {state}", c.id, c.name, date(c.created_ms), seen);
    }
    Ok(())
}

/// `remote revoke ID|NAME`.
pub fn revoke(instance: &Instance, client: &str) -> Result<()> {
    let (_conn, backend) = service(instance)?;
    let backend: &dyn Backend = &*backend;
    println!("{}", backend.remote_revoke(client)?);
    Ok(())
}

/// `remote tunnel-setup [--tunnel NAME] [--port PORT]`: the tunnel
/// automation's manifest, `remote.listen`, and the one-time Dev Tunnels steps.
pub fn tunnel_setup(instance: &Instance, dir: &Path, tunnel: Option<String>, port: Option<u16>) -> Result<()> {
    let mut settings = Settings::load(&instance.home);
    let tunnel = tunnel.or_else(|| settings.remote.tunnel.clone()).unwrap_or_else(|| remote::DEFAULT_TUNNEL.into());
    anyhow::ensure!(
        !tunnel.is_empty() && tunnel.len() <= 60 && tunnel.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "a tunnel id is lower-case letters, digits and '-'"
    );
    let port = port.unwrap_or_else(|| listen_port(&settings));
    anyhow::ensure!(port != 0, "--port must not be 0");
    let devtunnel = remote::find_devtunnel_exe();
    let program = devtunnel.clone().unwrap_or_else(|| PathBuf::from("devtunnel.exe"));
    let path = canvas_agents::remote::tunnel_manifest_path(dir);
    let text = canvas_agents::remote::tunnel_manifest(&program, &tunnel, port);
    let existed = path.is_file();
    std::fs::create_dir_all(path.parent().context("manifest folder")?)?;
    std::fs::write(&path, &text).with_context(|| format!("cannot write {}", path.display()))?;
    settings.remote.listen = Some(format!("127.0.0.1:{port}"));
    settings.remote.tunnel = Some(tunnel.clone());
    settings.save(&instance.home).context("cannot save settings.json")?;
    println!("{} {}", if existed { "Updated" } else { "Wrote" }, path.display());
    println!("Set remote.listen = 127.0.0.1:{port} in {} (the service listens there; `remote pair` also turns it on in a running service).", canvas_common::settings::path(&instance.home).display());
    match &devtunnel {
        Some(exe) => println!("Dev Tunnels CLI: {}", exe.display()),
        None => println!("Dev Tunnels CLI not found: install it (`winget install Microsoft.devtunnel`) and run this again."),
    }
    let on_path = devtunnel.as_ref().and_then(|p| p.parent()).is_some_and(|d| std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|p| p == d));
    println!();
    println!("Once, on this machine (PowerShell; signing in opens a browser):");
    let dt = if on_path {
        "devtunnel".to_owned()
    } else {
        println!("  $dt = \"{}\"", program.display());
        "& $dt".to_owned()
    };
    println!("  {dt} user login -g                  # GitHub (`user login` for a Microsoft account)");
    println!("  {dt} create {tunnel}         # private: only you can connect");
    println!("  {dt} port create {tunnel} -p {port}");
    println!("Then register and start the tunnel host:");
    println!("  copilot-canvas automations apply {} --yes", remote::TUNNEL_TASK);
    println!("  copilot-canvas automations run {}", remote::TUNNEL_TASK);
    println!("Then pair the home PC: copilot-canvas remote pair --name home-pc");
    println!("Check it any time with: copilot-canvas doctor");
    Ok(())
}

/// What `remote add` was given.
pub struct AddArgs {
    pub name: String,
    pub tunnel: Option<String>,
    pub port: Option<u16>,
    pub addr: Option<String>,
    pub code: String,
    pub vscode_tunnel: Option<String>,
    /// What the service lists this machine as (default: its computer name).
    pub client_name: Option<String>,
}

/// `remote add NAME (--tunnel ID [--port PORT] | --addr HOST:PORT) --code CODE`:
/// pairs with the service and stores the credentials (DPAPI) in
/// `<home>/remote/servers.json`.
pub fn add(instance: &Instance, args: AddArgs) -> Result<()> {
    anyhow::ensure!(remote::valid_name(&args.name), "a server name is 1-40 characters of letters, digits, '-', '_' and '.'");
    anyhow::ensure!(!RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&args.name)), "{:?} is reserved; choose another name", args.name);
    let code = pairing::normalize_code(&args.code).context("the code looks like ABCD-EFGH (from `copilot-canvas remote pair` on the service's machine)")?;
    if args.tunnel.is_some() == args.addr.is_some() {
        bail!("give either --tunnel TUNNEL_ID [--port PORT] or --addr HOST:PORT");
    }
    if let Some(addr) = &args.addr {
        anyhow::ensure!(addr.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok()), "--addr takes HOST:PORT");
    }
    let client_name = args.client_name.clone().unwrap_or_else(remote::machine_name);
    let mut record = ServerRecord {
        name: args.name.clone(),
        tunnel: args.tunnel.clone(),
        port: args.port.unwrap_or(remote::DEFAULT_PORT),
        addr: args.addr.clone(),
        vscode_tunnel: args.vscode_tunnel.clone(),
        client_id: String::new(),
        secret: String::new(),
        added_ms: now_ms(),
    };
    // A tunnel is connected for the pairing and stays up for the check below.
    let route = Server::new(record.clone(), Vec::new());
    if let Some(tunnel) = &record.tunnel {
        println!("Connecting to the Dev Tunnel {tunnel} (devtunnel connect)…");
    }
    let addr = route.address()?;
    let (client_id, secret) = conn::pair(&addr, &code, &client_name).context("pairing failed")?;
    record.client_id = client_id.clone();
    record.secret = dpapi::protect_hex(&secret, "Copilot Canvas server secret").context("cannot protect the secret")?;
    let mut servers = Servers::load(&instance.home)?;
    let previous = servers.find(&record.name).map(|s| s.client_id.clone());
    servers.upsert(record.clone());
    servers.save(&instance.home)?;
    // The first real connection, as the canvas will make it.
    let check = ConnTarget::Tcp { addr, remote_id: Some(client_id.clone()), secret };
    let options = ConnOptions { name: "cli".into(), heartbeat: Duration::from_secs(2), ..Default::default() };
    let conn = conn::open(&check, &options, &ClientSession::new()).context("paired, but the first connection failed")?;
    println!(
        "Paired {} with {} as {client_name} (client id {client_id}); service build {}.",
        if previous.is_some() { "again" } else { "this machine" },
        record.describe(),
        conn.service_build
    );
    if let Some(old) = previous.filter(|old| !old.is_empty() && *old != client_id) {
        println!("The previous pairing ({old}) still works on the service: revoke it there with `copilot-canvas remote revoke {old}`.");
    }
    println!("The secret is stored for this Windows user in {}.", Servers::path(&instance.home).display());
    println!("Open the canvas with: copilot-canvas --remote {}", record.name);
    Ok(())
}

/// `remote servers`: the services this machine paired with.
pub fn servers(instance: &Instance) -> Result<()> {
    let servers = Servers::load(&instance.home)?;
    if servers.servers.is_empty() {
        println!("No paired servers. Add one with: copilot-canvas remote add NAME --tunnel TUNNEL_ID --code CODE");
        return Ok(());
    }
    println!("{:<16} {:<34} {:<20} {:<16} ADDED", "NAME", "WHERE", "CLIENT ID", "VS CODE TUNNEL");
    for s in &servers.servers {
        println!("{:<16} {:<34} {:<20} {:<16} {}", s.name, s.describe(), s.client_id, s.vscode_tunnel.as_deref().unwrap_or("—"), date(s.added_ms));
    }
    Ok(())
}

/// `remote forget NAME`: removes a server and its secret from this machine.
pub fn forget(instance: &Instance, name: &str) -> Result<()> {
    let mut servers = Servers::load(&instance.home)?;
    let removed = servers.remove(name).with_context(|| format!("no server named {name:?}"))?;
    servers.save(&instance.home)?;
    println!("Forgot {} ({}). Revoke it on the service too: copilot-canvas remote revoke {}", removed.name, removed.describe(), removed.client_id);
    Ok(())
}

/// How a doctor line reads: fine, not set up (informational) or broken.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Ok,
    Info,
    Fail,
}

/// Doctor lines about remote mode: (mark, label, detail).
pub fn doctor_lines(instance: &Instance, dir: &Path) -> Vec<(Mark, &'static str, String)> {
    let mut lines = Vec::new();
    let settings = Settings::load(&instance.home);
    lines.push(match (settings.listen(), conn::listening_port(instance)) {
        (_, Some(port)) => (Mark::Ok, "Remote listening", format!("listening on 127.0.0.1:{port}")),
        (Some(addr), None) => (Mark::Info, "Remote listening", format!("configured ({addr}); the service is not running or not listening yet")),
        (None, None) => (Mark::Info, "Remote listening", "off (remote.listen not set; `remote tunnel-setup` or `remote pair` turns it on)".into()),
    });
    lines.push(match canvas_agents::remote::ClientsFile::load(&instance.home) {
        Ok(file) => {
            let active = file.active().count();
            let revoked = file.clients.len() - active;
            let detail = format!("{active} active{}", if revoked > 0 { format!(", {revoked} revoked") } else { String::new() });
            (if active > 0 { Mark::Ok } else { Mark::Info }, "Paired clients", detail)
        }
        Err(error) => (Mark::Fail, "Paired clients", format!("{error:#}")),
    });
    lines.push(match Servers::load(&instance.home) {
        Ok(servers) if servers.servers.is_empty() => (Mark::Info, "Paired servers", "none (this machine is not a remote client)".into()),
        Ok(servers) => (Mark::Ok, "Paired servers", servers.servers.iter().map(|s| format!("{} ({})", s.name, s.describe())).collect::<Vec<_>>().join(", ")),
        Err(error) => (Mark::Fail, "Paired servers", format!("{error:#}")),
    });
    lines.push(match remote::find_devtunnel_exe() {
        None => (Mark::Info, "Dev Tunnels CLI", "not installed (remote mode needs it: `winget install Microsoft.devtunnel`)".into()),
        Some(exe) => match devtunnel_user(&exe) {
            Ok(status) if status.to_ascii_lowercase().starts_with("logged in") => (Mark::Ok, "Dev Tunnels CLI", format!("{} · {status}", exe.display())),
            Ok(status) => (Mark::Info, "Dev Tunnels CLI", format!("{} · {status} (sign in once: devtunnel user login -g)", exe.display())),
            Err(error) => (Mark::Info, "Dev Tunnels CLI", format!("{} · `devtunnel user show` failed: {error:#}", exe.display())),
        },
    });
    let manifest = canvas_agents::remote::tunnel_manifest_path(dir);
    lines.push(if manifest.is_file() {
        let task = canvas_agents::taskschd::list().ok().and_then(|tasks| tasks.get(&remote::TUNNEL_TASK.to_lowercase()).map(|t| if t.enabled { t.state.clone() } else { "disabled".into() }));
        match task {
            Some(state) => (Mark::Ok, "Tunnel automation", format!("{} · task {state}", manifest.display())),
            None => (Mark::Info, "Tunnel automation", format!("{} · not registered (copilot-canvas automations apply {} --yes)", manifest.display(), remote::TUNNEL_TASK)),
        }
    } else {
        (Mark::Info, "Tunnel automation", "no manifest (copilot-canvas remote tunnel-setup)".into())
    });
    lines
}
/// `devtunnel user show --json` → its status line ("Logged in as … using GitHub.", "Not logged in").
fn devtunnel_user(exe: &Path) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let mut child = std::process::Command::new(exe)
        .args(["user", "show", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("no answer within 15 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().context("output")?, &mut text)?;
    let json: serde_json::Value = serde_json::from_str(text.trim()).with_context(|| format!("unexpected output {:?}", text.trim()))?;
    let status = json["status"].as_str().unwrap_or("unknown").to_owned();
    let who = [&json["username"], &json["provider"]].iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" via ");
    Ok(if who.is_empty() { status } else { format!("{status} ({who})") })
}
