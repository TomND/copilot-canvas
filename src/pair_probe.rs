//! `copilot-canvas selftest --remote paired`: pairing and the managed Dev
//! Tunnel end to end, without a signed-in Dev Tunnels CLI. A real service
//! (temporary data folder) plays the Cloud PC, a second data folder the home
//! PC, and `copilot-canvas devtunnel-standin` plays `devtunnel connect`.
//!
//! Checks: remote listening is off until a code is opened; wrong and reused
//! codes are refused; `remote add` over a direct address and through the
//! stand-in; wrong secrets and unknown clients are refused; the canvas with
//! `--remote NAME` (through the stand-in): a shell, a dropped connection, a
//! killed tunnel child (restarted), a remote path opened in the viewer;
//! revocation drops the canvas with a clear message; the tunnel child ends
//! with the client; `remote forget`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::client::conn::{self, AuthError, ClientSession, ConnOptions, ConnTarget, Refused};
use crate::paths::Instance;
use crate::protocol::ClientMsg;
use crate::selftest::Console;
use crate::standin;

/// Reconnecting must take less than this.
const RECONNECT_LIMIT: Duration = Duration::from_secs(10);

fn step(text: &str) {
    println!("  · {text}");
}

pub fn run(exe: &Path) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-paired-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    std::fs::create_dir_all(root.join("work"))?;
    let cloud = Instance::new(Some(format!("paired{}", std::process::id())), Some(root.join("cloud")), Some(root.join("sessions")))?;
    let result = scenario(exe, &cloud, &root);
    if let Ok(conn) = conn::try_connect(&cloud) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_millis(2000));
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

