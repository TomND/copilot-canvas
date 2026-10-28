//! Remote clients on the service's machine (PLAN.md D5 "App-level
//! authentication"): one-time pairing codes, the paired clients in
//! `<home>/remote/clients.json` (secrets DPAPI-protected), revocation, and
//! the `CopilotCanvas-Tunnel` automation that hosts the Dev Tunnel.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canvas_common::handshake::{Credential, PairGrant, Pairing};
use canvas_common::{dpapi, pairing, remote};
use serde::{Deserialize, Serialize};

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
    /// Once revoked: the key its refusals are proven with
    /// (`pairing::revocation_key`), DPAPI-protected (hex).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revoked_key: String,
    /// Who opened the code it paired with (`remote pair`), and when.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub paired_by: String,
    #[serde(default)]
    pub code_opened_ms: u64,
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
    opened_ms: u64,
    opened_by: String,
}

#[derive(Default)]
struct State {
    codes: Vec<Pending>,
    /// Failed pairing attempts in a row (reset by a success).
    failures: u32,
    /// No attempt runs before this time (ms).
    retry_at_ms: u64,
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
    /// `name`, opened by `opened_by` (for the log and the client list);
    /// returns it with its expiry.
    pub fn open_code(&self, name: Option<String>, opened_by: &str, now: u64) -> Result<(String, u64)> {
        if let Some(name) = &name {
            anyhow::ensure!(remote::valid_name(name), "a client name is 1-40 characters of letters, digits, '-', '_' and '.'");
        }
        let mut state = self.state();
        state.codes.retain(|c| c.expires_ms > now);
        if state.codes.len() >= pairing::MAX_OPEN_CODES {
            state.codes.remove(0);
        }
        let code = pairing::new_code();
        let expires_ms = now + pairing::CODE_TTL_MS;
        state.codes.push(Pending { code: code.clone(), name, expires_ms, opened_ms: now, opened_by: opened_by.to_owned() });
        Ok((code, expires_ms))
    }

    /// Codes open now.
    pub fn open_codes(&self, now: u64) -> usize {
        self.state().codes.iter().filter(|c| c.expires_ms > now).count()
    }

    /// How long a pairing attempt at `now` must wait (failed attempts back off).
    pub fn wait_at(&self, now: u64) -> Duration {
        Duration::from_millis(self.state().retry_at_ms.saturating_sub(now))
    }

    /// Codes open at `now` (canonical form).
    pub fn codes_at(&self, now: u64) -> Vec<String> {
        let mut state = self.state();
        state.codes.retain(|c| c.expires_ms > now);
        state.codes.iter().map(|c| c.code.clone()).collect()
    }

    /// A failed attempt at `now`: the next one waits [`pairing::backoff`].
    pub fn failed_at(&self, now: u64) {
        let mut state = self.state();
        state.failures = state.failures.saturating_add(1);
        state.retry_at_ms = now + pairing::backoff(state.failures).as_millis() as u64;
    }

    /// Uses up `code` (it must still be open) and creates the client: the
    /// name `remote pair --name` gave, else the client's own (cleaned).
    pub fn grant_at(&self, code: &str, client_name: &str, now: u64) -> Result<PairGrant, String> {
        let pending = {
            let mut state = self.state();
            state.codes.retain(|c| c.expires_ms > now);
            let index = state.codes.iter().position(|c| c.code == code).ok_or("the pairing code expired meanwhile; open a new one")?;
            state.failures = 0;
            state.retry_at_ms = 0;
            state.codes.remove(index)
        };
        let name = pending.name.clone().unwrap_or_else(|| clean_name(client_name));
        self.add_client(&name, &pending, now).map(|(client_id, secret)| PairGrant { client_id, secret }).map_err(|e| format!("the service could not store the client: {e:#}"))
    }

    /// Stores a new client (its name made unique among active clients).
    fn add_client(&self, name: &str, pending: &Pending, now: u64) -> Result<(String, Vec<u8>)> {
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
        file.clients.push(ClientRecord {
            id: id.clone(),
            name: unique,
            created_ms: now,
            secret: sealed,
            paired_by: pending.opened_by.clone(),
            code_opened_ms: pending.opened_ms,
            ..Default::default()
        });
        file.save(&self.home)?;
        Ok((id, secret))
    }

    /// What a client id authenticates with: the secret of an active client,
    /// the revocation key of a revoked one, else nothing.
    pub fn credential(&self, id: &str) -> Credential {
        let _guard = self.state();
        let Ok(file) = ClientsFile::load(&self.home) else { return Credential::Refused };
        let Some(client) = file.clients.iter().find(|c| c.id == id) else { return Credential::Refused };
        let (sealed, revoked) = if client.revoked_ms == 0 { (&client.secret, false) } else { (&client.revoked_key, true) };
        match dpapi::unprotect_hex(sealed) {
            Ok(key) if revoked => Credential::Revoked(key),
            Ok(secret) => Credential::Secret(secret),
            Err(_) => Credential::Refused,
        }
    }

