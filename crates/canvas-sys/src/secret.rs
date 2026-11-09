//! Secrets at rest (paired clients' and servers' secrets).
//!
//! Windows keeps DPAPI in user scope and its hex blobs, unchanged. macOS and
//! Linux seal each secret with one random 32-byte master key per data home
//! (MAC-LINUX-PLAN D6, §5.4) using the repository's HMAC-SHA256
//! stream-and-tag sealing, stored as `v1u:<nonce hex><sealed hex>`. The
//! master key lives in the platform store (the macOS login Keychain, the
//! Linux Secret Service) or, without one and always with
//! `ATELIER_SECRETS=file` (CI, development builds, headless Linux), in
//! a 0600 key file `<home>/remote/master.key` in a 0700 folder.

#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::Path;

pub fn seal(_home: &Path, data: &[u8], description: &str) -> io::Result<String> {
    canvas_common::dpapi::protect_hex(data, description)
}

pub fn open(_home: &Path, text: &str) -> io::Result<Vec<u8>> {
    canvas_common::dpapi::unprotect_hex(text)
}

/// Where secrets are kept (`doctor`).
pub fn backend(_home: &Path) -> String {
    "DPAPI (user scope)".into()
}
}

#[cfg(unix)]
pub use unix::{Store, backend, open, seal, store};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::path::{Path, PathBuf};

    use canvas_common::paths::{hex, unhex};

    const PREFIX: &str = "v1u:";
    const LABEL: &str = "copilot-canvas secret at rest v1";
    const NONCE_LEN: usize = 16;

    fn key_path(home: &Path) -> PathBuf {
        home.join("remote").join("master.key")
    }

    /// Where the master key of a home lives.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Store {
        /// `<home>/remote/master.key`, 0600 in a 0700 folder.
        File,
        /// The platform store: a generic password in the login Keychain
        /// (macOS), an item of the Secret Service (Linux).
        Keychain,
    }

    /// The home's store: `ATELIER_SECRETS=file|keychain`; else the one
    /// the home already uses (its key file, or the Keychain marker); else, for
    /// a new home, the Keychain when this program is signed with a Developer
    /// ID (its Keychain access needs no prompt and every signed build shares
    /// it), on Linux the Secret Service when one answers on the session bus;
    /// the key file otherwise (development builds, headless machines).
    pub fn store(home: &Path) -> Store {
        match canvas_common::env::var("ATELIER_SECRETS").ok().as_deref().map(str::trim) {
            Some("file") => return Store::File,
            Some("keychain") if super::keychain::available() => return Store::Keychain,
            _ => {}
        }
        if key_path(home).is_file() {
            return Store::File;
        }
        if super::keychain::marker(home).is_file() && super::keychain::available() {
            return Store::Keychain;
        }
        if super::keychain::available() && super::keychain::signed_for_release() { Store::Keychain } else { Store::File }
    }

    /// The home's master key in the key file, created on first use.
    fn file_key(home: &Path) -> io::Result<Vec<u8>> {
        let path = key_path(home);
        match std::fs::read_to_string(&path) {
            Ok(text) => unhex(text.trim()).filter(|key| key.len() == 32).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a master key", path.display()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::fs::private_dir(path.parent().expect("remote folder"))?;
                let key = canvas_common::protocol::nonce()[..32].to_vec();
                match crate::fs::create_private(&path) {
                    Ok(mut file) => {
                        use std::io::Write;
                        file.write_all(hex(&key).as_bytes())?;
                        file.sync_all()?;
                        Ok(key)
                    }
                    // Another process created it first: use theirs.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => file_key(home),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn master_key(home: &Path, store: Store, service: &str) -> io::Result<Vec<u8>> {
        match store {
            Store::File => file_key(home),
            Store::Keychain => super::keychain::master_key(home, service),
        }
    }

    /// Seals `data` with the home's master key.
    pub fn seal(home: &Path, data: &[u8], _description: &str) -> io::Result<String> {
        seal_in(home, store(home), super::keychain::SERVICE, data)
    }

    pub(super) fn seal_in(home: &Path, store: Store, service: &str, data: &[u8]) -> io::Result<String> {
        let key = master_key(home, store, service)?;
        let (nonce, sealed) = canvas_common::pairing::seal_for_client(&key, LABEL, data);
        Ok(format!("{PREFIX}{}{}", hex(&nonce), hex(&sealed)))
    }

    /// Opens what [`seal`] produced with the same home's master key.
    pub fn open(home: &Path, text: &str) -> io::Result<Vec<u8>> {
        open_in(home, store(home), super::keychain::SERVICE, text)
    }

    pub(super) fn open_in(home: &Path, store: Store, service: &str, text: &str) -> io::Result<Vec<u8>> {
        let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
        let body = text.strip_prefix(PREFIX).ok_or_else(|| invalid("not a secret sealed on macOS or Linux (a Windows DPAPI blob?)"))?;
        let bytes = unhex(body).ok_or_else(|| invalid("a sealed secret is not hex"))?;
        if bytes.len() < NONCE_LEN {
            return Err(invalid("a sealed secret is too short"));
        }
        let (nonce, sealed) = bytes.split_at(NONCE_LEN);
        let key = master_key(home, store, service)?;
        canvas_common::pairing::open_for_client(&key, LABEL, nonce, sealed).ok_or_else(|| invalid("cannot open a sealed secret (another data home's, or changed)"))
    }

    /// Where secrets are kept (`doctor`).
    pub fn backend(home: &Path) -> String {
        match store(home) {
            Store::File => format!("master key file {} (0600)", key_path(home).display()),
            Store::Keychain => super::keychain::describe(home),
        }
    }
}

/// The macOS Keychain store of a home's master key (MAC-LINUX-PLAN §5.4):
/// one generic password (service "Atelier", account = the home's
/// hash), so every secret of the home costs at most one access decision.
#[cfg(target_os = "macos")]
mod keychain {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use security_framework::passwords::{PasswordOptions, delete_generic_password, generic_password, set_generic_password_options};

    pub const SERVICE: &str = "Atelier";
    /// errSecItemNotFound.
    const NOT_FOUND: i32 = -25300;

    /// The home's account name: a hash of its path.
    pub fn account(home: &Path) -> String {
        // An HMAC of the path (a fixed derivation, the same in every process).
        let digest = canvas_common::pairing::revocation_key(home.to_string_lossy().as_bytes());
        format!("home-{}", canvas_common::paths::hex(&digest[..12]))
    }

    /// Marks a home whose master key is in the Keychain.
    pub fn marker(home: &Path) -> PathBuf {
        home.join("remote").join("master.keychain")
    }

    pub fn available() -> bool {
        true
    }

    /// Where the home's master key is (`doctor`).
    pub fn describe(home: &Path) -> String {
        format!("login Keychain item \"{SERVICE}\" / {}", account(home))
    }

    /// This executable carries a Developer ID signature (a team identifier).
    pub fn signed_for_release() -> bool {
        static SIGNED: OnceLock<bool> = OnceLock::new();
        *SIGNED.get_or_init(|| {
            let Ok(exe) = std::env::current_exe() else { return false };
            std::process::Command::new("/usr/bin/codesign")
                .arg("-dv")
                .arg(&exe)
                .output()
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stderr).lines().any(|l| l.strip_prefix("TeamIdentifier=").is_some_and(|t| !t.is_empty() && t != "not set")))
                .unwrap_or(false)
        })
    }

    fn options(home: &Path, service: &str) -> PasswordOptions {
        PasswordOptions::new_generic_password(service, &account(home))
    }

    /// The home's master key, created (and the home marked) on first use.
    /// `service` is [`SERVICE`] (tests use their own).
    pub fn master_key(home: &Path, service: &str) -> io::Result<Vec<u8>> {
        match generic_password(options(home, service)) {
            Ok(key) if key.len() == 32 => return Ok(key),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "the Keychain item is not a master key")),
            Err(error) if error.code() == NOT_FOUND => {}
            Err(error) => return Err(io::Error::other(format!("cannot read the master key from the Keychain: {error}"))),
        }
        let key = canvas_common::protocol::nonce()[..32].to_vec();
        let mut new = options(home, service);
        new.set_label("Atelier master key");
        new.set_comment(&format!("Seals the paired secrets of {}", home.display()));
        set_generic_password_options(&key, new).map_err(|e| io::Error::other(format!("cannot store the master key in the Keychain: {e}")))?;
        crate::fs::private_dir(&home.join("remote"))?;
        let _ = std::fs::write(marker(home), b"the master key of this home is in the login Keychain\n");
        // Another process may have stored its key first: use the stored one.
        generic_password(options(home, service)).map_err(|e| io::Error::other(format!("cannot read the master key from the Keychain: {e}")))
    }

    /// Removes the home's item (tests).
    #[allow(dead_code)]
    pub fn forget(home: &Path, service: &str) {
        let _ = delete_generic_password(service, &account(home));
    }
}

