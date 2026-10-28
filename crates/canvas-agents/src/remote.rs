//! Remote clients on the service's machine (PLAN.md D5 "App-level
//! authentication"): one-time pairing codes, the paired clients in
//! `<home>/remote/clients.json` (secrets DPAPI-protected), revocation, and
//! the `CopilotCanvas-Tunnel` automation that hosts the Dev Tunnel.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use canvas_common::handshake::{PairGrant, PairRequest};
use canvas_common::{dpapi, pairing, remote};
use serde::{Deserialize, Serialize};

/// Codes open at the same time (a new one replaces the oldest).
const MAX_OPEN_CODES: usize = 3;
/// A wrong code is answered this late (slows guessing down further).
const FAILURE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// A paired client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ClientRecord {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_seen_ms: u64,
    /// When it was revoked (0: it is not); the secret is erased then.
    #[serde(default)]
    pub revoked_ms: u64,
    /// The client secret, DPAPI-protected (hex).
    #[serde(default)]
    pub secret: String,
}

/// `<home>/remote/clients.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClientsFile {
    #[serde(default)]
    pub clients: Vec<ClientRecord>,
}

impl ClientsFile {
    pub fn path(home: &Path) -> PathBuf {
        remote::dir(home).join("clients.json")
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
        canvas_common::paths::write_atomic(&path, serde_json::to_string_pretty(self)?.as_bytes()).with_context(|| format!("cannot write {}", path.display()))
    }

    /// Clients that may connect.
    pub fn active(&self) -> impl Iterator<Item = &ClientRecord> {
        self.clients.iter().filter(|c| c.revoked_ms == 0)
    }
}

struct Pending {
    code: String,
    name: Option<String>,
    expires_ms: u64,
}

#[derive(Default)]
struct State {
    codes: Vec<Pending>,
    failures: u32,
}

/// Pairing codes and paired clients of one service.
pub struct Registry {
    home: PathBuf,
    state: Mutex<State>,
}

/// A client name for the list: printable, at most 40 characters.
fn clean_name(name: &str) -> String {
    let name: String = name.trim().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).take(40).collect();
    if name.is_empty() { "client".into() } else { name }
}

