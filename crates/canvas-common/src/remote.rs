//! Remote mode on the client's side: the services this machine paired with
//! (`<home>/remote/servers.json`, secrets DPAPI-protected), where the Dev
//! Tunnels CLI is, and the names the tunnel setup uses.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The service's remote port when `remote.listen` is not set.
pub const DEFAULT_PORT: u16 = 47600;
/// The persistent Dev Tunnel `remote tunnel-setup` suggests.
pub const DEFAULT_TUNNEL: &str = "copilot-canvas";
/// The automation that hosts the tunnel on the service's machine.
pub const TUNNEL_TASK: &str = "CopilotCanvas-Tunnel";
/// Overrides the Dev Tunnels CLI: a program, optionally followed by `|` and
/// arguments that go before the command's own (`C:\x\copilot-canvas.exe|devtunnel-standin`).
pub const DEVTUNNEL_ENV: &str = "COPILOT_CANVAS_DEVTUNNEL";

/// `<home>/remote`: paired servers (client side) and paired clients (service side).
pub fn dir(home: &Path) -> PathBuf {
    home.join("remote")
}

/// Names of servers and clients: 1–40 characters of `[A-Za-z0-9_.-]`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// This machine's name (what a service lists for a client it paired).
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "client".into())
}

/// A service this machine paired with (`remote add`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ServerRecord {
    /// What `--remote NAME` calls it.
    pub name: String,
    /// Dev Tunnel id (`devtunnel connect TUNNEL`); `None`: a direct address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// The tunnel's port: the service's `remote.listen` port.
    #[serde(default)]
    pub port: u16,
    /// `HOST:PORT` of a direct connection (loopback tests, an SSH forward).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// The VS Code tunnel on the service's machine (`code tunnel --name …`):
    /// its files open there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vscode_tunnel: Option<String>,
    /// The id the service gave this client.
    pub client_id: String,
    /// The client secret, DPAPI-protected (hex).
    pub secret: String,
    pub added_ms: u64,
}

impl ServerRecord {
    /// The client secret (only this Windows user can decrypt it).
    pub fn secret(&self) -> Result<Vec<u8>> {
        crate::dpapi::unprotect_hex(&self.secret).with_context(|| format!("cannot decrypt the secret of {:?} (paired by another Windows user?)", self.name))
    }

    /// `tunnel copilot-canvas, port 47600` or the direct address.
    pub fn describe(&self) -> String {
        match (&self.tunnel, &self.addr) {
            (Some(tunnel), _) => format!("tunnel {tunnel}, port {}", self.port),
            (None, Some(addr)) => addr.clone(),
            (None, None) => "no address".into(),
        }
    }
}

/// `<home>/remote/servers.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Servers {
    #[serde(default)]
    pub servers: Vec<ServerRecord>,
}

impl Servers {
    pub fn path(home: &Path) -> PathBuf {
        dir(home).join("servers.json")
    }

    /// A missing file is an empty list.
    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} is not readable", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let path = Self::path(home);
        crate::paths::write_atomic(&path, serde_json::to_string_pretty(self)?.as_bytes()).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn find(&self, name: &str) -> Option<&ServerRecord> {
        self.servers.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// Adds `record`, replacing a server of the same name.
    pub fn upsert(&mut self, record: ServerRecord) {
        self.servers.retain(|s| !s.name.eq_ignore_ascii_case(&record.name));
        self.servers.push(record);
    }

    pub fn remove(&mut self, name: &str) -> Option<ServerRecord> {
        let index = self.servers.iter().position(|s| s.name.eq_ignore_ascii_case(name))?;
        Some(self.servers.remove(index))
    }
}

/// How to run the Dev Tunnels CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct DevTunnelCli {
    pub program: PathBuf,
    /// Arguments before the command's own (the stand-in's subcommand).
    pub prefix: Vec<String>,
}

/// The Dev Tunnels CLI: `COPILOT_CANVAS_DEVTUNNEL`, else [`find_devtunnel_exe`].
pub fn devtunnel() -> Option<DevTunnelCli> {
    if let Some(value) = std::env::var(DEVTUNNEL_ENV).ok().filter(|v| !v.trim().is_empty()) {
        let mut parts = value.split('|').map(str::trim);
        let program = PathBuf::from(parts.next().unwrap_or_default());
        return Some(DevTunnelCli { program, prefix: parts.filter(|p| !p.is_empty()).map(str::to_owned).collect() });
    }
    find_devtunnel_exe().map(|program| DevTunnelCli { program, prefix: Vec::new() })
}

/// `devtunnel.exe` on PATH, among WinGet's links, or in WinGet's package folder.
pub fn find_devtunnel_exe() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path).map(|d| d.join("devtunnel.exe")).find(|p| p.is_file())
    {
        return Some(found);
    }
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let winget = local.join("Microsoft").join("WinGet");
    let link = winget.join("Links").join("devtunnel.exe");
    if link.is_file() {
        return Some(link);
    }
    std::fs::read_dir(winget.join("Packages"))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("microsoft.devtunnel"))
        .map(|e| e.path().join("devtunnel.exe"))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_persist_with_protected_secrets() {
        let home = std::env::temp_dir().join(format!("cc-servers-{}", uuid::Uuid::new_v4()));
        assert_eq!(Servers::load(&home).unwrap(), Servers::default(), "no file yet");
        let secret = crate::pairing::new_secret();
        let record = ServerRecord {
            name: "CloudPC".into(),
            tunnel: Some("copilot-canvas".into()),
            port: 47600,
            client_id: "pc-1".into(),
            secret: crate::dpapi::protect_hex(&secret, "test").unwrap(),
            ..Default::default()
        };
        let mut servers = Servers::default();
        servers.upsert(record.clone());
        servers.upsert(ServerRecord { name: "direct".into(), tunnel: None, addr: Some("127.0.0.1:1".into()), ..record.clone() });
        servers.save(&home).unwrap();
        let text = std::fs::read_to_string(Servers::path(&home)).unwrap();
        assert!(!text.contains(&crate::paths::hex(&secret)), "the secret is not stored in the clear");
        let loaded = Servers::load(&home).unwrap();
        assert_eq!(loaded, servers);
        let found = loaded.find("cloudpc").unwrap();
        assert_eq!(found.secret().unwrap(), secret);
        assert_eq!(found.describe(), "tunnel copilot-canvas, port 47600");
        let mut changed = loaded.clone();
        changed.upsert(ServerRecord { client_id: "pc-2".into(), ..record });
        assert_eq!(changed.servers.len(), 2, "same name replaces");
        assert_eq!(changed.remove("DIRECT").unwrap().describe(), "127.0.0.1:1");
        assert!(changed.find("direct").is_none());
        std::fs::write(Servers::path(&home), "{broken").unwrap();
        assert!(Servers::load(&home).is_err(), "a broken file is an error, not an empty list");
        std::fs::remove_dir_all(home).unwrap();
        assert!(valid_name("cloud-pc_1.x") && !valid_name("a b") && !valid_name("") && !valid_name("host:1"));
    }

    #[test]
    fn the_devtunnel_override_takes_a_program_and_prefix_arguments() {
        // Only this test sets the variable.
        unsafe { std::env::set_var(DEVTUNNEL_ENV, r"C:\x\copilot-canvas.exe|devtunnel-standin") };
        let cli = devtunnel().unwrap();
        unsafe { std::env::remove_var(DEVTUNNEL_ENV) };
        assert_eq!(cli, DevTunnelCli { program: PathBuf::from(r"C:\x\copilot-canvas.exe"), prefix: vec!["devtunnel-standin".into()] });
    }
}
