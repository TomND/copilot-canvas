//! The record a service stopped on purpose leaves in its home (R3-B-01,
//! R3-C-03): `<home>/run/stopped-<instance>` (JSON: [`Stopped`]; clients go
//! by its presence and modification time, `canvas_core::conn::stop_record_path`).
//!
//! A local client's `ClientMsg::Shutdown` (`atelier service stop`, a window's
//! *Stop service*) writes it before the service stops its terminals; a crash,
//! a killed process, a logoff or reboot, the idle exit, and a restart (a
//! client that sends [`RESTART_ACTION`] before its `Shutdown`: `service
//! restart`, `migrate`, a window's *Update service*) never do. The service
//! removes it as it starts, however it is started (a client, `service
//! start`, the login entry, a restart).
//!
//! What a client does with it: while it exists, a local client never starts
//! the service on its own (a reconnect after the connection ended): it says
//! the service was stopped, with a Start action (which removes it, then
//! starts the service), and keeps trying to connect without starting it, so
//! it is back as soon as something else starts the service. What the service
//! ran before the stop is still resumed by the next start (the restore keeps
//! the running set whatever ended the service).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::Instance;

/// What a client sends (`ClientMsg::Bus { json }`) just before `Shutdown`
/// when it restarts the service: the stop is not recorded as one on purpose.
/// An older service ignores it (unknown bus actions are dropped).
pub const RESTART_ACTION: &str = r#"{"action":"service","op":"restarting"}"#;

/// A service stopped on purpose.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Stopped {
    /// The service process that stopped.
    pub pid: u32,
    /// When it was asked to stop (Unix ms).
    pub stopped_ms: u64,
    /// Who asked: the client's name and process, as the service knew them.
    #[serde(default)]
    pub by: String,
    /// The build of the service that stopped.
    #[serde(default)]
    pub build: String,
}

/// `<home>/run/stopped-<instance>`.
pub fn path(instance: &Instance) -> PathBuf {
    instance.home.join("run").join(format!("stopped-{}", instance.name))
}

/// The record, while there is one (an unreadable file counts as one, with nothing known).
pub fn read(instance: &Instance) -> Option<Stopped> {
    let text = std::fs::read_to_string(path(instance)).ok()?;
    Some(serde_json::from_str(&text).unwrap_or_default())
}

/// Writes the record (whole: a temporary file renamed over it).
pub fn write(instance: &Instance, stopped: &Stopped) -> std::io::Result<()> {
    let path = path(instance);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_file_name(format!("stopped-{}.{}.tmp", instance.name, std::process::id()));
    std::fs::write(&partial, serde_json::to_string(stopped).unwrap_or_default())?;
    std::fs::rename(&partial, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })
}

/// Removes the record; answers the one there was.
pub fn clear(instance: &Instance) -> Option<Stopped> {
    let stopped = read(instance)?;
    let _ = std::fs::remove_file(path(instance));
    Some(stopped)
}

/// Whether `json` (a `ClientMsg::Bus` body) is [`RESTART_ACTION`].
pub fn is_restart_action(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json).is_ok_and(|v| v["action"] == "service" && v["op"] == "restarting")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_is_written_read_and_cleared() {
        let home = std::env::temp_dir().join(format!("cc-stopped-{}-{}", std::process::id(), crate::paths::now_ms()));
        let instance = Instance::new(Some("t".into()), Some(home.clone()), Some(home.clone())).unwrap();
        assert_eq!(read(&instance), None);
        let stopped = Stopped { pid: 7, stopped_ms: 1_000, by: "atelier (pid 9)".into(), build: "0.3.0-x".into() };
        write(&instance, &stopped).unwrap();
        assert_eq!(path(&instance), home.join("run").join("stopped-t"));
        assert_eq!(read(&instance), Some(stopped.clone()));
        // Older or newer fields are tolerated: a file with the pid and time only reads.
        std::fs::write(path(&instance), r#"{"pid":7,"stopped_ms":1000,"later":true}"#).unwrap();
        assert_eq!(read(&instance).map(|s| s.pid), Some(7));
        assert_eq!(clear(&instance).map(|s| s.stopped_ms), Some(1_000));
        assert_eq!(read(&instance), None);
        assert!(is_restart_action(RESTART_ACTION) && !is_restart_action(r#"{"action":"restore","op":"cancel"}"#));
        let _ = std::fs::remove_dir_all(home);
    }
}
