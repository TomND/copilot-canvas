//! Machine-level settings in `<home>/settings.json` (separate from the canvas
//! document): the terminal engine, remote access, the front end and the
//! default agent. Environment variables override them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub terminal: TerminalSettings,
    #[serde(default)]
    pub remote: RemoteSettings,
    /// Front end the `atelier` command opens: "tui" or "gui".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Agent CLI for new sessions: "copilot", "opencode" or "claude" (unset: auto-detect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Canvas effects on this machine: "on", "reduced" (static badges) or
    /// "off" (unset: reduced when Windows' animations are off, else on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects: Option<String>,
    /// *Settings › Working glow*: the dots around working sessions glow
    /// (unset: on). Named after the dot grid pulse it replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_pulse: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TerminalSettings {
    /// "ghostty" or "alacritty" (unset: the build's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RemoteSettings {
    /// Loopback address the service also listens on for remote clients
    /// (behind a Dev Tunnel), e.g. "127.0.0.1:47600". Off when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Compress remote frames with zstd.
    #[serde(default = "yes")]
    pub compress: bool,
    /// The Dev Tunnel that hosts the remote port (`remote tunnel-setup`);
    /// shown in pairing instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// The remote port's Dev Tunnels web-forwarding address for the iPhone
    /// (`wss://atelier-47600.usw2.devtunnels.ms`); found with
    /// `devtunnel show` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self { listen: None, compress: true, tunnel: None, web_url: None }
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join("settings.json")
}

impl Settings {
    /// Missing or unreadable settings are the defaults.
    pub fn load(home: &Path) -> Self {
        std::fs::read_to_string(path(home)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    /// Settings to change and save: a missing file is the defaults, a file
    /// that cannot be read or parsed is an error (it is never overwritten).
    pub fn load_checked(home: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        let file = path(home);
        match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid settings JSON; fix or remove it first", file.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", file.display())),
        }
    }

    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let target = path(home);
        let temp = target.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(&temp, &target)
    }

    /// The terminal engine name: `ATELIER_ENGINE`, else the setting.
    pub fn engine(&self) -> Option<String> {
        crate::env::var("ATELIER_ENGINE").ok().filter(|v| !v.trim().is_empty()).or_else(|| self.terminal.engine.clone())
    }

    /// The front end: `ATELIER_CLIENT`, else the setting.
    pub fn client(&self) -> Option<String> {
        crate::env::var("ATELIER_CLIENT").ok().filter(|v| !v.trim().is_empty()).or_else(|| self.client.clone())
    }

    /// Canvas effects: `ATELIER_EFFECTS`, else the setting ("on",
    /// "reduced", "off"; `None`: follow Windows' animation setting).
    pub fn effects(&self) -> Option<String> {
        let value = crate::env::var("ATELIER_EFFECTS").ok().filter(|v| !v.trim().is_empty()).or_else(|| self.effects.clone())?;
        let value = value.trim().to_ascii_lowercase();
        matches!(value.as_str(), "on" | "reduced" | "off").then_some(value)
    }

    /// The working glow around working sessions: `ATELIER_GRID_PULSE`
    /// ("on"/"off"), else the setting, else on.
    pub fn grid_pulse(&self) -> bool {
        let env = crate::env::var("ATELIER_GRID_PULSE").ok().map(|v| v.trim().to_ascii_lowercase());
        match env.as_deref() {
            Some("on" | "1" | "true" | "yes") => true,
            Some("off" | "0" | "false" | "no") => false,
            _ => self.grid_pulse.unwrap_or(true),
        }
    }

    /// Address the service listens on for remote clients:
    /// `ATELIER_LISTEN`, else `remote.listen`.
    pub fn listen(&self) -> Option<String> {
        crate::env::var("ATELIER_LISTEN").ok().filter(|v| !v.trim().is_empty()).or_else(|| self.remote.listen.clone())
    }

    /// Compress remote frames: `ATELIER_COMPRESS` (0/off/false), else `remote.compress`.
    pub fn compress(&self) -> bool {
        match crate::env::var("ATELIER_COMPRESS") {
            Ok(value) if !value.trim().is_empty() => !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
            _ => self.remote.compress,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_missing_or_partial_files() {
        let home = std::env::temp_dir().join(format!("cc-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(Settings::load(&home), Settings::default());
        std::fs::write(path(&home), r#"{"terminal":{"engine":"alacritty"}}"#).unwrap();
        let loaded = Settings::load(&home);
        assert_eq!(loaded.terminal.engine.as_deref(), Some("alacritty"));
        assert!(loaded.remote.compress, "compression defaults on");
        let mut changed = loaded.clone();
        changed.remote.listen = Some("127.0.0.1:47600".into());
        changed.save(&home).unwrap();
        assert_eq!(Settings::load(&home), changed);
        assert_eq!(Settings::load_checked(&home).unwrap(), changed);
        std::fs::write(path(&home), "{ \"remote\": { \"listen\": ").unwrap();
        assert_eq!(Settings::load(&home), Settings::default(), "readers fall back to the defaults");
        assert!(Settings::load_checked(&home).unwrap_err().to_string().contains("not valid settings JSON"), "writers stop");
        std::fs::remove_dir_all(home).unwrap();
        let missing = std::env::temp_dir().join(format!("cc-settings-{}", uuid::Uuid::new_v4()));
        assert_eq!(Settings::load_checked(&missing).unwrap(), Settings::default());
    }
}