    /// The secret of an active client.
    pub fn secret(&self, id: &str) -> Option<Vec<u8>> {
        match self.credential(id) {
            Credential::Secret(secret) => Some(secret),
            _ => None,
        }
    }

    /// Notes that an active client authenticated at `now`.
    pub fn seen(&self, id: &str, now: u64) {
        let _guard = self.state();
        let Ok(mut file) = ClientsFile::load(&self.home) else { return };
        if let Some(client) = file.clients.iter_mut().find(|c| c.id == id && c.revoked_ms == 0) {
            client.last_seen_ms = now;
            let _ = file.save(&self.home);
        }
    }

    /// Why `id` is refused (for the service log only).
    pub fn describe(&self, id: &str) -> String {
        match ClientsFile::load(&self.home).ok().and_then(|f| f.clients.into_iter().find(|c| c.id == id)) {
            Some(client) if client.revoked_ms > 0 => format!("{:?} ({:?}) was revoked", client.id, client.name),
            Some(client) => format!("the secret of {:?} ({:?}) cannot be read", client.id, client.name),
            None => format!("{id:?} is not a paired client"),
        }
    }

    pub fn list(&self) -> Result<Vec<ClientRecord>> {
        let _guard = self.state();
        Ok(ClientsFile::load(&self.home)?.clients)
    }

    /// Active clients.
    pub fn active_count(&self) -> usize {
        self.list().map(|clients| clients.iter().filter(|c| c.revoked_ms == 0).count()).unwrap_or(0)
    }

    /// Revokes the active client with this id or name: its secret is erased
    /// and only the key that proves its refusals is kept.
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
        client.revoked_key = dpapi::unprotect_hex(&client.secret)
            .ok()
            .and_then(|secret| dpapi::protect_hex(&pairing::revocation_key(&secret), "Copilot Canvas revoked client").ok())
            .unwrap_or_default();
        client.revoked_ms = now;
        client.secret.clear();
        let revoked = client.clone();
        file.save(&self.home)?;
        Ok(revoked)
    }
}

impl Pairing for Registry {
    fn wait(&self) -> Duration {
        self.wait_at(canvas_common::paths::now_ms())
    }

    fn codes(&self) -> Vec<String> {
        self.codes_at(canvas_common::paths::now_ms())
    }

    fn grant(&self, code: &str, name: &str) -> Result<PairGrant, String> {
        self.grant_at(code, name, canvas_common::paths::now_ms())
    }

    fn failed(&self) {
        self.failed_at(canvas_common::paths::now_ms())
    }
}

/// The `CopilotCanvas-Tunnel` manifest: `devtunnel host TUNNEL` (the tunnel
/// and its port `port` exist already: `devtunnel create`, `devtunnel port
/// create`) at logon and every hour (an hourly start does nothing while it
/// runs), restarted when it fails.
pub fn tunnel_manifest(devtunnel: &Path, tunnel: &str, port: u16) -> String {
    let q = |text: &str| format!("'{}'", text.replace('\'', "''"));
    format!(
        "# Hosts the private Dev Tunnel to the Copilot Canvas service (remote mode).\n\
         # Written by `copilot-canvas remote tunnel-setup`; register it with\n\
         # `copilot-canvas automations apply {task} --yes`. The tunnel forwards\n\
         # port {port} (`devtunnel port create {tunnel} -p {port}`).\n\
         name: {name}\n\
         family: 'Copilot Canvas'\n\
         description: 'Dev Tunnel host for remote canvases (devtunnel host {tunnel}, port {port})'\n\
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
        args = q(&format!("host {tunnel}")),
    )
}