impl Registry {
    pub fn new(home: &Path) -> Self {
        Self { home: home.to_path_buf(), state: Mutex::new(State::default()) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens a one-time code (canonical form) for a client to be called
    /// `name`; returns it with its expiry. Opening a code also clears the
    /// failed-attempt count.
    pub fn open_code(&self, name: Option<String>, now: u64) -> Result<(String, u64)> {
        if let Some(name) = &name {
            anyhow::ensure!(remote::valid_name(name), "a client name is 1-40 characters of letters, digits, '-', '_' and '.'");
        }
        let mut state = self.state();
        state.codes.retain(|c| c.expires_ms > now);
        if state.codes.len() >= MAX_OPEN_CODES {
            state.codes.remove(0);
        }
        let code = pairing::new_code();
        let expires_ms = now + pairing::CODE_TTL_MS;
        state.codes.push(Pending { code: code.clone(), name, expires_ms });
        state.failures = 0;
        Ok((code, expires_ms))
    }

    /// Codes open now.
    pub fn open_codes(&self, now: u64) -> usize {
        self.state().codes.iter().filter(|c| c.expires_ms > now).count()
    }

    /// Checks a pairing request against the open codes (each compared in
    /// constant time). A match is used up and creates the client; a miss
    /// counts, and after `MAX_ATTEMPTS` misses every open code is cancelled.
    pub fn pair(&self, request: &PairRequest, now: u64) -> Result<PairGrant, String> {
        let outcome = {
            let mut state = self.state();
            state.codes.retain(|c| c.expires_ms > now);
            if state.codes.is_empty() {
                return Err("no pairing code is open on the service: run `copilot-canvas remote pair` there first".into());
            }
            let mut matched = None;
            for (index, pending) in state.codes.iter().enumerate() {
                if pairing::check_code_proof(&pending.code, request.nonce, request.code_proof) {
                    matched = Some(index);
                }
            }
            match matched {
                Some(index) => Ok(state.codes.remove(index)),
                None => {
                    state.failures += 1;
                    if state.failures >= pairing::MAX_ATTEMPTS {
                        state.codes.clear();
                        state.failures = 0;
                        Err("too many wrong pairing codes: every open code was cancelled (run `copilot-canvas remote pair` again)".to_owned())
                    } else {
                        Err("wrong or expired pairing code".to_owned())
                    }
                }
            }
        };
        let pending = match outcome {
            Ok(pending) => pending,
            Err(reason) => {
                std::thread::sleep(FAILURE_DELAY);
                return Err(reason);
            }
        };
        let name = pending.name.clone().unwrap_or_else(|| clean_name(request.name));
        self.add_client(&name, now).map(|(client_id, secret)| PairGrant { client_id, code: pending.code, secret }).map_err(|e| format!("the service could not store the client: {e:#}"))
    }

    /// Stores a new client (its name made unique among active clients).
    fn add_client(&self, name: &str, now: u64) -> Result<(String, Vec<u8>)> {
        let _guard = self.state();
        let mut file = ClientsFile::load(&self.home)?;
        let mut unique = name.to_owned();
        let mut n = 2;
        while file.active().any(|c| c.name.eq_ignore_ascii_case(&unique)) {
            unique = format!("{name}-{n}");
            n += 1;
        }
        let id = pairing::new_client_id();
        let secret = pairing::new_secret();
        let sealed = dpapi::protect_hex(&secret, "Copilot Canvas remote client").context("cannot protect the client secret")?;
        file.clients.push(ClientRecord { id: id.clone(), name: unique, created_ms: now, last_seen_ms: 0, revoked_ms: 0, secret: sealed });
        file.save(&self.home)?;
        Ok((id, secret))
    }

    /// The secret of an active client (and notes that it was seen).
    pub fn secret(&self, id: &str, now: u64) -> Option<Vec<u8>> {
        let _guard = self.state();
        let mut file = ClientsFile::load(&self.home).ok()?;
        let client = file.clients.iter_mut().find(|c| c.id == id && c.revoked_ms == 0)?;
        let secret = dpapi::unprotect_hex(&client.secret).ok()?;
        client.last_seen_ms = now;
        let _ = file.save(&self.home);
        Some(secret)
    }

    /// Why `id` is refused (it has no secret).
    pub fn refusal(&self, id: &str) -> String {
        let machine = remote::machine_name();
        match ClientsFile::load(&self.home).ok().and_then(|f| f.clients.into_iter().find(|c| c.id == id)) {
            Some(client) if client.revoked_ms > 0 => format!("this client ({}) was revoked on {machine}; pair it again (`copilot-canvas remote pair` there)", client.name),
            Some(client) => format!("the secret of this client ({}) cannot be read on {machine}; pair it again", client.name),
            None => format!("this client is not paired with {machine} (unknown client id {id}); pair it (`copilot-canvas remote pair` there)"),
        }
    }

    pub fn list(&self) -> Result<Vec<ClientRecord>> {
        let _guard = self.state();
        Ok(ClientsFile::load(&self.home)?.clients)
    }

    /// Revokes the active client with this id or name and erases its secret.
    pub fn revoke(&self, key: &str, now: u64) -> Result<ClientRecord> {
        let _guard = self.state();
        let mut file = ClientsFile::load(&self.home)?;
        let matches: Vec<usize> =
            file.clients.iter().enumerate().filter(|(_, c)| c.revoked_ms == 0 && (c.id == key || c.name.eq_ignore_ascii_case(key))).map(|(i, _)| i).collect();
        let index = match matches.as_slice() {
            [index] => *index,
            [] => bail!("no paired client {key:?} (see `copilot-canvas remote clients`)"),
            _ => bail!("several clients are called {key:?}; revoke one by its id"),
        };
        let client = &mut file.clients[index];
        client.revoked_ms = now;
        client.secret.clear();
        let revoked = client.clone();
        file.save(&self.home)?;
        Ok(revoked)
    }
}

/// The `CopilotCanvas-Tunnel` manifest: hosts `tunnel` (port `port`) at
/// logon and every hour (an hourly start does nothing while it runs), and
/// restarts it when it fails.
pub fn tunnel_manifest(devtunnel: &Path, tunnel: &str, port: u16) -> String {
    let q = |text: &str| format!("'{}'", text.replace('\'', "''"));
    format!(
        "# Hosts the private Dev Tunnel to the Copilot Canvas service (remote mode).\n\
         # Written by `copilot-canvas remote tunnel-setup`; register it with\n\
         # `copilot-canvas automations apply {task} --yes`.\n\
         name: {name}\n\
         family: 'Copilot Canvas'\n\
         description: 'Dev Tunnel host for remote canvases (devtunnel host {tunnel})'\n\
         enabled: true\n\
         ai: false\n\
         command: {command}\n\
         args: {args}\n\
         schedule:\n  - 'logon'\n  - 'every 1h'\n\
         time_limit: '0'\n\
         overlap: ignore\n\
         restart_on_failure: '10 every 1m'\n\
         start_when_available: true\n\
         report: ''\n\
         docs: []\n\
         logs: []\n\
         outputs: []\n",
        task = remote::TUNNEL_TASK,
        name = q(remote::TUNNEL_TASK),
        command = q(&devtunnel.display().to_string()),
        args = q(&format!("host {tunnel} --port-numbers {port}")),
    )
}

/// Where `remote tunnel-setup` writes the manifest in an automations folder.
pub fn tunnel_manifest_path(dir: &Path) -> PathBuf {
    dir.join("copilot-canvas").join(format!("{}.yaml", remote::TUNNEL_TASK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_common::protocol;

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-remote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(code: &str, nonce: &[u8]) -> Vec<u8> {
        pairing::code_proof(code, nonce)
    }

    fn try_pair(registry: &Registry, code: &str, now: u64) -> Result<PairGrant, String> {
        let nonce = protocol::nonce();
        let proof = request(code, &nonce);
        registry.pair(&PairRequest { code_proof: &proof, name: "HOME PC!", nonce: &nonce }, now)
    }

    #[test]
    fn codes_are_single_use_and_expire() {
        let home = home();
        let registry = Registry::new(&home);
        assert!(try_pair(&registry, "ABCDEFGH", 1_000).unwrap_err().contains("no pairing code is open"));
        let (code, expires) = registry.open_code(Some("home".into()), 1_000).unwrap();
        assert_eq!(expires, 1_000 + pairing::CODE_TTL_MS);
        let grant = try_pair(&registry, &code, 2_000).unwrap();
        assert_eq!(grant.code, code);
        assert_eq!(registry.secret(&grant.client_id, 3_000), Some(grant.secret.clone()));
        assert!(try_pair(&registry, &code, 2_500).unwrap_err().contains("no pairing code is open"), "used up");
        let (late, _) = registry.open_code(None, 10_000).unwrap();
        assert!(try_pair(&registry, &late, 10_000 + pairing::CODE_TTL_MS).is_err(), "expired");
        let (named_by_client, _) = registry.open_code(None, 20_000).unwrap();
        let grant = try_pair(&registry, &named_by_client, 20_001).unwrap();
        let list = registry.list().unwrap();
        assert_eq!(list.iter().find(|c| c.id == grant.client_id).unwrap().name, "HOME-PC-", "the client's own name, cleaned");
        assert_eq!(list[0].last_seen_ms, 3_000, "seen when it authenticated");
        assert!(registry.open_code(Some("bad name".into()), 0).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn wrong_codes_are_limited_and_cancel_every_open_code() {
        let home = home();
        let registry = Registry::new(&home);
        let (code, _) = registry.open_code(None, 0).unwrap();
        for _ in 1..pairing::MAX_ATTEMPTS {
            assert_eq!(try_pair(&registry, "ABCDEFGH", 1).unwrap_err(), "wrong or expired pairing code");
        }
        assert_eq!(registry.open_codes(1), 1, "still open after {} misses", pairing::MAX_ATTEMPTS - 1);
        assert!(try_pair(&registry, "ABCDEFGH", 1).unwrap_err().contains("too many wrong pairing codes"));
        assert_eq!(registry.open_codes(1), 0);
        assert!(try_pair(&registry, &code, 1).is_err(), "the right code no longer works either");
        // At most a few codes at once.
        for _ in 0..5 {
            registry.open_code(None, 2).unwrap();
        }
        assert_eq!(registry.open_codes(2), MAX_OPEN_CODES);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn clients_persist_protected_and_revocation_refuses_them() {
        let home = home();
        let registry = Registry::new(&home);
        let (code, _) = registry.open_code(Some("laptop".into()), 0).unwrap();
        let first = try_pair(&registry, &code, 1).unwrap();
        let (code, _) = registry.open_code(Some("laptop".into()), 2).unwrap();
        let second = try_pair(&registry, &code, 3).unwrap();
        let text = std::fs::read_to_string(ClientsFile::path(&home)).unwrap();
        assert!(!text.contains(&canvas_common::paths::hex(&first.secret)), "secrets are DPAPI-protected on disk");
        let names: Vec<String> = registry.list().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["laptop", "laptop-2"], "names stay unique");
        // A second registry (a restarted service) reads the same file.
        let restarted = Registry::new(&home);
        assert_eq!(restarted.secret(&second.client_id, 4), Some(second.secret.clone()));
        assert!(restarted.revoke("nobody", 5).is_err());
        let revoked = restarted.revoke("LAPTOP-2", 5).unwrap();
        assert_eq!((revoked.id.as_str(), revoked.revoked_ms), (second.client_id.as_str(), 5));
        assert_eq!(restarted.secret(&second.client_id, 6), None);
        assert!(restarted.refusal(&second.client_id).contains("was revoked"));
        assert!(restarted.refusal("pc-unknown").contains("not paired"));
        assert!(restarted.revoke(&second.client_id, 7).is_err(), "already revoked");
        let stored = ClientsFile::load(&home).unwrap();
        assert!(stored.clients[1].secret.is_empty(), "a revoked client's secret is erased");
        assert_eq!(stored.active().count(), 1);
        assert_eq!(registry.secret(&first.client_id, 8), Some(first.secret));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_tunnel_manifest_parses_as_an_automation() {
        let path = std::env::temp_dir().join("x").join(format!("{}.yaml", remote::TUNNEL_TASK));
        let text = tunnel_manifest(Path::new(r"C:\Users\me\it's\devtunnel.exe"), "copilot-canvas", 47600);
        let manifest = crate::factory::parse_manifest(&text, &path).unwrap();
        assert_eq!(manifest.name, remote::TUNNEL_TASK);
        assert_eq!(manifest.command, r"C:\Users\me\it's\devtunnel.exe");
        assert_eq!(manifest.args, "host copilot-canvas --port-numbers 47600");
        assert_eq!(manifest.schedule.iter().map(|t| t.text()).collect::<Vec<_>>(), ["logon", "every 1h"]);
        assert_eq!((manifest.restart_count, manifest.time_limit_ms, manifest.overlap.as_str()), (10, 0, "ignore"));
    }
}
