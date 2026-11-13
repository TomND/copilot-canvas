//! The notebook's files on the backend's machine (docs/NOTEBOOK-TASKS.md
//! §2.3): listings with provenance, reads with revisions, atomic writes that
//! never overwrite someone else's change, folders, renames, the trash and
//! search. Also the "never overwrite" file creation `atelier note new`
//! and report filing use.
//!
//! Reads take any absolute path (as `ReadText` does: a client can do what
//! you can on this machine). Writes stay inside the roots the caller names:
//! absolute paths, no `..`, never inside `.git`, on Windows no name Windows
//! would read as another (`x.`, `a:stream`, `CON`, `GIT~1`), and no link or
//! junction leading out of the roots. The checks run right before the change
//! (a folder swapped for a link in between is not caught: they keep mistakes
//! inside the notebook, the client is trusted); renames never replace a file
//! that appeared meanwhile.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use canvas_model::notebook::{self, Entry, Hit, Info, Kind, Meta, NoteText, Place};
use canvas_model::rpc::Response;

/// Files `NoteRevs` answers at once.
const MAX_REVS: usize = 500;
/// Names read from one folder before sorting (the first 5,000 are answered).
const MAX_SCAN: usize = 50_000;
/// Search: largest file read, files looked at, bytes read and time spent at most.
const SEARCH_FILE: u64 = 2 << 20;
const SEARCH_FILES: usize = 20_000;
const SEARCH_BYTES: u64 = 256 << 20;
const SEARCH_TIME: Duration = Duration::from_secs(10);
/// Folders search never enters.
const SEARCH_SKIP: &[&str] = &[notebook::TRASH, ".git", "node_modules"];

/// Writes of this process, one at a time (the revision check and the write).
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn checked(path: &str) -> Result<PathBuf> {
    notebook::checked_path(path).map_err(|e| anyhow!(e))
}

/// [`notebook::writable`], and on Windows no component below its root that
/// Windows would read as another name ([`windows_name_problem`]).
fn writable(path: &str, roots: &[String]) -> Result<PathBuf> {
    let buf = notebook::writable(path, roots).map_err(|e| anyhow!(e))?;
    if cfg!(windows) {
        // Below the deepest root holding it (the roots' own names are the user's).
        let depth = roots.iter().filter_map(|r| notebook::checked_path(r).ok()).filter(|r| notebook::inside(&buf, r)).map(|r| r.components().count()).max().unwrap_or(0);
        for part in buf.components().skip(depth) {
            let name = part.as_os_str().to_string_lossy();
            if let Some(why) = windows_name_problem(&name) {
                bail!("{}: Windows cannot write {name:?} as named ({why})", buf.display());
            }
        }
    }
    Ok(buf)
}

/// Why Windows would not write a path component as named: it drops trailing
/// dots and spaces (`.git.` is `.git`), `:` names an alternate data stream,
/// device names are devices (`nul.md`), `GIT~1` may be `.git`'s short name,
/// and `<>"/\|?*` and control characters are no part of a name.
fn windows_name_problem(name: &str) -> Option<&'static str> {
    if name.chars().any(|c| (c as u32) < 32 || r#"<>:"/\|?*"#.contains(c)) {
        Some("it has one of <>:\"/\\|?* or a control character")
    } else if name.ends_with(['.', ' ']) {
        Some("Windows drops trailing dots and spaces")
    } else if notebook::is_device_name(name) {
        Some("a device name")
    } else if name.to_ascii_lowercase().strip_prefix("git~").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
        Some("it may be the short name of .git")
    } else {
        None
    }
}

/// The part of `path` that exists, links and junctions followed, is inside
/// one of `roots` (resolved alike): a link inside the notebook never leads a
/// change out of it. `follow`: `path` itself counts when it is a link (a
/// write changes its target); else only its folder (a rename or the trash
/// moves the link itself). Paths are resolved only when a link sits below
/// the root (a root that is a link is the user's choice; some drives cannot
/// resolve paths at all).
fn resolved_inside(path: &Path, roots: &[String], follow: bool) -> Result<()> {
    let roots: Vec<PathBuf> = roots.iter().filter_map(|r| notebook::checked_path(r).ok()).collect();
    let Some(depth) = roots.iter().filter(|r| notebook::inside(path, r)).map(|r| r.components().count()).max() else { return Ok(()) };
    let mut existing = if follow { path } else { path.parent().unwrap_or(path) };
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return Ok(()),
        }
    }
    let linked = existing.ancestors().take_while(|a| a.components().count() > depth).any(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()));
    if !linked {
        return Ok(());
    }
    let real = std::fs::canonicalize(existing).map_err(|e| anyhow!("{} cannot be resolved (a link that leads nowhere?): {e}", existing.display()))?;
    let inside = roots.iter().filter_map(|root| std::fs::canonicalize(root).ok()).any(|root| notebook::inside(&real, &root));
    anyhow::ensure!(inside, "{} leads out of the notebook's folders (a link or junction to {})", path.display(), real.display());
    Ok(())
}

/// Access denied, or on Windows a sharing or lock violation, elsewhere a busy
/// file: usually another program (an antivirus, an indexer, a sync client)
/// holding the file a moment.
fn held(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy) || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)))
}

