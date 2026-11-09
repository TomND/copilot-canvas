//! `atelier service install|uninstall`: start the service at login
//! (MAC-LINUX-PLAN D5). Linux: a systemd user unit. Windows: the
//! `Atelier-Service` logon task from an automation manifest (README).
//! macOS: a LaunchAgent that runs `service start` at login.

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
