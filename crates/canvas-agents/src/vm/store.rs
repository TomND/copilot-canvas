//! The service's VM stores under `<home>/vm/` (docs/VM-SESSIONS-PLAN.md
//! §2.7): this hub's id, the satellites' credentials, the sandboxes it made,
//! the images per repository and the day's budget. The service is their only
//! writer; each file carries a format version, is written atomically, and a
//! file that cannot be read (or that a newer Atelier wrote) is never written
//! over: changes are refused with the reason until it reads again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use canvas_common::handshake::Credential;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::paths::now_ms;

pub const STORE_VERSION: u32 = 1;

pub fn dir(home: &Path) -> PathBuf {
    home.join("vm")
}

/// A versioned store: `Ok(None)` when it does not exist yet.
fn read<T: DeserializeOwned + Versioned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: T = serde_json::from_slice(&bytes).with_context(|| format!("{} cannot be read: fix or move it", path.display()))?;
            if value.version() > STORE_VERSION {
                bail!("{} was written by a newer Atelier: it is not changed by this one", path.display());
            }
            Ok(Some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    crate::paths::write_atomic(path, &serde_json::to_vec_pretty(value)?).with_context(|| format!("cannot write {}", path.display()))
}

trait Versioned {
    fn version(&self) -> u32;
}

macro_rules! versioned {
    ($($t:ty),*) => { $(impl Versioned for $t { fn version(&self) -> u32 { self.version } })* };
}

/// This hub's identity: every sandbox it makes carries `atelier.hub=<id>`,
/// so reconciliation tells its own sandboxes from others'.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Hub {
    pub version: u32,
    pub id: String,
}

/// The hub's id (made once).
pub fn hub_id(home: &Path) -> Result<String> {
    let path = dir(home).join("hub.json");
    if let Some(hub) = read::<Hub>(&path)?.filter(|h| !h.id.is_empty()) {
        return Ok(hub.id);
    }
    let hub = Hub { version: STORE_VERSION, id: uuid::Uuid::new_v4().simple().to_string()[..16].to_owned() };
    write(&path, &hub)?;
    Ok(hub.id)
}

// ---------- satellites' credentials ----------

/// A satellite's credential (`satellites.json`): an id of the paired-client
/// form (`sat-` and 16 hex digits) and a secret sealed for this home.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SatelliteRecord {
    pub id: String,
    pub sandbox: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    pub revoked_ms: u64,
    /// Sealed (`canvas_sys::secret`); empty once revoked.
    pub secret: String,
    /// Once revoked: the key its refusals are proven with, sealed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub revoked_key: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SatellitesFile {
    pub version: u32,
    pub satellites: Vec<SatelliteRecord>,
}

versioned!(Hub, SatellitesFile, SandboxesFile, ImagesFile, BudgetFile);

/// Whether `id` is a satellite credential's id.
pub fn is_satellite_id(id: &str) -> bool {
    id.len() == 20 && id.starts_with("sat-") && id[4..].chars().all(|c| c.is_ascii_hexdigit())
}

pub struct Satellites {
    home: PathBuf,
}

/// A satellite credential handed to a guest (`satellite.json`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(default)]
pub struct Minted {
    pub id: String,
    /// Hex.
    pub secret: String,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Minted({}, …)", self.id)
    }
}

impl Satellites {
    pub fn new(home: &Path) -> Self {
        Satellites { home: home.to_path_buf() }
    }

    fn path(&self) -> PathBuf {
        dir(&self.home).join("satellites.json")
    }

    fn load(&self) -> Result<SatellitesFile> {
        Ok(read(&self.path())?.unwrap_or(SatellitesFile { version: STORE_VERSION, ..Default::default() }))
    }

    fn save(&self, file: &mut SatellitesFile) -> Result<()> {
        file.version = STORE_VERSION;
        write(&self.path(), file)
    }

