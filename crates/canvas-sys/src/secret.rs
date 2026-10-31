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
pub use unix::*;

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

    /// The home's master key, created on first use.
    fn master_key(home: &Path) -> io::Result<Vec<u8>> {
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
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => master_key(home),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Seals `data` with the home's master key.
    pub fn seal(home: &Path, data: &[u8], _description: &str) -> io::Result<String> {
        let key = master_key(home)?;
        let (nonce, sealed) = canvas_common::pairing::seal_for_client(&key, LABEL, data);
        Ok(format!("{PREFIX}{}{}", hex(&nonce), hex(&sealed)))
    }

    /// Opens what [`seal`] produced with the same home's master key.
    pub fn open(home: &Path, text: &str) -> io::Result<Vec<u8>> {
        let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
        let body = text.strip_prefix(PREFIX).ok_or_else(|| invalid("not a secret sealed on macOS or Linux (a Windows DPAPI blob?)"))?;
        let bytes = unhex(body).ok_or_else(|| invalid("a sealed secret is not hex"))?;
        if bytes.len() < NONCE_LEN {
            return Err(invalid("a sealed secret is too short"));
        }
        let (nonce, sealed) = bytes.split_at(NONCE_LEN);
        let key = master_key(home)?;
        canvas_common::pairing::open_for_client(&key, LABEL, nonce, sealed).ok_or_else(|| invalid("cannot open a sealed secret (another data home's, or changed)"))
    }

    /// Where secrets are kept (`doctor`).
    pub fn backend(home: &Path) -> String {
        format!("master key file {} (0600)", key_path(home).display())
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
}