/// The Linux Secret Service store of a home's master key (MAC-LINUX-PLAN
/// §5.4): one item (attributes `application` = "atelier", `home` = the
/// home's hash) in the default collection of whatever answers
/// `org.freedesktop.secrets` on the session bus (GNOME Keyring, KWallet,
/// KeePassXC). A locked collection is unlocked through the desktop's prompt,
/// once. Without a session bus or a provider (servers, SSH) it is not
/// available and new homes use the key file.
#[cfg(target_os = "linux")]
mod keychain {
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use secret_service::EncryptionType;
    use secret_service::blocking::{Collection, SecretService};

    pub const SERVICE: &str = "atelier";

    /// The home's account: a hash of its path (the same as the macOS one).
    pub fn account(home: &Path) -> String {
        let digest = canvas_common::pairing::revocation_key(home.to_string_lossy().as_bytes());
        format!("home-{}", canvas_common::paths::hex(&digest[..12]))
    }

    /// Marks a home whose master key is in the platform store.
    pub fn marker(home: &Path) -> PathBuf {
        home.join("remote").join("master.keychain")
    }

    fn connect() -> io::Result<SecretService<'static>> {
        SecretService::connect(EncryptionType::Dh).map_err(|e| io::Error::new(io::ErrorKind::NotConnected, format!("no Secret Service on the session bus: {e}")))
    }

    /// A Secret Service answers on the session bus (checked once per process).
    pub fn available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
                || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| Path::new(&dir).join("bus").exists());
            bus && connect().is_ok()
        })
    }

    /// Where the home's master key is (`doctor`).
    pub fn describe(home: &Path) -> String {
        format!("Secret Service item application={SERVICE} home={}", account(home))
    }

    /// No signature gate on Linux: the Secret Service asks no per-program
    /// question, so a new home uses it whenever it is available.
    pub fn signed_for_release() -> bool {
        true
    }

    fn other(what: &str, error: secret_service::Error) -> io::Error {
        io::Error::other(format!("{what} the master key in the Secret Service: {error}"))
    }

    /// The default collection ("login"), else any, else a new one; unlocked
    /// (the desktop may ask for the keyring's password once).
    fn collection<'a>(ss: &'a SecretService<'a>) -> io::Result<Collection<'a>> {
        let collection = match ss.get_default_collection() {
            Ok(found) => found,
            Err(_) => match ss.get_any_collection() {
                Ok(found) => found,
                Err(_) => ss.create_collection("Login", "default").map_err(|e| other("cannot create a collection for", e))?,
            },
        };
        collection.ensure_unlocked().map_err(|e| other("the keyring is locked: cannot reach", e))?;
        Ok(collection)
    }

    /// The home's master key, created (and the home marked) on first use.
    /// `service` is [`SERVICE`] (tests use their own).
    pub fn master_key(home: &Path, service: &str) -> io::Result<Vec<u8>> {
        let ss = connect()?;
        let account = account(home);
        let attributes = HashMap::from([("application", service), ("home", account.as_str())]);
        let found = ss.search_items(attributes.clone()).map_err(|e| other("cannot look up", e))?;
        if let Some(item) = found.unlocked.first().or(found.locked.first()) {
            item.ensure_unlocked().map_err(|e| other("the keyring is locked: cannot read", e))?;
            let key = item.get_secret().map_err(|e| other("cannot read", e))?;
            return if key.len() == 32 { Ok(key) } else { Err(io::Error::new(io::ErrorKind::InvalidData, "the Secret Service item is not a master key")) };
        }
        let key = canvas_common::protocol::nonce()[..32].to_vec();
        let label = format!("Atelier master key ({})", home.display());
        collection(&ss)?.create_item(&label, attributes.clone(), &key, false, "application/octet-stream").map_err(|e| other("cannot store", e))?;
        crate::fs::private_dir(&home.join("remote"))?;
        let _ = std::fs::write(marker(home), b"the master key of this home is in the Secret Service\n");
        // Another process may have stored its key first: use the oldest one.
        let found = ss.search_items(attributes).map_err(|e| other("cannot look up", e))?;
        let mut items: Vec<_> = found.unlocked.into_iter().chain(found.locked).collect();
        items.sort_by_key(|item| item.get_created().unwrap_or(u64::MAX));
        let item = items.first().ok_or_else(|| io::Error::other("the stored master key is gone"))?;
        item.get_secret().map_err(|e| other("cannot read", e))
    }

    /// Removes the home's items (tests).
    #[allow(dead_code)]
    pub fn forget(home: &Path, service: &str) {
        let Ok(ss) = connect() else { return };
        let account = account(home);
        if let Ok(found) = ss.search_items(HashMap::from([("application", service), ("home", account.as_str())])) {
            for item in found.unlocked.iter().chain(&found.locked) {
                let _ = item.delete();
            }
        }
    }
}

