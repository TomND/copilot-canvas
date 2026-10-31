//! Secrets at rest (paired clients' and servers' secrets).
//!
//! Windows keeps DPAPI in user scope and its hex blobs, unchanged. macOS and
//! Linux seal each secret with one random 32-byte master key per data home
//! (MAC-LINUX-PLAN D6, §5.4) using the repository's HMAC-SHA256
//! stream-and-tag sealing, stored as `v1u:<nonce hex><sealed hex>`. The
//! master key lives in the platform store; until that backend is wired, and
//! always with `COPILOT_CANVAS_SECRETS=file` (CI, development builds), it is
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
        /// A generic password in the login Keychain (macOS).
        Keychain,
    }

    /// The home's store: `COPILOT_CANVAS_SECRETS=file|keychain`; else the one
    /// the home already uses (its key file, or the Keychain marker); else, for
    /// a new home, the Keychain when this program is signed with a Developer
    /// ID (its Keychain access needs no prompt and every signed build shares
    /// it), the key file otherwise (development builds, Linux until the
    /// Secret Service backend lands).
    pub fn store(home: &Path) -> Store {
        match std::env::var("COPILOT_CANVAS_SECRETS").ok().as_deref().map(str::trim) {
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
            Store::Keychain => format!("login Keychain item \"{}\" / {}", super::keychain::SERVICE, super::keychain::account(home)),
        }
    }
}

/// The macOS Keychain store of a home's master key (MAC-LINUX-PLAN §5.4):
/// one generic password (service "Copilot Canvas", account = the home's
/// hash), so every secret of the home costs at most one access decision.
#[cfg(target_os = "macos")]
mod keychain {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use security_framework::passwords::{PasswordOptions, delete_generic_password, generic_password, set_generic_password_options};

    pub const SERVICE: &str = "Copilot Canvas";
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
        new.set_label("Copilot Canvas master key");
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

/// No Keychain elsewhere (Linux: the Secret Service backend comes with the
/// Linux port; the key file meanwhile).
#[cfg(all(unix, not(target_os = "macos")))]
mod keychain {
    use std::io;
    use std::path::{Path, PathBuf};

    pub const SERVICE: &str = "";

    pub fn account(_home: &Path) -> String {
        String::new()
    }

    pub fn marker(home: &Path) -> PathBuf {
        home.join("remote").join("master.keychain")
    }

    pub fn available() -> bool {
        false
    }

    pub fn signed_for_release() -> bool {
        false
    }

    pub fn master_key(_home: &Path, _service: &str) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no Keychain on this platform"))
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
    /// `COPILOT_CANVAS_KEYCHAIN_TESTS=1` (a runner may have no login Keychain).
    #[test]
    #[cfg(target_os = "macos")]
    fn the_keychain_holds_one_master_key_per_home() {
        if std::env::var_os("COPILOT_CANVAS_KEYCHAIN_TESTS").is_none() {
            eprintln!("skipped: set COPILOT_CANVAS_KEYCHAIN_TESTS=1 to use the login Keychain");
            return;
        }
        use super::unix::{Store, open_in, seal_in};
        let service = format!("Copilot Canvas test {}", uuid::Uuid::new_v4().simple());
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
}
