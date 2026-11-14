//! `atelier service install|uninstall`: start the service at login
//! (MAC-LINUX-PLAN D5). Linux: a systemd user unit. Windows: the
//! `Atelier-Service` logon task from an automation manifest (README).
//! macOS: a LaunchAgent that runs `service start` at login.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use canvas_sys::host::Platform;

use crate::paths::Instance;

pub fn install(instance: &Instance) -> Result<()> {
    match canvas_sys::host::platform() {
        Platform::Linux => {
            let exe = canvas_agents::scheduler::common::launcher_exe();
            let path = canvas_agents::scheduler::systemd::service_unit::install(&exe, &instance.name, &instance.home, &instance.sessions_dir)?;
            println!("Installed {} (enabled: the service starts at login).", path.display());
            println!("Start it now: systemctl --user start {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            println!("On a server without a login session: loginctl enable-linger $USER");
            Ok(())
        }
        Platform::Windows => bail!("on Windows the service starts at logon from a scheduled task: see the Atelier-Service manifest example in README (atelier automations apply)"),
        Platform::MacOs => {
            let exe = canvas_agents::scheduler::common::launcher_exe();
            let path = canvas_agents::scheduler::launchd::service_agent::install(&exe, &instance.name, &instance.home, &instance.sessions_dir)?;
            println!("Installed {} (loaded: the service starts at login, and now unless it runs).", path.display());
            Ok(())
        }
    }
}

pub fn uninstall(instance: &Instance) -> Result<()> {
    match canvas_sys::host::platform() {
        Platform::Linux => {
            match canvas_agents::scheduler::systemd::service_unit::uninstall(&instance.name)? {
                Some(path) => println!("Removed {} (a running service keeps running until it stops).", path.display()),
                None => println!("No login unit was installed for instance {}.", instance.name),
            }
            Ok(())
        }
        Platform::Windows => bail!("on Windows remove the Atelier-Service task: atelier automations remove Atelier-Service"),
        Platform::MacOs => {
            match canvas_agents::scheduler::launchd::service_agent::uninstall(&instance.name)? {
                Some(path) => println!("Removed {} (a running service keeps running until it stops).", path.display()),
                None => println!("No login agent was installed for instance {}.", instance.name),
            }
            Ok(())
        }
    }
}

/// One `Stats` round trip on `conn`, with the terminals the service listed
/// meanwhile (it lists them all right after connecting).
pub(crate) fn stats_and_terminals(conn: &crate::client::conn::Conn) -> Result<(crate::protocol::ServiceStats, Vec<crate::protocol::TermInfo>)> {
    use crate::protocol::{ClientMsg, ServiceMsg};
    conn.tx.send(ClientMsg::Stats)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut terms: Vec<crate::protocol::TermInfo> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match conn.rx.recv_timeout(left) {
            Ok(ServiceMsg::Stats(stats)) => return Ok((stats, terms)),
            Ok(ServiceMsg::Terminals(infos)) => {
                for info in infos {
                    terms.retain(|t| t.id != info.id);
                    terms.push(info);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    bail!("the service did not answer")
}

/// What looks wedged in the running service (README, *Safety*): see
/// `canvas_host::server::wedge_signs`.
pub fn wedge_signs(instance: &Instance, terms: &[crate::protocol::TermInfo]) -> Vec<String> {
    canvas_host::server::wedge_signs(canvas_host::server::read_health(instance).as_ref(), terms, crate::paths::now_ms())
}

/// `atelier service status`: running, its build and numbers, and `wedged`:
/// the signs that it is stuck (empty when none).
pub fn status(instance: &Instance) -> Result<()> {
    let Ok(conn) = crate::client::conn::connect(instance, false) else {
        println!("{{\"running\": false}}");
        return Ok(());
    };
    let (stats, terms) = stats_and_terminals(&conn)?;
    let signs = wedge_signs(instance, &terms);
    let report = serde_json::json!({
        "running": true,
        "build": conn.service_build,
        "pid": stats.pid,
        "terminals": stats.terminals,
        "alive": stats.alive,
        "private_mb": stats.private_bytes / (1024 * 1024),
        "working_set_mb": stats.working_set_bytes / (1024 * 1024),
        "cpu_ms": stats.cpu_ms,
        "uptime_s": stats.uptime_ms / 1000,
        "wedged": signs,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !signs.is_empty() {
        eprintln!("The service looks wedged; `atelier service restart --yes` stops it gracefully and starts it again (conversations stay resumable).");
    }
    Ok(())
}

/// The scheduled task that starts the service at logon on Windows (README).
const LOGON_TASK: &str = "Atelier-Service";

/// `atelier service restart [--yes]`: stops the service gracefully (each
/// session gets its grace period; `--yes` is needed while sessions run),
/// waits for its process to end, then starts it through the entry it starts
/// by at login (Windows: the `Atelier-Service` task; macOS: its LaunchAgent,
/// `launchctl kickstart`; Linux: its systemd user unit), else as `service
/// start` does, and waits until it answers.
pub fn restart(instance: &Instance, yes: bool) -> Result<()> {
    match crate::client::conn::connect(instance, false) {
        Ok(conn) => {
            let (stats, _) = stats_and_terminals(&conn)?;
            if stats.alive > 0 && !yes {
                bail!(
                    "the service runs {} session(s); a restart stops them gracefully (their conversations stay resumable): run `atelier service restart --yes`",
                    stats.alive
                );
            }
            // A restart, not a stop on purpose: open windows may start it again (R3-B-01).
            conn.tx.send(crate::protocol::ClientMsg::Bus { json: canvas_common::stopped::RESTART_ACTION.into() })?;
            conn.tx.send(crate::protocol::ClientMsg::Shutdown)?;
            drop(conn);
            println!("Stopping the service (pid {}); sessions get up to {} s to exit gracefully…", stats.pid, canvas_host::term::STOP_GRACE.as_secs());
            let deadline = Instant::now() + canvas_host::term::STOP_GRACE + Duration::from_secs(15);
            while canvas_sys::process::alive(stats.pid) {
                anyhow::ensure!(Instant::now() < deadline, "the service (pid {}) did not exit; end that process, then run `atelier service start`", stats.pid);
                std::thread::sleep(Duration::from_millis(200));
            }
            println!("Stopped.");
        }
        Err(_) => println!("No service was running."),
    }
    let how = start_like_login(instance)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(conn) = crate::client::conn::connect(instance, false)
            && let Ok((stats, _)) = stats_and_terminals(&conn)
        {
            println!("Service started {how} (pid {}, build {}).", stats.pid, conn.service_build);
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the service did not start within 30 s ({how}); see {}", instance.log_path().display());
    }
}

/// Starts the service the way it starts at login, when that is installed;
/// answers how.
fn start_like_login(instance: &Instance) -> Result<String> {
    match canvas_sys::host::platform() {
        Platform::Windows => {
            let scheduler = canvas_agents::scheduler::get();
            if instance.name == "default" && scheduler.task(LOGON_TASK).ok().flatten().is_some() {
                scheduler.run(LOGON_TASK)?;
                return Ok(format!("through the {LOGON_TASK} logon task"));
            }
        }
        Platform::MacOs => {
            if canvas_agents::scheduler::launchd::service_agent::kickstart(&instance.name)? {
                return Ok(format!("through its login agent ({})", canvas_agents::scheduler::launchd::service_agent::label(&instance.name)));
            }
        }
        Platform::Linux => {
            if canvas_agents::scheduler::systemd::service_unit::restart(&instance.name)? {
                return Ok(format!("through its systemd unit ({})", canvas_agents::scheduler::systemd::service_unit::file_name(&instance.name)));
            }
        }
    }
    crate::client::conn::spawn_service(instance)?;
    Ok("as `service start` does (no login entry is installed)".to_owned())
}