/// A file operation, tried again for about 0.6 s while the file is [`held`].
fn retrying<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempt = 0;
    loop {
        match op() {
            Err(error) if attempt < 5 && held(&error) => {
                std::thread::sleep(Duration::from_millis(20 << attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Renames `from` to `to`, never over a file there, also one that appeared
/// after the checks (Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`).
fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    retrying(|| move_once(from, to))
}

#[cfg(windows)]
fn move_once(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Through the resolved folders: `\\?\` paths, so long paths work too (as
    // given where a drive cannot resolve them).
    let wide = |path: &Path| -> std::io::Result<Vec<u16>> {
        let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else { return Err(std::io::Error::other(format!("{} has no folder", path.display()))) };
        let folder = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
        Ok(folder.join(name).as_os_str().encode_wide().chain(Some(0)).collect())
    };
    let (from, to) = (wide(from)?, wide(to)?);
    // SAFETY: two NUL-terminated UTF-16 paths that live across the call.
    if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// macOS and Linux: an atomic no-replace rename (`renamex_np` /
/// `renameat2`; a link-claimed or checked rename where the file system
/// refuses the flag). A rename onto itself changes nothing; a case change on
/// a case-blind file system (APFS) renames.
#[cfg(not(windows))]
fn move_once(from: &Path, to: &Path) -> std::io::Result<()> {
    canvas_sys::fs::rename_no_replace(from, to)
}

/// `Request::NotebookInfo`: the root (created with its three folders), the
/// canvas-wide folder and the home.
pub(crate) fn info(home: &Path, root: Option<&str>, canvas: Option<&str>) -> Result<Response> {
    let root = match root.map(str::trim).filter(|r| !r.is_empty()) {
        Some(root) => checked(root).context("the notebook folder")?,
        None => notebook::root_of(None, home),
    };
    for folder in [notebook::GROUPS, notebook::INBOX, notebook::TODOS] {
        let path = root.join(folder);
        std::fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;
    }
    let canvas_dir = canvas.filter(|id| canvas_model::doc::valid_canvas_id(id)).map(|id| canvas_model::canvas_wide::canvas_dir(home, id).display().to_string());
    Ok(Response::NotebookInfo(Info { root: root.display().to_string(), canvas_dir, home: home.display().to_string() }))
}

/// Reads at most `max` bytes from the start of a file.
fn head(path: &Path, max: usize) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(max as u64).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// A file whose content is in the cloud only (OneDrive and other sync
/// clients' placeholders): listings and search leave its content alone, as
/// reading it would download it. Opening it reads it.
fn cloud_only(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        cloud_attributes(meta.file_attributes())
    }
    #[cfg(not(windows))]
    {
        canvas_sys::fs::cloud_only(meta)
    }
}

/// `FILE_ATTRIBUTE_OFFLINE`, `_RECALL_ON_OPEN` or `_RECALL_ON_DATA_ACCESS`.
#[cfg_attr(not(windows), allow(dead_code))]
fn cloud_attributes(attributes: u32) -> bool {
    attributes & (0x1000 | 0x4_0000 | 0x40_0000) != 0
}

/// The place a root is: the canvas-wide folder when it is inside the
/// service's canvas-agents folder.
fn place_of(home: &Path, root: &Path) -> Place {
    if notebook::inside(root, &home.join("canvas-agents")) { Place::CanvasWide } else { Place::Notes }
}

/// One entry: its metadata (Markdown and HTML: from the first 8 KiB), title and provenance.
pub fn entry(path: &Path, rel: &str, place: Place, meta: &std::fs::Metadata) -> Entry {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if meta.is_dir() {
        return Entry {
            title: name.clone(),
            name,
            path: path.display().to_string(),
            rel: rel.to_owned(),
            kind: Kind::Folder,
            size: 0,
            modified_ms: modified_ms(meta),
            provenance: notebook::classify(&format!("{rel}/"), &Meta::default(), place),
        };
    }
    let kind = Kind::of(&name);
    // Only regular files are read (a pipe named `x.md` would never answer), and
    // no placeholder of a file in the cloud (classified by its folder instead).
    let text = (meta.is_file() && !cloud_only(meta) && matches!(kind, Kind::Markdown | Kind::Html)).then(|| head(path, notebook::META_BYTES)).flatten().unwrap_or_default();
    let note_meta = notebook::note_meta(kind, &text);
    Entry {
        title: notebook::title(kind, &text, &note_meta, &name),
        provenance: notebook::classify(rel, &note_meta, place),
        name,
        path: path.display().to_string(),
        rel: rel.to_owned(),
        kind,
        size: meta.len(),
        modified_ms: modified_ms(meta),
    }
}

/// `Request::NotebookList`.
pub(crate) fn list(home: &Path, dir: &str, root: Option<&str>) -> Result<Response> {
    let dir = checked(dir)?;
    let root = match root {
        Some(root) => checked(root)?,
        None => dir.clone(),
    };
    let base = notebook::rel_path(&dir, &root).with_context(|| format!("{} is not inside {}", dir.display(), root.display()))?;
    let place = place_of(home, &root);
    let mut found: Vec<(bool, String, PathBuf)> = Vec::new();
    for item in std::fs::read_dir(&dir).map_err(|e| anyhow!("cannot list {}: {e}", dir.display()))?.flatten().take(MAX_SCAN) {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('$') || name == "System Volume Information" {
            continue;
        }
        let path = item.path();
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        found.push((is_dir, name, path));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())).then_with(|| a.1.cmp(&b.1)));
    found.truncate(notebook::MAX_ENTRIES);
    let entries = found
        .into_iter()
        .filter_map(|(_, name, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            let rel = if base.is_empty() { name } else { format!("{base}/{name}") };
            Some(entry(&path, &rel, place, &meta))
        })
        .collect();
    Ok(Response::NoteEntries(entries))
}

/// A note's bytes (refused over [`notebook::MAX_NOTE`]: saving a cut text would lose the rest).
fn read_note(path: &Path) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).with_context(|| format!("cannot open {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} is not a file", path.display());
    anyhow::ensure!(
        meta.len() <= notebook::MAX_NOTE,
        "{} is {:.1} MiB; the notebook opens notes of at most {} MiB",
        path.display(),
        meta.len() as f64 / (1u64 << 20) as f64,
        notebook::MAX_NOTE >> 20
    );
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

fn note_text(bytes: &[u8]) -> NoteText {
    NoteText { text: String::from_utf8_lossy(bytes).into_owned(), rev: notebook::rev_of(bytes) }
}

/// `Request::NoteRead`.
pub(crate) fn read(path: &str) -> Result<Response> {
    Ok(Response::Note(note_text(&read_note(&checked(path)?)?)))
}

/// Writes `bytes` over `path` atomically: a hidden temporary file in the same
/// folder renamed over it (tried again while another program holds it; the
/// temporary file never stays behind).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().with_context(|| format!("{} has no folder", path.display()))?;
    // A long name is cut: the temporary file's name must fit too.
    let name: String = path.file_name().map(|n| n.to_string_lossy().chars().take(64).collect()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.{}.tmp", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let written = std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display())).and_then(|()| replace(&temp, path).with_context(|| format!("cannot write {}", path.display())));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Renames `temp` over `path`, tried again while either is [`held`] (an
/// antivirus scanning the new temporary file: a sharing violation).
fn replace(temp: &Path, path: &Path) -> std::io::Result<()> {
    retrying(|| std::fs::rename(temp, path))
}

/// `Request::NoteWrite`.
pub(crate) fn write(path: &str, text: &str, base: Option<&str>, roots: &[String]) -> Result<Response> {
    let path = writable(path, roots)?;
    anyhow::ensure!(text.len() as u64 <= notebook::MAX_NOTE, "a note is at most {} MiB", notebook::MAX_NOTE >> 20);
    let _one = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    resolved_inside(&path, roots, true)?;
    // A link to a note: its file changes, the link stays.
    let path = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&path).with_context(|| format!("cannot resolve {}", path.display()))?,
        _ => path,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    match base {
        None => match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(claimed) => {
                drop(claimed);
                if let Err(error) = write_atomic(&path, text.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Someone else's file: what is there now.
                return Ok(Response::NoteConflict(note_text(&read_note(&path)?)));
            }
            Err(error) => return Err(error).with_context(|| format!("cannot create {}", path.display())),
        },
        Some(base) => {
            anyhow::ensure!(path.exists(), "{} was moved or deleted; save it as a new note", path.display());
            let current = read_note(&path)?;
            if notebook::rev_of(&current) != base {
                return Ok(Response::NoteConflict(note_text(&current)));
            }
            // Read with its invalid bytes replaced: saving the text would lose them.
            anyhow::ensure!(
                std::str::from_utf8(&current).is_ok(),
                "{} is not UTF-8 text: the notebook shows it but never saves it (its other bytes would be lost); change it in another editor",
                path.display()
            );
            write_atomic(&path, text.as_bytes())?;
        }
    }
    Ok(Response::NoteSaved { rev: notebook::rev_of(text.as_bytes()) })
}

/// Revisions of files as last read, by path: (size, modified time, rev).
/// Open notes are polled every 2 s in every window: an unchanged file
/// (same size and time) is not read and hashed again (S8-07).
static REVS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, RevStamp>>> = std::sync::LazyLock::new(Default::default);
/// A file's size and time, and its revision then.
type RevStamp = (u64, std::time::SystemTime, String);
/// Files whose revision is remembered at most (beyond, it starts over).
const MAX_REMEMBERED_REVS: usize = 2_048;

/// The revision of a file, read and hashed only when its size or time changed.
fn file_rev(path: &Path, meta: &std::fs::Metadata) -> Option<String> {
    let modified = meta.modified().ok();
    if let Some(modified) = modified
        && let Some((len, at, rev)) = REVS.lock().unwrap_or_else(|e| e.into_inner()).get(path)
        && *len == meta.len()
        && *at == modified
    {
        return Some(rev.clone());
    }
    let rev = notebook::rev_of(&std::fs::read(path).ok()?);
    if let Some(modified) = modified {
        let mut revs = REVS.lock().unwrap_or_else(|e| e.into_inner());
        if revs.len() >= MAX_REMEMBERED_REVS {
            revs.clear();
        }
        revs.insert(path.to_path_buf(), (meta.len(), modified, rev.clone()));
    }
    Some(rev)
}

/// `Request::NoteRevs`.
pub(crate) fn revs(paths: &[String]) -> Result<Response> {
    anyhow::ensure!(paths.len() <= MAX_REVS, "at most {MAX_REVS} files at once");
    let rev = |path: &String| -> Option<String> {
        let path = notebook::checked_path(path).ok()?;
        let meta = std::fs::metadata(&path).ok()?;
        (meta.is_file() && meta.len() <= notebook::MAX_BYTES).then(|| file_rev(&path, &meta)).flatten()
    };
    Ok(Response::NoteRevs(paths.iter().map(rev).collect()))
}

/// `Request::ReadBytes`.
pub(crate) fn read_bytes(path: &str, max_bytes: u64) -> Result<Response> {
    let path = checked(path)?;
    let max = max_bytes.min(notebook::MAX_BYTES);
    let meta = std::fs::metadata(&path).with_context(|| format!("cannot open {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} is not a file", path.display());
    anyhow::ensure!(meta.len() <= max, "{} is larger than {max} bytes", path.display());
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(&path).and_then(|f| f.take(max).read_to_end(&mut bytes)).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Response::Bytes(bytes))
}

/// `Request::NoteMkdir`.
pub(crate) fn mkdir(path: &str, roots: &[String]) -> Result<Response> {
    let path = writable(path, roots)?;
    let _one = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    anyhow::ensure!(std::fs::symlink_metadata(&path).is_err(), "{} already exists", path.display());
    resolved_inside(&path, roots, true)?;
    std::fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;
    Ok(Response::Done)
}

/// The same file: the same path (case-blind on Windows), or on macOS and
/// Linux the same file under two names (a case-blind file system: APFS).
fn same_path(a: &Path, b: &Path) -> bool {
    (notebook::inside(a, b) && notebook::inside(b, a)) || (cfg!(unix) && same_inode(a, b))
}

#[cfg(unix)]
fn same_inode(a: &Path, b: &Path) -> bool {
    canvas_sys::fs::same_file(a, b)
}

#[cfg(not(unix))]
fn same_inode(_a: &Path, _b: &Path) -> bool {
    false
}

/// `Request::NoteRename`.
pub(crate) fn rename(from: &str, to: &str, roots: &[String]) -> Result<Response> {
    let (from, to) = (writable(from, roots)?, writable(to, roots)?);
    let _one = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::symlink_metadata(&from).with_context(|| format!("{} does not exist", from.display()))?;
    // A rename that only changes case is the same file on Windows.
    let taken = || anyhow!("{} already exists", to.display());
    if std::fs::symlink_metadata(&to).is_ok() && !same_path(&from, &to) {
        return Err(taken());
    }
    anyhow::ensure!(same_path(&from, &to) || !notebook::inside(&to, &from), "a folder cannot move into itself");
    resolved_inside(&from, roots, false)?;
    resolved_inside(&to, roots, false)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    match move_no_replace(&from, &to) {
        Ok(()) => Ok(Response::Done),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(taken()),
        Err(error) => Err(error).with_context(|| format!("cannot move {} to {}", from.display(), to.display())),
    }
}

/// `Request::NoteTrash`: into `<root>/.trash/<ms>-<name>` (`<ms>-<n>-<name>` when taken).
pub(crate) fn trash(path: &str, root: &str) -> Result<Response> {
    let root = checked(root)?;
    let roots = [root.display().to_string()];
    let path = writable(path, &roots)?;
    let bin = root.join(notebook::TRASH);
    anyhow::ensure!(!notebook::inside(&path, &bin), "{} is already in the trash", path.display());
    let _one = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::symlink_metadata(&path).with_context(|| format!("{} does not exist", path.display()))?;
    resolved_inside(&path, &roots, false)?;
    resolved_inside(&bin, &roots, true)?;
    std::fs::create_dir_all(&bin).with_context(|| format!("cannot create {}", bin.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ms = crate::paths::now_ms();
    for n in 1..1_000 {
        let target = if n == 1 { bin.join(format!("{ms}-{name}")) } else { bin.join(format!("{ms}-{n}-{name}")) };
        match move_no_replace(&path, &target) {
            Ok(()) => return Ok(Response::Done),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("cannot move {} to the trash", path.display())),
        }
    }
    bail!("no free name in the trash")
}

/// A line around its match (`at`: a byte offset), at most 200 characters.
fn snippet(line: &str, at: usize) -> String {
    let line = line.trim_end();
    let at = if at <= line.len() && line.is_char_boundary(at) { at } else { 0 };
    let chars: Vec<char> = line.chars().collect();
    let at = line[..at].chars().count();
    let start = if chars.len() <= 200 { 0 } else { at.saturating_sub(60).min(chars.len() - 200) };
    let text: String = chars[start..chars.len().min(start + 200)].iter().collect();
    let text = if start == 0 { text.trim_start().to_owned() } else { format!("…{}", text.trim_start()) };
    if start + 200 < chars.len() { format!("{text}…") } else { text }
}

struct Search {
    query: String,
    limit: usize,
    hits: Vec<Hit>,
    files: usize,
    bytes: u64,
    deadline: Instant,
}

impl Search {
    fn done(&self) -> bool {
        self.hits.len() >= self.limit || self.files >= SEARCH_FILES || self.bytes >= SEARCH_BYTES || Instant::now() >= self.deadline
    }

    fn folder(&mut self, dir: &Path, depth: usize) {
        if depth > 40 || self.done() {
            return;
        }
        let Ok(items) = std::fs::read_dir(dir) else { return };
        let mut items: Vec<(String, PathBuf, std::fs::FileType)> = items.flatten().take(MAX_SCAN).filter_map(|i| Some((i.file_name().to_string_lossy().into_owned(), i.path(), i.file_type().ok()?))).collect();
        items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        for (name, path, kind) in items {
            if self.done() {
                return;
            }
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !SEARCH_SKIP.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    self.folder(&path, depth + 1);
                }
                continue;
            }
            if !kind.is_file() {
                // Pipes, sockets, devices: never opened.
                continue;
            }
            self.files += 1;
            let path_text = path.display().to_string();
            if name.to_lowercase().contains(&self.query) {
                self.hits.push(Hit { path: path_text.clone(), line: 0, text: name.clone() });
            }
            let meta = std::fs::metadata(&path).ok();
            let size = meta.as_ref().map_or(u64::MAX, |m| m.len());
            if !Kind::of(&name).is_text() || size > SEARCH_FILE || meta.as_ref().is_some_and(cloud_only) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            self.bytes += bytes.len() as u64;
            let text = String::from_utf8_lossy(&bytes);
            for (n, line) in text.lines().enumerate() {
                if self.hits.len() >= self.limit {
                    return;
                }
                if let Some(at) = line.to_lowercase().find(&self.query) {
                    // `at` is in the lower-case line; map it back (lengths can differ outside ASCII).
                    let at = if line.len() == line.to_lowercase().len() { at } else { 0 };
                    self.hits.push(Hit { path: path_text.clone(), line: n as u32 + 1, text: snippet(line, at) });
                }
            }
        }
    }
}

/// `Request::NoteSearch`: roots inside other roots are searched once.
pub(crate) fn search(roots: &[String], query: &str, limit: u32) -> Result<Response> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Response::NoteHits(Vec::new()));
    }
    let roots: Vec<PathBuf> = roots.iter().map(|r| checked(r)).collect::<Result<_>>()?;
    let mut search = Search { query, limit: limit.clamp(1, notebook::MAX_HITS) as usize, hits: Vec::new(), files: 0, bytes: 0, deadline: Instant::now() + SEARCH_TIME };
    for (i, root) in roots.iter().enumerate() {
        let nested = roots.iter().enumerate().any(|(j, other)| j != i && notebook::inside(root, other) && (!same_path(root, other) || j < i));
        if !nested {
            search.folder(root, 0);
        }
    }
    Ok(Response::NoteHits(search.hits))
}

/// Writes a new file `<stem>.<ext>` in `dir` (created), else `<stem>-2.<ext>`,
/// `<stem>-3.<ext>`, …: never over an existing file. Answers its path.
pub fn create_new(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for n in 1..10_000 {
        let candidate = if n == 1 { dir.join(format!("{stem}.{ext}")) } else { dir.join(format!("{stem}-{n}.{ext}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(claimed) => {
                drop(claimed);
                if let Err(error) = write_atomic(&candidate, bytes) {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("cannot create {}", candidate.display())),
        }
    }
    bail!("no free name for {stem}.{ext} in {}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Ctx, handle};
    use canvas_model::notebook::{Audience, Author, Saved, Source};
    use canvas_model::rpc::{Backend, Request};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-notebook-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn s(path: &Path) -> String {
        path.display().to_string()
    }

    fn backend(home: &Path) -> crate::backend::LocalBackend {
        crate::backend::LocalBackend::new(Ctx { home: home.to_owned(), sessions_dir: home.join("sessions") })
    }

    #[test]
    fn the_root_is_created_with_its_folders_and_the_canvas_folder_is_named() {
        let home = temp("info");
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        let info = backend.notebook_info(None, Some("c-1")).unwrap();
        assert_eq!(PathBuf::from(&info.root), home.join("notebook"));
        assert_eq!(info.canvas_dir.as_deref().map(PathBuf::from), Some(home.join("canvas-agents").join("c-1")));
        assert_eq!(PathBuf::from(&info.home), home);
        for folder in ["Groups", "Inbox", "Todos"] {
            assert!(home.join("notebook").join(folder).is_dir(), "{folder}");
        }
        let custom = home.join("elsewhere").join("Notes");
        let info = backend.notebook_info(Some(&s(&custom)), Some("../evil")).unwrap();
        assert_eq!((PathBuf::from(&info.root), info.canvas_dir), (custom.clone(), None), "a canvas id names a folder only when valid");
        assert!(custom.join("Inbox").is_dir());
        assert!(backend.notebook_info(Some("relative\\notes"), None).unwrap_err().to_string().contains("absolute"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn listings_have_folders_first_with_titles_and_provenance() {
        let home = temp("list");
        let root = home.join("notebook");
        let group = root.join("Groups").join("Team");
        std::fs::create_dir_all(group.join("ai-docs")).unwrap();
        std::fs::create_dir_all(group.join("human-outputs")).unwrap();
        std::fs::create_dir_all(group.join(".canvas")).unwrap();
        std::fs::write(group.join("ai-docs").join("plan.md"), "---\naudience: human\nagent: claude\n---\n# The plan\n").unwrap();
        std::fs::write(group.join("ai-docs").join("log.md"), "no heading").unwrap();
        std::fs::write(group.join("b.html"), "<html><head><meta name=\"canvas-author\" content=\"ai\"><title>Board</title></head></html>").unwrap();
        std::fs::write(group.join("A.txt"), "text").unwrap();
        std::fs::write(group.join(".canvas").join("events.yaml"), "x").unwrap();
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        let entries = backend.notebook_list(&s(&group), Some(&s(&root))).unwrap();
        let names: Vec<(&str, &str, Kind)> = entries.iter().map(|e| (e.name.as_str(), e.rel.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [
                (".canvas", "Groups/Team/.canvas", Kind::Folder),
                ("ai-docs", "Groups/Team/ai-docs", Kind::Folder),
                ("human-outputs", "Groups/Team/human-outputs", Kind::Folder),
                ("A.txt", "Groups/Team/A.txt", Kind::Text),
                ("b.html", "Groups/Team/b.html", Kind::Html)
            ]
        );
        let html = &entries[4];
        assert_eq!((html.title.as_str(), html.provenance.audience, html.provenance.source, html.provenance.author, html.size > 0), ("Board", Audience::Human, Source::FileType, Author::Ai, true));
        assert!(entries[0].provenance.audience == Audience::Ai && !entries[0].provenance.internal, ".canvas leads to passive memory");
        assert_eq!(entries[1].provenance.audience, Audience::Ai);
        let docs = backend.notebook_list(&s(&group.join("ai-docs")), Some(&s(&root))).unwrap();
        assert_eq!(docs.iter().map(|e| (e.title.as_str(), e.provenance.audience, e.provenance.inferred)).collect::<Vec<_>>(), [("log", Audience::Ai, true), ("The plan", Audience::Human, false)]);
        assert_eq!(docs[1].provenance.agent.as_deref(), Some("claude"));
        let internal = backend.notebook_list(&s(&group.join(".canvas")), Some(&s(&root))).unwrap();
        assert!(internal[0].provenance.internal, "events.yaml is internal");
        // Without a root, the folder is its own root; a canvas-agents folder is the canvas-wide one.
        let own = backend.notebook_list(&s(&group.join("ai-docs")), None).unwrap();
        assert_eq!((own[0].rel.as_str(), own[0].provenance.audience), ("log.md", Audience::Human), "classified from the folder down");
        let canvas = home.join("canvas-agents").join("c1");
        std::fs::create_dir_all(&canvas).unwrap();
        std::fs::write(canvas.join("GOALS.md"), "# Goals").unwrap();
        let wide = backend.notebook_list(&s(&canvas), None).unwrap();
        assert_eq!((wide[0].provenance.audience, wide[0].provenance.author), (Audience::Ai, Author::Ai));
        assert!(backend.notebook_list(&s(&group), Some(&s(&home.join("other")))).unwrap_err().to_string().contains("not inside"));
        assert!(backend.notebook_list(&s(&home.join("missing")), None).is_err());
        assert!(backend.notebook_list("relative", None).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn writes_check_the_base_revision_and_never_overwrite_a_new_note() {
        let home = temp("write");
        let root = home.join("notebook");
        let roots = vec![s(&root)];
        let note = root.join("Inbox").join("a.md");
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        let Saved::Written(rev) = backend.note_write(&s(&note), "# A\n", None, &roots).unwrap() else { panic!("written") };
        assert_eq!(rev, notebook::rev_of(b"# A\n"));
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "# A\n", "parent folders are created");
        // A new note over an existing file: nothing written, the current text answered.
        assert_eq!(backend.note_write(&s(&note), "other", None, &roots).unwrap(), Saved::Conflict(NoteText { text: "# A\n".into(), rev: rev.clone() }));
        let read = backend.note_read(&s(&note)).unwrap();
        assert_eq!((read.text.as_str(), read.rev.as_str()), ("# A\n", rev.as_str()));
        let Saved::Written(rev2) = backend.note_write(&s(&note), "# A2\n", Some(&rev), &roots).unwrap() else { panic!("saved on the base") };
        // Someone else changed it: the old base conflicts and nothing is written.
        std::fs::write(&note, "theirs").unwrap();
        let Saved::Conflict(current) = backend.note_write(&s(&note), "mine", Some(&rev2), &roots).unwrap() else { panic!("conflict") };
        assert_eq!((current.text.as_str(), std::fs::read_to_string(&note).unwrap().as_str()), ("theirs", "theirs"));
        backend.note_write(&s(&note), "mine", Some(&current.rev), &roots).unwrap();
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "mine");
        assert!(std::fs::read_dir(root.join("Inbox")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")), "no temporary files left");
        // Refused: outside the roots, `..`, .git, relative, too large, deleted under a base.
        let outside = home.join("x.md");
        assert!(backend.note_write(&s(&outside), "x", None, &roots).unwrap_err().to_string().contains("outside"));
        assert!(backend.note_write(&format!("{}\\..\\x.md", root.display()), "x", None, &roots).is_err());
        assert!(backend.note_write(&s(&root.join(".git").join("config")), "x", None, &roots).unwrap_err().to_string().contains(".git"));
        assert!(backend.note_write("Inbox\\a.md", "x", None, &roots).is_err());
        assert!(backend.note_write(&s(&note), &"x".repeat(notebook::MAX_NOTE as usize + 1), Some(&notebook::rev_of(b"mine")), &roots).unwrap_err().to_string().contains("at most 4 MiB"));
        assert!(!outside.exists());
        std::fs::remove_file(&note).unwrap();
        assert!(backend.note_write(&s(&note), "x", Some(&rev), &roots).unwrap_err().to_string().contains("moved or deleted"));
        // Reads: missing files, folders and notes over 4 MiB are errors.
        assert!(backend.note_read(&s(&note)).is_err());
        assert!(backend.note_read(&s(&root)).unwrap_err().to_string().contains("not a file"));
        let big = root.join("big.log");
        std::fs::write(&big, vec![b'x'; notebook::MAX_NOTE as usize + 1]).unwrap();
        assert!(backend.note_read(&s(&big)).unwrap_err().to_string().contains("at most 4 MiB"));
        std::fs::remove_dir_all(home).unwrap();
    }

    /// A rename that only changes case: on a case-blind file system (APFS,
    /// Windows) the same file gets the new case; elsewhere a new name. A
    /// name taken by another file is refused, as everywhere.
    #[test]
    fn a_case_change_renames_the_note() {
        let dir = temp("case");
        let roots = [s(&dir)];
        std::fs::write(dir.join("Plan.md"), "plan").unwrap();
        std::fs::write(dir.join("Other.md"), "other").unwrap();
        assert!(matches!(rename(&s(&dir.join("Plan.md")), &s(&dir.join("plan.md")), &roots), Ok(Response::Done)));
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"plan.md".to_owned()) && !names.contains(&"Plan.md".to_owned()), "{names:?}");
        assert_eq!(std::fs::read_to_string(dir.join("plan.md")).unwrap(), "plan");
        let refused = rename(&s(&dir.join("plan.md")), &s(&dir.join("Other.md")), &roots).unwrap_err().to_string();
        assert!(refused.contains("already exists") && std::fs::read_to_string(dir.join("Other.md")).unwrap() == "other", "{refused}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn polled_revisions_read_only_files_that_changed() {
        let home = temp("revs");
        let note = home.join("open.md");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(&note, "AAAA").unwrap();
        let at = std::fs::metadata(&note).unwrap().modified().unwrap();
        let first = revs(&[s(&note)]).unwrap();
        assert!(matches!(&first, Response::NoteRevs(r) if r[0].as_deref() == Some(notebook::rev_of(b"AAAA").as_str())));
        // Same size, same time: taken as unchanged, not read again.
        std::fs::write(&note, "BBBB").unwrap();
        std::fs::File::options().write(true).open(&note).unwrap().set_modified(at).unwrap();
        assert_eq!(revs(&[s(&note)]).unwrap(), first, "no read, no hash");
        // Changed (its time moved): read and hashed again.
        std::fs::File::options().write(true).open(&note).unwrap().set_modified(at + Duration::from_secs(2)).unwrap();
        assert!(matches!(revs(&[s(&note)]).unwrap(), Response::NoteRevs(r) if r[0].as_deref() == Some(notebook::rev_of(b"BBBB").as_str())));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn revisions_bytes_folders_renames_and_the_trash() {
        let home = temp("files");
        let root = home.join("notebook");
        let roots = vec![s(&root)];
        std::fs::create_dir_all(&root).unwrap();
        let a = root.join("a.md");
        std::fs::write(&a, "A").unwrap();
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        assert_eq!(backend.note_revs(vec![s(&a), s(&root.join("gone.md")), "relative".into(), s(&root)]).unwrap(), [Some(notebook::rev_of(b"A")), None, None, None]);
        assert!(backend.note_revs(vec![s(&a); MAX_REVS + 1]).is_err());
        let png = root.join("i.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(backend.read_bytes(&s(&png), 1 << 20).unwrap(), [0x89, b'P', b'N', b'G']);
        assert!(backend.read_bytes(&s(&png), 3).unwrap_err().to_string().contains("larger than 3 bytes"));
        assert!(backend.read_bytes(&s(&root), 10).is_err());
        // Folders.
        let folder = root.join("Projects").join("New");
        backend.note_mkdir(&s(&folder), &roots).unwrap();
        assert!(folder.is_dir());
        assert!(backend.note_mkdir(&s(&folder), &roots).unwrap_err().to_string().contains("already exists"));
        assert!(backend.note_mkdir(&s(&home.join("out")), &roots).is_err());
        // Renames and moves; never over another file, never a folder into itself.
        let b_md = folder.join("b.md");
        backend.note_rename(&s(&a), &s(&b_md), &roots).unwrap();
        assert!(!a.exists() && std::fs::read_to_string(&b_md).unwrap() == "A");
        std::fs::write(&a, "other").unwrap();
        assert!(backend.note_rename(&s(&a), &s(&b_md), &roots).unwrap_err().to_string().contains("already exists"));
        assert!(backend.note_rename(&s(&root.join("Projects")), &s(&folder.join("inner")), &roots).unwrap_err().to_string().contains("into itself"));
        assert!(backend.note_rename(&s(&a), &s(&home.join("a.md")), &roots).is_err(), "never out of the roots");
        assert!(backend.note_rename(&s(&root.join("missing.md")), &s(&root.join("m2.md")), &roots).unwrap_err().to_string().contains("does not exist"));
        if cfg!(windows) {
            backend.note_rename(&s(&a), &s(&root.join("A.md")), &roots).unwrap();
            assert!(std::fs::read_dir(&root).unwrap().flatten().any(|e| e.file_name() == "A.md"), "a change of case is a rename");
        }
        let a = std::fs::read_dir(&root).unwrap().flatten().find(|e| e.file_name().eq_ignore_ascii_case("a.md")).unwrap().path();
        // The trash.
        backend.note_trash(&s(&a), &s(&root)).unwrap();
        backend.note_trash(&s(&root.join("Projects")), &s(&root)).unwrap();
        let trashed: Vec<String> = std::fs::read_dir(root.join(".trash")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(trashed.len(), 2);
        assert!(trashed.iter().any(|n| n.to_lowercase().ends_with("-a.md")) && trashed.iter().any(|n| n.ends_with("-Projects")), "{trashed:?}");
        assert!(!a.exists() && !root.join("Projects").exists());
        let inside = root.join(".trash").join(&trashed[0]);
        assert!(backend.note_trash(&s(&inside), &s(&root)).unwrap_err().to_string().contains("already in the trash"));
        assert!(backend.note_trash(&s(&root), &s(&root)).is_err(), "never the root itself");
        assert!(backend.note_trash(&s(&root.join("gone.md")), &s(&root)).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn search_finds_names_and_lines_and_skips_the_trash_git_and_node_modules() {
        let home = temp("search");
        let root = home.join("notebook");
        for dir in ["Inbox", ".trash", ".git", "node_modules", "Groups/Team/ai-docs"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("Inbox").join("Rollout plan.md"), "# Plan\nThe ROLLOUT starts Monday.\nnothing\n").unwrap();
        std::fs::write(root.join("Groups/Team/ai-docs").join("notes.txt"), format!("{}rollout at the end\n", "x".repeat(300))).unwrap();
        std::fs::write(root.join("Groups/Team/ai-docs").join("rollout.png"), "rollout in a binary").unwrap();
        for skipped in [".trash", ".git", "node_modules"] {
            std::fs::write(root.join(skipped).join("rollout.md"), "rollout").unwrap();
        }
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        // The group folder is inside the root: searched once.
        let roots = vec![s(&root), s(&root.join("Groups").join("Team"))];
        let hits = backend.note_search(&roots, "  Rollout ", 100).unwrap();
        let found: Vec<(String, u32)> = hits.iter().map(|h| (notebook::rel_path(Path::new(&h.path), &root).unwrap(), h.line)).collect();
        assert_eq!(
            found,
            [
                ("Groups/Team/ai-docs/notes.txt".to_owned(), 1),
                ("Groups/Team/ai-docs/rollout.png".to_owned(), 0),
                ("Inbox/Rollout plan.md".to_owned(), 0),
                ("Inbox/Rollout plan.md".to_owned(), 2)
            ],
            "names of every file, contents of text files only"
        );
        assert_eq!((hits[1].text.as_str(), hits[3].text.as_str()), ("rollout.png", "The ROLLOUT starts Monday."));
        assert!(hits[0].text.starts_with('…') && hits[0].text.contains("rollout at the end") && hits[0].text.chars().count() <= 202, "{}", hits[0].text);
        assert_eq!(backend.note_search(&roots, "rollout", 2).unwrap().len(), 2, "the limit");
        assert!(backend.note_search(&roots, "   ", 10).unwrap().is_empty());
        assert!(backend.note_search(&["relative".into()], "x", 10).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn new_files_never_replace_existing_ones() {
        let dir = temp("create");
        assert_eq!(create_new(&dir.join("sub"), "Plan", "md", b"one").unwrap(), dir.join("sub").join("Plan.md"));
        assert_eq!(create_new(&dir.join("sub"), "Plan", "md", b"two").unwrap(), dir.join("sub").join("Plan-2.md"));
        // Windows' and macOS's usual file systems ignore case (plan.md is Plan.md); Linux's do not.
        let case_blind = dir.join("sub").join("PLAN.md").exists();
        assert_eq!(create_new(&dir.join("sub"), "plan", "md", b"three").unwrap().file_name().unwrap().to_string_lossy().to_lowercase(), if case_blind { "plan-3.md" } else { "plan.md" });
        assert_eq!(std::fs::read_to_string(dir.join("sub").join("Plan.md")).unwrap(), "one");
        let ctx = Ctx { home: dir.clone(), sessions_dir: dir.clone() };
        assert!(matches!(handle(&ctx, Request::NoteRead { path: s(&dir.join("sub").join("Plan-2.md")) }), Response::Note(n) if n.text == "two"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_windows_would_read_as_others_are_known() {
        for name in ["x.", "x ", ".git.", ".git ", ".git::$INDEX_ALLOCATION", "a.md:stream", "nul.md", "CON", "com1.txt", "GIT~1", "git~12", "a<b", "a|b", "a?", "tab\there"] {
            assert!(windows_name_problem(name).is_some(), "{name:?}");
        }
        for name in ["plan.md", ".git", ".hidden", "git~", "git~x", "a~1", "Été", "con-tract.md", "x..y"] {
            assert_eq!(windows_name_problem(name), None, "{name:?}");
        }
        assert!(cloud_attributes(0x40_0020) && cloud_attributes(0x1000) && cloud_attributes(0x4_0000) && !cloud_attributes(0x20) && !cloud_attributes(0x80));
    }

    #[test]
    #[cfg(windows)]
    fn writes_refuse_names_windows_would_change() {
        let home = temp("winnames");
        let root = home.join("notebook");
        std::fs::create_dir_all(&root).unwrap();
        let roots = vec![s(&root)];
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        // `.git.` and `GIT~1` would be `.git`; `x.` would be `x`; `nul.md` the device; `:` a stream of `a.md`.
        std::fs::write(root.join("a.md"), "mine").unwrap();
        for rel in [".git.\\config", "GIT~1\\config", "x.", "sub \\x.md", "nul.md", "a.md:stream", "a.md::$DATA"] {
            let path = format!("{}\\{rel}", root.display());
            assert!(backend.note_write(&path, "x", None, &roots).unwrap_err().to_string().contains("Windows cannot write"), "{rel}");
            assert!(backend.note_mkdir(&path, &roots).is_err(), "{rel}");
        }
        assert_eq!(std::fs::read_to_string(root.join("a.md")).unwrap(), "mine", "no stream written to a.md");
        assert!(backend.note_rename(&s(&root.join("a.md")), &format!("{}\\CON", root.display()), &roots).unwrap_err().to_string().contains("device"));
        assert!(backend.note_trash(&format!("{}\\a.md.", root.display()), &s(&root)).unwrap_err().to_string().contains("trailing dots"), "never the trash of another file");
        assert!(root.join("a.md").exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "nothing created");
        // The roots' own names are the user's: a root with a short name still works.
        let tilde = home.join("GIT~9 notes");
        std::fs::create_dir_all(&tilde).unwrap();
        backend.note_write(&s(&tilde.join("x.md")), "x", None, &[s(&tilde)]).unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    /// A link (a junction on Windows: no privilege needed) at `link` to the folder `target`.
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().is_ok_and(|o| o.status.success())
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn links_never_lead_a_change_out_of_the_roots() {
        let home = temp("links");
        let (root, outside, mount) = (home.join("notebook"), home.join("outside"), home.join("mount"));
        for dir in [&root, &outside, &mount] {
            std::fs::create_dir_all(dir).unwrap();
        }
        if !link_dir(&root.join("out"), &outside) || !link_dir(&root.join("vault"), &mount) {
            eprintln!("no links on this machine: skipped");
            return;
        }
        let roots = vec![s(&root)];
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        std::fs::write(root.join("a.md"), "a").unwrap();
        let leads = |e: anyhow::Error| e.to_string().contains("leads out of the notebook's folders");
        assert!(leads(backend.note_write(&s(&root.join("out").join("x.md")), "x", None, &roots).unwrap_err()));
        assert!(leads(backend.note_write(&s(&root.join("out").join("new").join("x.md")), "x", None, &roots).unwrap_err()));
        assert!(leads(backend.note_mkdir(&s(&root.join("out").join("sub")), &roots).unwrap_err()));
        assert!(leads(backend.note_rename(&s(&root.join("a.md")), &s(&root.join("out").join("a.md")), &roots).unwrap_err()));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing written outside");
        // A link to another root (a mount) is fine; so is moving the link itself.
        let both = vec![s(&root), s(&mount)];
        backend.note_write(&s(&root.join("vault").join("v.md")), "v", None, &both).unwrap();
        assert_eq!(std::fs::read_to_string(mount.join("v.md")).unwrap(), "v");
        assert!(leads(backend.note_write(&s(&root.join("vault").join("w.md")), "w", None, &roots).unwrap_err()), "not a root here");
        backend.note_rename(&s(&root.join("out")), &s(&root.join("out2")), &roots).unwrap();
        assert!(outside.is_dir() && std::fs::symlink_metadata(root.join("out2")).is_ok());
        // A trash that is a link out of its root is refused.
        assert!(link_dir(&root.join(".trash"), &outside));
        assert!(leads(backend.note_trash(&s(&root.join("a.md")), &s(&root)).unwrap_err()));
        assert!(root.join("a.md").exists() && std::fs::read_dir(&outside).unwrap().count() == 0);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_note_that_is_not_utf8_is_shown_but_never_saved() {
        let home = temp("lossy");
        let root = home.join("notebook");
        std::fs::create_dir_all(&root).unwrap();
        let roots = vec![s(&root)];
        let b = backend(&home);
        let backend: &dyn Backend = &b;
        let latin = root.join("latin1.md");
        let bytes = b"caf\xe9 \xff\n".to_vec();
        std::fs::write(&latin, &bytes).unwrap();
        let read = backend.note_read(&s(&latin)).unwrap();
        assert_eq!(read.text, "caf\u{fffd} \u{fffd}\n");
        let error = backend.note_write(&s(&latin), &format!("{}more", read.text), Some(&read.rev), &roots).unwrap_err().to_string();
        assert!(error.contains("not UTF-8"), "{error}");
        assert_eq!(std::fs::read(&latin).unwrap(), bytes, "its bytes are kept");
        // UTF-8 with a byte order mark and CRLF: saved as given, the mark kept.
        let bom = root.join("bom.md");
        std::fs::write(&bom, "\u{feff}a\r\nb\r\n").unwrap();
        let read = backend.note_read(&s(&bom)).unwrap();
        assert!(read.text.starts_with('\u{feff}'));
        backend.note_write(&s(&bom), &read.text.replace('b', "c"), Some(&read.rev), &roots).unwrap();
        assert_eq!(std::fs::read(&bom).unwrap(), "\u{feff}a\r\nc\r\n".as_bytes());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn moves_never_replace_a_file_and_failed_writes_leave_no_temporary_file() {
        let dir = temp("moves");
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        // As if `b.md` appeared after the checks.
        assert_eq!(move_no_replace(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!((std::fs::read_to_string(&a).unwrap(), std::fs::read_to_string(&b).unwrap()), ("a".into(), "b".into()));
        move_no_replace(&a, &dir.join("c.md")).unwrap();
        assert!(!a.exists() && dir.join("c.md").exists());
        // Past 260 characters too.
        let deep = dir.join("d".repeat(120)).join("e".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        move_no_replace(&dir.join("c.md"), &deep.join("c.md")).unwrap();
        assert_eq!(std::fs::read_to_string(deep.join("c.md")).unwrap(), "a");
        // A rename over a folder fails: the temporary file goes.
        std::fs::create_dir_all(dir.join("folder.md")).unwrap();
        assert!(write_atomic(&dir.join("folder.md"), b"x").is_err());
        let long = dir.join(format!("{}.md", "n".repeat(240)));
        write_atomic(&long, b"long").unwrap();
        assert_eq!(std::fs::read(&long).unwrap(), b"long", "a long name still fits its temporary file");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn writes_and_moves_wait_for_a_program_that_holds_a_file_a_moment() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("held");
        /// Opens `path` without any sharing (as antivirus scanners open new files) for 150 ms.
        fn hold(path: &Path) -> std::thread::JoinHandle<()> {
            let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                drop(held);
            })
        }
        let (note, fresh) = (dir.join("held.md"), dir.join(".held.md.tmp"));
        std::fs::write(&note, "old").unwrap();
        // The note held: access denied until it closes.
        let release = hold(&note);
        write_atomic(&note, b"new").unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read(&note).unwrap(), b"new");
        // The fresh temporary file held: a sharing violation (error 32), also waited for.
        std::fs::write(&fresh, "newer").unwrap();
        let release = hold(&fresh);
        replace(&fresh, &note).unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read(&note).unwrap(), b"newer");
        // A note being moved while held.
        let release = hold(&note);
        move_no_replace(&note, &dir.join("moved.md")).unwrap();
        release.join().unwrap();
        assert!(dir.join("moved.md").exists() && !note.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