/// No platform store on other Unixes.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
mod keychain {
    use std::io;
    use std::path::{Path, PathBuf};

    pub const SERVICE: &str = "";

    pub fn marker(home: &Path) -> PathBuf {
        home.join("remote").join("master.keychain")
    }

    pub fn available() -> bool {
        false
    }

    pub fn describe(_home: &Path) -> String {
        String::new()
    }

    pub fn signed_for_release() -> bool {
        false
    }

    pub fn master_key(_home: &Path, _service: &str) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no platform store on this system"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secrets_round_trip_with_the_homes_master_key_only() {
        let home = std::env::temp_dir().join(format!("cs-secret-{}", uuid::Uuid::new_v4().simple()));
        let other = std::env::temp_dir().join(format!("cs-secret-{}", uuid::Uuid::new_v4().simple()));
        let sealed = super::seal(&home, b"top secret", "test").unwrap();
        assert!(sealed.starts_with("v1u:"));
        assert!(!sealed.contains(&canvas_common::paths::hex(b"top secret")));
        assert_eq!(super::open(&home, &sealed).unwrap(), b"top secret");
        assert!(super::open(&other, &sealed).is_err(), "another home's key cannot open it");
        let key = home.join("remote").join("master.key");
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(key.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(super::open(&home, "0123abcd").is_err(), "a DPAPI blob is refused");
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(other);
    }

