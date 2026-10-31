//! Private files: created for this user only (Windows: a protected DACL
//! naming only the user's SID; Unix: mode 0600, created exclusively).

#[cfg(windows)]
pub use canvas_common::win::{create_private, write_private};

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    /// Creates a new file only this user can read or write (0600); fails when it exists.
    pub fn create_private(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
    }

    /// Writes `path` atomically with mode 0600: a private temporary file
    /// (`NAME.<random>.tmp` next to it) renamed over it.
    pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?.to_string_lossy().into_owned();
        let temp = path.with_file_name(format!("{name}.{}.tmp", canvas_common::paths::hex(&canvas_common::protocol::nonce()[..8])));
        let written = create_private(&temp).and_then(|mut file| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, path)
        });
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written
    }

    /// Creates `dir` (and its parents) and makes it this user's only (0700).
    /// Refuses a folder another user owns.
    pub fn private_dir(dir: &Path) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let meta = std::fs::symlink_metadata(dir)?;
        // SAFETY: geteuid cannot fail.
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not a folder of this user", dir.display())));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Where the folder browser starts: the drives (Windows).
#[cfg(windows)]
pub fn roots() -> Vec<String> {
    // SAFETY: no arguments; returns a bit mask.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i) as char)).collect()
}

/// Where the folder browser starts: `/`, the home folder, then mounted
/// volumes (macOS `/Volumes/*`; Linux `/media/$USER/*`, `/run/media/$USER/*`, `/mnt/*`).
#[cfg(unix)]
pub fn roots() -> Vec<String> {
    let mut roots = vec!["/".to_owned()];
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        roots.push(std::path::PathBuf::from(home).display().to_string());
    }
    let user = crate::host::user_name();
    let mounts: Vec<std::path::PathBuf> = match crate::host::platform() {
        crate::host::Platform::MacOs => vec!["/Volumes".into()],
        _ => vec![format!("/media/{user}").into(), format!("/run/media/{user}").into(), "/mnt".into()],
    };
    for parent in mounts {
        let Ok(entries) = std::fs::read_dir(&parent) else { continue };
        let mut found: Vec<String> = entries.flatten().filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path().display().to_string()).collect();
        found.sort();
        roots.extend(found);
    }
    roots
}

/// Removes macOS's `com.apple.quarantine` attribute from a file this program
/// copied (the per-build service copy), so Gatekeeper does not ask again for
/// a build the user already opened. Nothing to do elsewhere.
pub fn strip_quarantine(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
        // SAFETY: NUL-terminated path and attribute name.
        unsafe { libc::removexattr(path.as_ptr(), c"com.apple.quarantine".as_ptr(), 0) };
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
}

/// Creates `dir` (and its parents); on Windows folders inherit the profile's ACL.
#[cfg(windows)]
pub fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_files_are_0600_and_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!("cs-fs-{}", uuid::Uuid::new_v4().simple()));
        super::private_dir(&dir).unwrap();
        let path = dir.join("service-x.token");
        super::write_private(&path, b"one").unwrap();
        super::write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file is left behind");
        assert!(super::create_private(&path).is_err(), "never opens an existing file");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
