//! Private files: created for this user only (Windows: a protected DACL
//! naming only the user's SID; Unix: mode 0600, created exclusively), and
//! renames that never replace a file (Unix).

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

    /// Renames `from` to `to`, never over something at `to` (`AlreadyExists`
    /// then), also something that appeared a moment before, atomic in the
    /// kernel: macOS `renamex_np(RENAME_EXCL)`, Linux
    /// `renameat2(RENAME_NOREPLACE)`. File systems that refuse the flag (some
    /// FUSE, NFS, SMB and older kernels: `EINVAL`, `ENOSYS`, `ENOTSUP`,
    /// `EOPNOTSUPP`) get [`rename_claimed`]. A rename onto itself changes
    /// nothing; a case change of one file (a case-blind file system: APFS,
    /// HFS+) renames it. (Windows: `MoveFileExW` without
    /// `MOVEFILE_REPLACE_EXISTING`, in the notebook's backend.)
    pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        if same_file(from, to) && from.to_string_lossy().to_lowercase() == to.to_string_lossy().to_lowercase() {
            return std::fs::rename(from, to);
        }
        let c = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
        let (old, new) = (c(from)?, c(to)?);
        // SAFETY: two NUL-terminated paths that live across the call.
        #[cfg(target_os = "macos")]
        let done = unsafe { libc::renamex_np(old.as_ptr(), new.as_ptr(), libc::RENAME_EXCL) };
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        let done = unsafe { libc::renameat2(libc::AT_FDCWD, old.as_ptr(), libc::AT_FDCWD, new.as_ptr(), libc::RENAME_NOREPLACE) };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let done: libc::c_int = {
            return rename_claimed(from, to);
        };
        if done == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code) => rename_claimed(from, to),
            _ => Err(error),
        }
    }

    /// `a` and `b` are the same file (device and inode; links not followed):
    /// two names of one file on a case-blind file system.
    pub fn same_file(a: &Path, b: &Path) -> bool {
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

    /// A no-replace rename without the kernel's help: a file's new name is
    /// claimed with a hard link (which fails when the name is taken), then the
    /// old name goes; folders, and filesystems without hard links, are checked
    /// and renamed (a name taken in between is the only gap).
    pub fn rename_claimed(from: &Path, to: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(from)?.is_file() {
            match std::fs::hard_link(from, to) {
                Ok(()) => {
                    return std::fs::remove_file(from).inspect_err(|_| {
                        // Not both names: the new one goes again.
                        let _ = std::fs::remove_file(to);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error),
                Err(_) => {}
            }
        }
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        std::fs::rename(from, to)
    }

    #[cfg(test)]
    mod rename_tests {
        use super::*;

        fn folder(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("canvas-sys-rename-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn renames_never_replace() {
            for (name, rename) in [("kernel", rename_no_replace as fn(&Path, &Path) -> io::Result<()>), ("claimed", rename_claimed)] {
                let dir = folder(name);
                let (a, b, c) = (dir.join("a.md"), dir.join("b.md"), dir.join("c.md"));
                std::fs::write(&a, "a").unwrap();
                std::fs::write(&b, "b").unwrap();
                assert_eq!(rename(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "{name}");
                assert_eq!((std::fs::read_to_string(&a).unwrap(), std::fs::read_to_string(&b).unwrap()), ("a".into(), "b".into()), "{name}: both kept");
                rename(&a, &c).unwrap();
                assert!(!a.exists() && std::fs::read_to_string(&c).unwrap() == "a", "{name}");
                // Folders too, also onto an empty folder (rename(2) would replace it).
                let (x, y) = (dir.join("x"), dir.join("y"));
                std::fs::create_dir_all(x.join("inner")).unwrap();
                std::fs::create_dir_all(&y).unwrap();
                assert_eq!(rename(&x, &y).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "{name}: an empty folder is not replaced");
                rename(&x, &dir.join("z")).unwrap();
                assert!(dir.join("z").join("inner").is_dir(), "{name}");
                // A link at the target counts as taken, even one that leads nowhere.
                std::os::unix::fs::symlink(dir.join("missing"), dir.join("link")).unwrap();
                assert_eq!(rename(&c, &dir.join("link")).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "{name}: a dangling link");
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
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

/// The file system holding `path`: (bytes free for this user, bytes in all).
pub fn space(path: &std::path::Path) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path with NUL"))?;
        // SAFETY: a valid C string and a zeroed statvfs the call fills.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let unit = stat.f_frsize as u64;
        Ok((stat.f_bavail as u64 * unit, stat.f_blocks as u64 * unit))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: a NUL-terminated wide string and two u64 the call fills.
        if unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((free, total))
    }
}

#[cfg(test)]
mod space_tests {
    #[test]
    fn the_temp_folder_has_room() {
        let (free, total) = super::space(&std::env::temp_dir()).unwrap();
        assert!(total > 0 && free <= total, "{free} of {total}");
    }
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
    fn case_changes_rename_and_links_are_not_the_same_name() {
        let dir = std::env::temp_dir().join(format!("cs-mv-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (b, c) = (dir.join("b.md"), dir.join("c.md"));
        std::fs::write(&b, "b").unwrap();
        std::fs::write(&c, "c").unwrap();
        super::rename_no_replace(&c, &c).unwrap();
        assert!(c.exists(), "a rename onto itself changes nothing");
        // A second name of one file is taken all the same.
        let link = dir.join("link.md");
        std::fs::hard_link(&b, &link).unwrap();
        assert!(super::same_file(&b, &link));
        assert_eq!(super::rename_no_replace(&b, &link).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert!(b.exists() && link.exists());
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