    /// The Keychain store, with a test-only service name and its items
    /// deleted afterwards (safety rule 4); only with
    /// `ATELIER_KEYCHAIN_TESTS=1` (a runner may have no login Keychain).
    #[test]
    #[cfg(target_os = "macos")]
    fn the_keychain_holds_one_master_key_per_home() {
        if canvas_common::env::var_os("ATELIER_KEYCHAIN_TESTS").is_none() {
            eprintln!("skipped: set ATELIER_KEYCHAIN_TESTS=1 to use the login Keychain");
            return;
        }
        use super::unix::{Store, open_in, seal_in};
        let service = format!("Atelier test {}", uuid::Uuid::new_v4().simple());
        let home = std::env::temp_dir().join(format!("cs-keychain-{}", uuid::Uuid::new_v4().simple()));
        let sealed = seal_in(&home, Store::Keychain, &service, b"top secret").unwrap();
        assert!(super::keychain::marker(&home).is_file() && !home.join("remote").join("master.key").exists());
        assert_eq!(super::store(&home), Store::Keychain, "the home now uses the Keychain");
        assert_eq!(open_in(&home, Store::Keychain, &service, &sealed).unwrap(), b"top secret");
        super::keychain::forget(&home, &service);
        assert!(open_in(&home, Store::Keychain, &service, &sealed).is_err(), "a new key cannot open the old secret");
        super::keychain::forget(&home, &service);
        let _ = std::fs::remove_dir_all(home);
    }

    /// The Secret Service store, with a test-only `application` attribute and
    /// its items deleted afterwards; only with
    /// `ATELIER_SECRET_SERVICE_TESTS=1` (CI runs it in a private
    /// session bus with an unlocked GNOME Keyring).
    #[test]
    #[cfg(target_os = "linux")]
    fn the_secret_service_holds_one_master_key_per_home() {
        if canvas_common::env::var_os("ATELIER_SECRET_SERVICE_TESTS").is_none() {
            eprintln!("skipped: set ATELIER_SECRET_SERVICE_TESTS=1 to use the session's Secret Service");
            return;
        }
        use super::unix::{Store, open_in, seal_in};
        assert!(super::keychain::available(), "a Secret Service answers");
        let service = format!("atelier-test-{}", uuid::Uuid::new_v4().simple());
        let home = std::env::temp_dir().join(format!("cs-secretservice-{}", uuid::Uuid::new_v4().simple()));
        let other = std::env::temp_dir().join(format!("cs-secretservice-{}", uuid::Uuid::new_v4().simple()));
        let sealed = seal_in(&home, Store::Keychain, &service, b"top secret").unwrap();
        assert!(super::keychain::marker(&home).is_file() && !home.join("remote").join("master.key").exists());
        assert_eq!(open_in(&home, Store::Keychain, &service, &sealed).unwrap(), b"top secret");
        assert_eq!(open_in(&home, Store::Keychain, &service, &seal_in(&home, Store::Keychain, &service, b"again").unwrap()).unwrap(), b"again", "the same key is used again");
        assert!(open_in(&other, Store::Keychain, &service, &sealed).is_err(), "another home's key cannot open it");
        super::keychain::forget(&home, &service);
        super::keychain::forget(&other, &service);
        assert!(open_in(&home, Store::Keychain, &service, &sealed).is_err(), "a new key cannot open the old secret");
        super::keychain::forget(&home, &service);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(other);
    }
}
