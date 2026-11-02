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

/// Renames `from` to `to`, never over anything at `to`, also something that
/// appeared after a look (atomic): macOS `renamex_np(RENAME_EXCL)`, Linux
/// `renameat2(RENAME_NOREPLACE)`; where a file system has neither, a file is
/// linked to its new name (which fails when the name is taken) and then
/// unlinked, a folder renamed after a look. `AlreadyExists` when taken. A
/// rename to the same file under another case (a case-blind file system:
/// APFS, HFS+) renames it. (Windows: `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`, in the notebook's backend.)
#[cfg(unix)]
pub fn rename_no_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::ffi::OsStrExt;
    if same_file(from, to) {
        return std::fs::rename(from, to);
    }
    let c = |p: &std::path::Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| Error::new(ErrorKind::InvalidInput, "a NUL in a path"));
    let (cfrom, cto) = (c(from)?, c(to)?);
    // SAFETY: NUL-terminated paths that live across the calls.
    #[cfg(target_os = "macos")]
    let done = unsafe { libc::renamex_np(cfrom.as_ptr(), cto.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let done = unsafe { libc::renameat2(libc::AT_FDCWD, cfrom.as_ptr(), libc::AT_FDCWD, cto.as_ptr(), libc::RENAME_NOREPLACE) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let done = {
        let _ = (&cfrom, &cto);
        -1
    };
    if done == 0 {
        return Ok(());
    }
    let error = Error::last_os_error();
    // Not supported here (a file system without it, an old kernel): the portable way.
    let unsupported = matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)) || cfg!(not(any(target_os = "macos", target_os = "linux")));
    if !unsupported {
        return Err(error);
    }
    if std::fs::symlink_metadata(from)?.is_dir() {
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(ErrorKind::AlreadyExists.into());
        }
        return std::fs::rename(from, to);
    }
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

/// `a` and `b` are the same file (device and inode; links not followed):
/// two names of one file on a case-blind file system.
#[cfg(unix)]
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// A file whose content is not on this Mac (iCloud Drive's "optimize
/// storage" leaves a dataless placeholder; reading it downloads it): the
/// notebook's listings and search leave its content alone, as on Windows
/// with OneDrive's placeholders. Never on Linux.
#[cfg(unix)]
pub fn cloud_only(meta: &std::fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        /// `SF_DATALESS` of `<sys/stat.h>`.
        const SF_DATALESS: u32 = 0x4000_0000;
        meta.st_flags() & SF_DATALESS != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = meta;
        false
    }
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

    #[test]
    fn renames_never_replace_and_case_changes_rename() {
        let dir = std::env::temp_dir().join(format!("cs-mv-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        assert_eq!(super::rename_no_replace(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!((std::fs::read_to_string(&a).unwrap(), std::fs::read_to_string(&b).unwrap()), ("a".into(), "b".into()), "nothing replaced");
        assert_eq!(super::rename_no_replace(&dir.join("folder"), &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists, "a folder neither");
        let c = dir.join("c.md");
        super::rename_no_replace(&a, &c).unwrap();
        assert!(!a.exists() && std::fs::read_to_string(&c).unwrap() == "a");
        // A case change: renamed where the file system ignores case, a new name where it does not.
        let upper = dir.join("C.md");
        super::rename_no_replace(&c, &upper).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"C.md".to_owned()) && !names.contains(&"c.md".to_owned()), "{names:?}");
        assert!(!super::same_file(&b, &upper) && super::same_file(&upper, &upper));
        assert!(!super::cloud_only(&std::fs::metadata(&upper).unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