/// Runs the CLI; `Ok(stdout)` when it succeeded, `Err(stdout + stderr)` when not.
fn cli(exe: &Path, args: &[String], env: &[(&str, String)]) -> Result<Result<String, String>> {
    let mut command = Command::new(exe);
    command.args(args).stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().with_context(|| format!("cannot run {}", exe.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok(if output.status.success() { Ok(stdout) } else { Err(format!("{stdout}{stderr}")) })
}

fn ok(exe: &Path, args: &[String], env: &[(&str, String)]) -> Result<String> {
    cli(exe, args, env)?.map_err(|text| anyhow::anyhow!("`copilot-canvas {}` failed:\n{text}", args.join(" ")))
}

fn fails(exe: &Path, args: &[String], env: &[(&str, String)]) -> Result<String> {
    match cli(exe, args, env)? {
        Ok(out) => bail!("`copilot-canvas {}` should have failed:\n{out}", args.join(" ")),
        Err(text) => Ok(text),
    }
}

fn args(base: &[String], more: &[&str]) -> Vec<String> {
    more.iter().map(|s| s.to_string()).chain(base.iter().cloned()).collect()
}

/// The code `remote pair` printed.
fn code_of(output: &str) -> Result<String> {
    output.lines().find_map(|l| l.strip_prefix("Pairing code: ")).and_then(|rest| rest.split_whitespace().next()).map(str::to_owned).with_context(|| format!("no code in:\n{output}"))
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn read_pid(control: &Path) -> Option<u32> {
    std::fs::read_to_string(control.join("standin.pid")).ok()?.trim().parse().ok()
}

fn scenario(exe: &Path, cloud: &Instance, root: &Path) -> Result<()> {
    println!("Copilot Canvas pairing and tunnel self-test (a stand-in plays `devtunnel connect`; data in {})", root.display());
    let home = root.join("home");
    let cloud_args: Vec<String> = vec![
        "--instance".into(),
        cloud.name.clone(),
        "--home".into(),
        cloud.home.display().to_string(),
        "--sessions-dir".into(),
        cloud.sessions_dir.display().to_string(),
    ];
    let home_args: Vec<String> = vec!["--instance".into(), "client".into(), "--home".into(), home.display().to_string()];
    let home_instance = Instance::new(Some("client".into()), Some(home.clone()), Some(root.join("sessions")))?;

    step("the service runs without remote listening until a pairing code is opened");
    conn::spawn_service_exe(exe, cloud, &[])?;
    wait_until("the service", Duration::from_secs(15), || conn::try_connect(cloud).is_ok())?;
    anyhow::ensure!(conn::listening_port(cloud).is_none(), "the service listens before anything was paired");
    // A free port instead of 47600 (which a real service may use).
    let mut settings = canvas_common::settings::Settings::load(&cloud.home);
    settings.remote.listen = Some("127.0.0.1:0".into());
    settings.save(&cloud.home)?;
    let code = code_of(&ok(exe, &args(&cloud_args, &["remote", "pair", "--name", "test"]), &[])?)?;
    let port = conn::listening_port(cloud).context("`remote pair` turned remote listening on")?;
    let addr = format!("127.0.0.1:{port}");
    println!("    code {code}; the service now listens on {addr}");

    step("a wrong code is refused; the right one pairs once (direct address)");
    let wrong = if code.starts_with('A') { "BBBB-BBBB" } else { "AAAA-AAAA" };
    let refused = fails(exe, &args(&home_args, &["remote", "add", "test", "--addr", &addr, "--code", wrong]), &[])?;
    anyhow::ensure!(refused.contains("wrong or expired pairing code"), "unexpected refusal:\n{refused}");
    let added = ok(exe, &args(&home_args, &["remote", "add", "test", "--addr", &addr, "--code", &code, "--client-name", "probe-direct"]), &[])?;
    anyhow::ensure!(added.contains("Paired this machine"), "{added}");
    let reused = fails(exe, &args(&home_args, &["remote", "add", "again", "--addr", &addr, "--code", &code]), &[])?;
    anyhow::ensure!(reused.contains("no pairing code is open") || reused.contains("wrong or expired"), "a used code was accepted again:\n{reused}");

    step("pairing through the tunnel stand-in; its child ends with `remote add`");
    let tunnel_env = |control: &Path| -> Vec<(&'static str, String)> {
        vec![
            (canvas_common::remote::DEVTUNNEL_ENV, format!("{}|devtunnel-standin", exe.display())),
            (standin::TARGET_ENV, addr.clone()),
            (standin::CONTROL_ENV, control.display().to_string()),
        ]
    };
    let add_control = root.join("standin-add");
    let code2 = code_of(&ok(exe, &args(&cloud_args, &["remote", "pair", "--name", "test2"]), &[])?)?;
    let added = ok(exe, &args(&home_args, &["remote", "add", "test2", "--tunnel", "fake", "--port", &port.to_string(), "--code", &code2]), &tunnel_env(&add_control))?;
    anyhow::ensure!(added.contains("tunnel fake"), "{added}");
    let add_pid = read_pid(&add_control).context("the stand-in ran for `remote add`")?;
    wait_until("the stand-in of `remote add` to end", Duration::from_secs(5), || !canvas_common::win::alive(add_pid))?;
    let listed = ok(exe, &args(&home_args, &["remote", "servers"]), &[])?;
    anyhow::ensure!(listed.contains("test ") && listed.contains("test2") && listed.contains("tunnel fake, port"), "{listed}");
    let clients = ok(exe, &args(&cloud_args, &["remote", "clients"]), &[])?;
    anyhow::ensure!(clients.matches("active").count() == 2, "two active clients:\n{clients}");
    let secrets = std::fs::read_to_string(canvas_common::remote::Servers::path(&home))?;
    let stored = canvas_common::remote::Servers::load(&home)?;
    let test = stored.find("test").context("test stored")?.clone();
    anyhow::ensure!(!secrets.contains(&crate::paths::hex(&test.secret()?)), "the secret is stored in the clear");
    println!("    {} and {} paired; secrets DPAPI-protected in servers.json", test.client_id, stored.find("test2").map(|s| s.client_id.as_str()).unwrap_or("?"));

    step("wrong secrets and unpaired clients are refused");
    let options = ConnOptions { name: "probe".into(), ..Default::default() };
    let wrong_secret = ConnTarget::Tcp { addr: addr.clone(), remote_id: Some(test.client_id.clone()), secret: crate::protocol::nonce() };
    let error = conn::open(&wrong_secret, &options, &ClientSession::new()).err().context("a wrong secret was accepted")?;
    anyhow::ensure!(error.is::<AuthError>(), "wrong secret: {error:#}");
    let stranger = ConnTarget::Tcp { addr: addr.clone(), remote_id: Some("pc-0000000000000000".into()), secret: crate::protocol::nonce() };
    let error = conn::open(&stranger, &options, &ClientSession::new()).err().context("an unpaired client was accepted")?;
    anyhow::ensure!(error.is::<Refused>() && error.to_string().contains("not paired"), "unpaired client: {error:#}");
    let tokenless = ConnTarget::Tcp { addr: addr.clone(), remote_id: None, secret: crate::protocol::nonce() };
    anyhow::ensure!(conn::open(&tokenless, &options, &ClientSession::new()).err().is_some_and(|e| e.is::<AuthError>()), "a client without credentials was accepted");
    let paired = conn::open(&ConnTarget::parse(Some("test"), &home_instance)?, &options, &ClientSession::new())?;
    anyhow::ensure!(paired.protocol == crate::protocol::PROTOCOL && paired.remote, "paired connection");
    drop(paired);
    let log = std::fs::read_to_string(cloud.log_path()).unwrap_or_default();
    anyhow::ensure!(log.contains(&format!("as paired client {}", test.client_id)), "the service saw the paired client:\n{log}");
    println!("    wrong secret: service failed to prove it (AuthError); unknown id: “{error}”");

    if std::env::var_os("CANVAS_PROBE_CLIPBOARD").is_some() {
        step("an image goes to the service's clipboard (CANVAS_PROBE_CLIPBOARD: replaces your clipboard)");
        image_paste(&home_instance)?;
    }

    step("the canvas through the stand-in (`--remote test2`): a shell, a drop, a killed tunnel child, a remote file");
    let work = root.join("work");
    let viewed = std::env::temp_dir().join(format!("ccv{}.txt", std::process::id()));
    std::fs::write(&viewed, "first line\nREMOTE_VIEWER_OK\tafter a tab\n")?;
    let tui_control = root.join("standin-tui");
    let env_values = tunnel_env(&tui_control);
    let env_paths: Vec<(&str, PathBuf)> = env_values.iter().map(|(k, v)| (*k, PathBuf::from(v))).collect();
    let env: Vec<(&str, &Path)> = env_paths.iter().map(|(k, v)| (*k, v.as_path())).collect();
    let tui_args = args(&home_args, &["--demo", "--cwd", &work.display().to_string(), "--remote", "test2"]);
    let mut ui = Console::start_with_env(exe, &tui_args, &env, 170, 46)?;
    let result = (|| -> Result<()> {
        ui.wait("the canvas", Duration::from_secs(30), |t| t.contains("Build session canvas"))?;
        let log = std::fs::read_to_string(cloud.log_path()).unwrap_or_default();
        anyhow::ensure!(log.contains("connected (tui, protocol 8"), "the service saw a remote TUI:\n{log}");
        let first = read_pid(&tui_control).context("the canvas started the tunnel child")?;
        ui.send(b"\r")?;
        ui.wait("PowerShell prompt", Duration::from_secs(20), |t| t.contains("PS "))?;
        ui.send(b"Write-Output ('BEFORE_' + 'DROP')\r")?;
        ui.wait("output", Duration::from_secs(10), |t| t.contains("BEFORE_DROP"))?;

        std::fs::write(tui_control.join("drop"), "")?;
        ui.wait("offline banner after a drop", Duration::from_secs(10), |t| t.contains("Offline — reconnecting"))?;
        let dropped = Instant::now();
        ui.wait("reconnected", RECONNECT_LIMIT, |t| t.contains("Reconnected") && !t.contains("Offline — reconnecting"))?;
        let after_drop = dropped.elapsed();
        ui.wait("the pane resynced", Duration::from_secs(5), |t| t.contains("BEFORE_DROP"))?;
        ui.send(b"Write-Output ('AFTER_' + 'DROP')\r")?;
        ui.wait("typing after the drop", Duration::from_secs(10), |t| t.contains("AFTER_DROP"))?;
        anyhow::ensure!(read_pid(&tui_control) == Some(first) && canvas_common::win::alive(first), "a dropped connection keeps the tunnel child");

        canvas_common::win::kill_tree(first)?;
        ui.wait("offline banner after the tunnel child died", Duration::from_secs(10), |t| t.contains("Offline — reconnecting"))?;
        let killed = Instant::now();
        ui.wait("reconnected through a new tunnel child", RECONNECT_LIMIT, |t| t.contains("Reconnected") && !t.contains("Offline — reconnecting"))?;
        let after_kill = killed.elapsed();
        let second = read_pid(&tui_control).context("pid")?;
        anyhow::ensure!(second != first && canvas_common::win::alive(second), "the tunnel child was started again");
        ui.send(b"Write-Output ('AFTER_' + 'KILL')\r")?;
        ui.wait("typing after the restart", Duration::from_secs(10), |t| t.contains("AFTER_KILL"))?;
        println!("    reconnected {:.1} s after a drop and {:.1} s after the tunnel child was killed (pid {first} → {second})", after_drop.as_secs_f64(), after_kill.as_secs_f64());

        // A path in the terminal opens in the canvas's viewer (no VS Code tunnel configured).
        ui.send(format!("Write-Output ('probe ' + '{}' + ' end')\r", viewed.display()).as_bytes())?;
        let needle = format!("probe {}", viewed.display());
        ui.wait("path printed", Duration::from_secs(10), |t| t.contains(&needle))?;
        let (lx, ly) = ui.find(&needle).context("printed path")?;
        let (lx, ly) = (lx + 10, ly + 1);
        ui.send(format!("\x1b[<16;{lx};{ly}M\x1b[<16;{lx};{ly}m").as_bytes())?;
        ui.wait("the viewer shows the service's file", Duration::from_secs(10), |t| t.contains("File on the service's machine") && t.contains("REMOTE_VIEWER_OK"))?;
        anyhow::ensure!(ui.text().contains("Copy path") && !ui.text().contains("Open in VS Code"), "viewer buttons without a VS Code tunnel");
        ui.send(b"\x1b")?;
        ui.wait("the viewer closed", Duration::from_secs(5), |t| !t.contains("File on the service's machine"))?;
        println!("    a path in the terminal opened in the viewer (ReadText through the tunnel)");

        let revoked = ok(exe, &args(&cloud_args, &["remote", "revoke", "test2"]), &[])?;
        anyhow::ensure!(revoked.contains("1 live connection(s) closed"), "{revoked}");
        ui.wait("the canvas says it was revoked", Duration::from_secs(15), |t| t.contains("was revoked"))?;
        anyhow::ensure!(!ui.text().contains("Offline — reconnecting"), "a revoked client stops reconnecting\n{}", ui.text());
        println!("    revoked: “{}”", ui.text().lines().find(|l| l.contains("was revoked")).unwrap_or_default().trim());

        ui.send(b"\x1d")?;
        ui.send(b"q")?;
        ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
        ui.send(b"\r")?;
        let code = ui.wait_exit(Duration::from_secs(10))?;
        anyhow::ensure!(code == 0, "the canvas exited with {code}");
        wait_until("the tunnel child to end with the canvas", Duration::from_secs(5), || !canvas_common::win::alive(second))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&viewed);
    if result.is_err() {
        let _ = ui.wait_exit(Duration::from_millis(100));
    }
    result?;

    step("`remote clients` shows the revocation; `remote forget` removes a server");
    let clients = ok(exe, &args(&cloud_args, &["remote", "clients"]), &[])?;
    anyhow::ensure!(clients.contains("revoked") && clients.matches("active").count() == 1, "{clients}");
    let forgot = ok(exe, &args(&home_args, &["remote", "forget", "test"]), &[])?;
    anyhow::ensure!(forgot.contains(&test.client_id), "{forgot}");
    let listed = ok(exe, &args(&home_args, &["remote", "servers"]), &[])?;
    anyhow::ensure!(!listed.lines().any(|l| l.starts_with("test ")), "{listed}");
    println!("PASS");
    Ok(())
}

/// Copies a small image to the service's clipboard over a paired connection.
fn image_paste(home: &Instance) -> Result<()> {
    use crate::client::rpc::Backend;
    let backend = crate::client::remote::RemoteBackend::new();
    let options = ConnOptions { name: "probe".into(), rpc: Some(backend.clone()), ..Default::default() };
    let _conn = conn::open(&ConnTarget::parse(Some("test"), home)?, &options, &ClientSession::new())?;
    let png = canvas_common::clipimage::encode_png(2, 2, &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255])?;
    let backend: &dyn Backend = &*backend;
    backend.set_clipboard_image(png)?;
    anyhow::ensure!(crate::client::clipboard::has_image(), "the clipboard holds no image");
    let back = crate::client::clipboard::get_image_png()?;
    let (w, h, _) = canvas_common::clipimage::decode_png(&back)?;
    anyhow::ensure!((w, h) == (2, 2), "the clipboard image is {w}x{h}");
    println!("    a 2x2 PNG went to the service's clipboard (CF_DIB + PNG) and reads back");
    Ok(())
}