/// Where `remote tunnel-setup` writes the manifest in an automations folder.
pub fn tunnel_manifest_path(dir: &Path) -> PathBuf {
    dir.join("copilot-canvas").join(format!("{}.yaml", remote::TUNNEL_TASK))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-remote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn codes_are_single_use_and_expire() {
        let home = home();
        let registry = Registry::new(&home);
        assert!(registry.codes_at(1_000).is_empty());
        let (code, expires) = registry.open_code(Some("home".into()), "a local process", 1_000).unwrap();
        assert_eq!(code.len(), pairing::CODE_LEN);
        assert_eq!(expires, 1_000 + pairing::CODE_TTL_MS);
        assert_eq!(registry.codes_at(2_000), std::slice::from_ref(&code));
        let grant = registry.grant_at(&code, "HOME PC!", 2_000).unwrap();
        assert_eq!(registry.secret(&grant.client_id), Some(grant.secret.clone()));
        assert!(registry.grant_at(&code, "x", 2_500).unwrap_err().contains("expired"), "used up");
        let (late, _) = registry.open_code(None, "a local process", 10_000).unwrap();
        assert!(registry.codes_at(10_000 + pairing::CODE_TTL_MS).is_empty(), "expired");
        assert!(registry.grant_at(&late, "x", 10_000 + pairing::CODE_TTL_MS).is_err());
        let (named_by_client, _) = registry.open_code(None, "terminal t1", 20_000).unwrap();
        let grant = registry.grant_at(&named_by_client, "HOME PC!", 20_001).unwrap();
        let list = registry.list().unwrap();
        let record = list.iter().find(|c| c.id == grant.client_id).unwrap();
        assert_eq!(record.name, "HOME-PC-", "the client's own name, cleaned");
        assert_eq!((record.paired_by.as_str(), record.code_opened_ms), ("terminal t1", 20_000), "who opened the code is kept");
        assert_eq!(list[0].last_seen_ms, 0, "not seen before it authenticated");
        registry.seen(&list[0].id, 3_000);
        assert_eq!(registry.list().unwrap()[0].last_seen_ms, 3_000);
        assert!(registry.open_code(Some("bad name".into()), "x", 0).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_attempts_back_off_and_a_success_resets_them() {
        let home = home();
        let registry = Registry::new(&home);
        let (code, _) = registry.open_code(None, "x", 0).unwrap();
        assert_eq!(registry.wait_at(0), Duration::ZERO);
        let mut now = 1_000;
        for failures in 1..=10u32 {
            registry.failed_at(now);
            assert_eq!(registry.wait_at(now), pairing::backoff(failures), "after {failures} failures");
            now += 100;
        }
        assert_eq!(registry.wait_at(now - 100), Duration::from_secs(30), "capped");
        assert_eq!(registry.codes_at(now).len(), 1, "codes stay open: failures only slow attempts down");
        registry.grant_at(&code, "pc", now).unwrap();
        assert_eq!(registry.wait_at(now), Duration::ZERO, "a success resets the backoff");
        // At most a few codes at once.
        for _ in 0..5 {
            registry.open_code(None, "x", now).unwrap();
        }
        assert_eq!(registry.open_codes(now), pairing::MAX_OPEN_CODES);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn clients_persist_protected_and_revocation_keeps_only_a_proof_key() {
        let home = home();
        let registry = Registry::new(&home);
        let (code, _) = registry.open_code(Some("laptop".into()), "x", 0).unwrap();
        let first = registry.grant_at(&code, "pc", 1).unwrap();
        let (code, _) = registry.open_code(Some("laptop".into()), "x", 2).unwrap();
        let second = registry.grant_at(&code, "pc", 3).unwrap();
        let text = std::fs::read_to_string(ClientsFile::path(&home)).unwrap();
        assert!(!text.contains(&canvas_common::paths::hex(&first.secret)), "secrets are DPAPI-protected on disk");
        let names: Vec<String> = registry.list().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["laptop", "laptop-2"], "names stay unique");
        // A second registry (a restarted service) reads the same file.
        let restarted = Registry::new(&home);
        assert_eq!(restarted.secret(&second.client_id), Some(second.secret.clone()));
        assert!(restarted.revoke("nobody", 5).is_err());
        let revoked = restarted.revoke("LAPTOP-2", 5).unwrap();
        assert_eq!((revoked.id.as_str(), revoked.revoked_ms), (second.client_id.as_str(), 5));
        assert_eq!(restarted.secret(&second.client_id), None);
        match restarted.credential(&second.client_id) {
            Credential::Revoked(key) => assert_eq!(key, pairing::revocation_key(&second.secret), "the client can check its refusal"),
            _ => panic!("a revoked client's refusal is proven"),
        }
        assert!(matches!(restarted.credential("pc-unknown"), Credential::Refused));
        assert!(restarted.describe(&second.client_id).contains("was revoked"));
        assert!(restarted.revoke(&second.client_id, 7).is_err(), "already revoked");
        let stored = ClientsFile::load(&home).unwrap();
        assert!(stored.clients[1].secret.is_empty(), "a revoked client's secret is erased");
        assert!(!stored.clients[1].revoked_key.is_empty() && !std::fs::read_to_string(ClientsFile::path(&home)).unwrap().contains(&canvas_common::paths::hex(&second.secret)));
        assert_eq!(stored.active().count(), 1);
        assert_eq!(restarted.active_count(), 1);
        assert_eq!(registry.secret(&first.client_id), Some(first.secret));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_tunnel_manifest_parses_as_an_automation() {
        let path = std::env::temp_dir().join("x").join(format!("{}.yaml", remote::TUNNEL_TASK));
        let text = tunnel_manifest(Path::new(r"C:\Users\me\it's\devtunnel.exe"), "copilot-canvas", 47600);
        let manifest = crate::factory::parse_manifest(&text, &path).unwrap();
        assert_eq!(manifest.name, remote::TUNNEL_TASK);
        assert_eq!(manifest.command, r"C:\Users\me\it's\devtunnel.exe");
        // `devtunnel host ID` hosts the tunnel with the ports it has (`port create`).
        assert_eq!(manifest.args, "host copilot-canvas");
        assert!(text.contains("port create copilot-canvas -p 47600"), "the manifest says which port it needs");
        assert_eq!(manifest.schedule.iter().map(|t| t.text()).collect::<Vec<_>>(), ["logon", "every 1h"]);
        assert_eq!((manifest.restart_count, manifest.time_limit_ms, manifest.overlap.as_str()), (10, 0, "ignore"));
    }
}
