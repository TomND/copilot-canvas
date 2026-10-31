//! `copilot-canvas service install|uninstall`: start the service at login
//! (MAC-LINUX-PLAN D5). Linux: a systemd user unit. Windows: the
//! `CopilotCanvas-Service` logon task from an automation manifest (README).
//! macOS: a LaunchAgent (with the launchd backend).

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
        Platform::Windows => bail!("on Windows the service starts at logon from a scheduled task: see the CopilotCanvas-Service manifest example in README (copilot-canvas automations apply)"),
        Platform::MacOs => bail!("service install on macOS comes with the launchd backend"),
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
        Platform::Windows => bail!("on Windows remove the CopilotCanvas-Service task: copilot-canvas automations remove CopilotCanvas-Service"),
        Platform::MacOs => bail!("service uninstall on macOS comes with the launchd backend"),
    }
}