    /// A new credential for `sandbox` (made at its claim, revoked at its deletion).
    pub fn mint(&self, sandbox: &str) -> Result<Minted> {
        let mut file = self.load()?;
        let id = format!("sat-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let secret = canvas_common::pairing::new_secret();
        let sealed = canvas_sys::secret::seal(&self.home, &secret, "Atelier VM satellite").context("cannot protect the satellite's secret")?;
        file.satellites.push(SatelliteRecord { id: id.clone(), sandbox: sandbox.to_owned(), created_ms: now_ms(), secret: sealed, ..Default::default() });
        self.save(&mut file)?;
        Ok(Minted { id, secret: canvas_common::paths::hex(&secret) })
    }

    /// What the handshake authenticates a satellite id with.
    pub fn credential(&self, id: &str) -> Credential {
        if !is_satellite_id(id) {
            return Credential::Refused;
        }
        let Ok(file) = self.load() else { return Credential::Refused };
        let Some(record) = file.satellites.iter().find(|s| s.id == id) else { return Credential::Refused };
        let (sealed, revoked) = if record.revoked_ms == 0 { (&record.secret, false) } else { (&record.revoked_key, true) };
        match canvas_sys::secret::open(&self.home, sealed) {
            Ok(key) if revoked => Credential::Revoked(key),
            Ok(secret) => Credential::Satellite(secret),
            Err(_) => Credential::Refused,
        }
    }

    /// The sandbox an active credential belongs to.
    pub fn sandbox_of(&self, id: &str) -> Option<String> {
        self.load().ok()?.satellites.into_iter().find(|s| s.id == id && s.revoked_ms == 0).map(|s| s.sandbox)
    }

    pub fn seen(&self, id: &str) {
        if let Ok(mut file) = self.load()
            && let Some(record) = file.satellites.iter_mut().find(|s| s.id == id)
        {
            record.last_seen_ms = now_ms();
            let _ = self.save(&mut file);
        }
    }

    /// Revokes every credential of `sandbox` (its deletion): their secrets
    /// are erased, only a key to prove the refusal is kept. Returns how many.
    pub fn revoke_sandbox(&self, sandbox: &str) -> Result<usize> {
        let mut file = self.load()?;
        let mut count = 0;
        for record in file.satellites.iter_mut().filter(|s| s.sandbox == sandbox && s.revoked_ms == 0) {
            let key = canvas_sys::secret::open(&self.home, &record.secret).ok().map(|s| canvas_common::pairing::revocation_key(&s));
            record.revoked_key = key.and_then(|k| canvas_sys::secret::seal(&self.home, &k, "Atelier VM satellite revoked").ok()).unwrap_or_default();
            record.secret.clear();
            record.revoked_ms = now_ms();
            count += 1;
        }
        // Revoked records older than 30 days are forgotten.
        let cutoff = now_ms().saturating_sub(30 * 86_400_000);
        file.satellites.retain(|s| s.revoked_ms == 0 || s.revoked_ms > cutoff);
        self.save(&mut file)?;
        Ok(count)
    }

    pub fn list(&self) -> Result<Vec<SatelliteRecord>> {
        Ok(self.load()?.satellites)
    }
}

// ---------- sandboxes ----------

/// What a sandbox is for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Ready in the warm pool (no credential, no lease).
    #[default]
    Pool,
    /// Leased to one or more boxes (a session and its subsessions).
    Session,
    /// Builds an image.
    Builder,
    /// Found on the provider with this hub's label but not in this store
    /// (a crash between its creation and its record): listed for you to
    /// adopt or delete, never deleted on its own.
    Orphan,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SandboxRecord {
    pub id: String,
    pub role: Role,
    pub repo: String,
    pub size: String,
    /// The image it was made from.
    pub image: String,
    pub created_ms: u64,
    /// The boxes it runs (a lease).
    pub boxes: Vec<String>,
    pub suspended: bool,
    /// When it last ran or was resumed (cost accounting).
    pub running_since_ms: u64,
    /// When it was last seen in use (suspend-after).
    pub used_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SandboxesFile {
    pub version: u32,
    pub sandboxes: Vec<SandboxRecord>,
}

pub fn sandboxes_path(home: &Path) -> PathBuf {
    dir(home).join("sandboxes.json")
}

pub fn load_sandboxes(home: &Path) -> Result<SandboxesFile> {
    Ok(read(&sandboxes_path(home))?.unwrap_or(SandboxesFile { version: STORE_VERSION, ..Default::default() }))
}

pub fn save_sandboxes(home: &Path, file: &mut SandboxesFile) -> Result<()> {
    file.version = STORE_VERSION;
    write(&sandboxes_path(home), file)
}

// ---------- images ----------

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ImageRecord {
    pub repo: String,
    pub id: String,
    pub name: String,
    pub built_ms: u64,
    /// `ready`, `building` or `failed`.
    pub state: String,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ImagesFile {
    pub version: u32,
    pub images: Vec<ImageRecord>,
    /// Repository → when its last build started (the schedule).
    pub last_build_ms: BTreeMap<String, u64>,
}

pub fn images_path(home: &Path) -> PathBuf {
    dir(home).join("images.json")
}

pub fn load_images(home: &Path) -> Result<ImagesFile> {
    Ok(read(&images_path(home))?.unwrap_or(ImagesFile { version: STORE_VERSION, ..Default::default() }))
}

pub fn save_images(home: &Path, file: &mut ImagesFile) -> Result<()> {
    file.version = STORE_VERSION;
    write(&images_path(home), file)
}

impl ImagesFile {
    /// The newest ready image of a repository.
    pub fn newest(&self, repo: &str) -> Option<&ImageRecord> {
        self.images.iter().filter(|i| i.repo == repo && i.state == "ready").max_by_key(|i| i.built_ms)
    }
}

// ---------- budget ----------

/// Today's running time and its estimated cost (`budget.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BudgetFile {
    pub version: u32,
    /// `YYYY-MM-DD` (UTC) the totals are for.
    pub day: String,
    pub cents: u64,
    /// Sandbox → cents of today (for the view).
    pub by_sandbox: BTreeMap<String, u64>,
}

pub fn budget_path(home: &Path) -> PathBuf {
    dir(home).join("budget.json")
}

pub fn load_budget(home: &Path) -> Result<BudgetFile> {
    Ok(read(&budget_path(home))?.unwrap_or(BudgetFile { version: STORE_VERSION, ..Default::default() }))
}

pub fn save_budget(home: &Path, file: &mut BudgetFile) -> Result<()> {
    file.version = STORE_VERSION;
    write(&budget_path(home), file)
}

/// `YYYY-MM-DD` (UTC) of `ms`.
pub fn day_of(ms: u64) -> String {
    canvas_common::paths::format_rfc3339_ms(ms)[..10].to_owned()
}

impl BudgetFile {
    /// Adds `cents` for `sandbox`, starting a new day when the date changed.
    pub fn add(&mut self, now: u64, sandbox: &str, cents: u64) {
        let day = day_of(now);
        if self.day != day {
            self.day = day;
            self.cents = 0;
            self.by_sandbox.clear();
        }
        self.cents += cents;
        *self.by_sandbox.entry(sandbox.to_owned()).or_default() += cents;
    }

    pub fn today(&self, now: u64) -> u64 {
        if self.day == day_of(now) { self.cents } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        std::env::temp_dir().join(format!("atelier-vm-store-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn the_hub_id_is_made_once() {
        let home = home();
        let id = hub_id(&home).unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(hub_id(&home).unwrap(), id);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn satellite_credentials_are_sealed_revocable_and_told_apart() {
        let home = home();
        let store = Satellites::new(&home);
        let minted = store.mint("sbx-1").unwrap();
        assert!(is_satellite_id(&minted.id) && minted.secret.len() == 64);
        assert!(!format!("{minted:?}").contains(&minted.secret), "never shown");
        let text = std::fs::read_to_string(dir(&home).join("satellites.json")).unwrap();
        assert!(!text.contains(&minted.secret), "sealed on disk");
        match store.credential(&minted.id) {
            Credential::Satellite(secret) => assert_eq!(canvas_common::paths::hex(&secret), minted.secret),
            _ => panic!("a satellite"),
        }
        assert_eq!(store.sandbox_of(&minted.id).as_deref(), Some("sbx-1"));
        assert!(matches!(store.credential("sat-0000000000000000"), Credential::Refused));
        assert!(matches!(store.credential("pc-0123456789abcdef"), Credential::Refused), "a paired client's id is never a satellite");
        assert_eq!(store.revoke_sandbox("sbx-1").unwrap(), 1);
        assert!(matches!(store.credential(&minted.id), Credential::Revoked(_)), "a deleted sandbox's satellite can verify its refusal");
        assert_eq!(store.sandbox_of(&minted.id), None);
        assert!(!std::fs::read_to_string(dir(&home).join("satellites.json")).unwrap().contains(&minted.secret));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn stores_never_overwrite_what_they_cannot_read() {
        let home = home();
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(sandboxes_path(&home), "{ not json").unwrap();
        assert!(load_sandboxes(&home).is_err());
        std::fs::write(images_path(&home), r#"{"version": 99}"#).unwrap();
        assert!(load_images(&home).unwrap_err().to_string().contains("newer Atelier"));
        std::fs::write(dir(&home).join("satellites.json"), "[").unwrap();
        assert!(Satellites::new(&home).mint("sbx").is_err(), "no credential minted over an unreadable store");
        let mut fresh = load_budget(&home).unwrap();
        fresh.add(86_400_000 * 3 + 5, "sbx-1", 7);
        fresh.add(86_400_000 * 3 + 9, "sbx-2", 3);
        assert_eq!(fresh.today(86_400_000 * 3 + 10), 10);
        fresh.add(86_400_000 * 4, "sbx-1", 1);
        assert_eq!((fresh.cents, fresh.by_sandbox.len()), (1, 1), "a new day starts at zero");
        assert_eq!(fresh.today(86_400_000 * 5), 0);
        save_budget(&home, &mut fresh).unwrap();
        assert_eq!(load_budget(&home).unwrap(), fresh);
        let _ = std::fs::remove_dir_all(home);
    }
}
