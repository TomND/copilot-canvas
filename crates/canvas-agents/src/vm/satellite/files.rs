//! The hub's synced roots in the guest (plan VM-34): the canvas's plugin,
//! the user's profile bundle, a box's group skills, instructions and notes,
//! the pack's mapped folders. The hub decides what each root holds
//! (`SatDown::Manifest`, then `SatDown::File` pieces); the satellite keeps
//! what it got in `<roots>/.state.json` (each file's revision) and answers
//! the hub's `{"roots_state":{}}` control with it, so a reconnect sends only
//! what changed.
//!
//! A writable root's edits in the guest go back (`SatUp::Note` with the
//! revision the edit started from); when the hub's file changed meanwhile
//! the guest keeps its version beside it as `<name> (conflict <time>)` and
//! takes the hub's: nothing is lost. A file deleted in the guest comes back.
//!
//! Containment: root ids and names are checked (`sat::name_ok`), a root's
//! folder is `<roots>/<id>` or an absolute path inside the guest's home, and
//! no link is followed on the way to a file.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use canvas_common::sat::{NoteAnswer, RootManifest, RootsState, SAT_FILE_CHUNK, name_ok, rev_of, root_ok};
use serde::{Deserialize, Serialize};

/// A piece being received: its folder's part file.
const PART: &str = ".atelier-part-";
/// An edit sent and not answered is sent again after this.
const NOTE_AGAIN: Duration = Duration::from_secs(30);
const MAX_FILES: usize = 4000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
struct RootState {
    target: String,
    writable: bool,
    /// Each file the hub gave and its revision.
    files: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
struct State {
    roots: BTreeMap<String, RootState>,
}

struct Part {
    path: PathBuf,
    written: u64,
}

/// An edit of the guest on its way to the hub.
pub struct Edit {
    pub root: String,
    pub name: String,
    pub base_rev: Option<String>,
    pub bytes: Vec<u8>,
}

pub struct Files {
    roots: PathBuf,
    home: PathBuf,
    state: Mutex<State>,
    parts: Mutex<HashMap<(String, String), Part>>,
    /// Edits sent: id → (root, name, revision of what was sent, when).
    pending: Mutex<HashMap<String, (String, String, String, Instant)>>,
    /// Revisions by (path, size, modified).
    revs: Mutex<HashMap<PathBuf, (u64, SystemTime, String)>>,
}

impl Files {
    /// The roots under `roots`, absolute targets only inside `home`.
    pub fn open(roots: PathBuf, home: PathBuf) -> Files {
        let state = std::fs::read(roots.join(".state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Files { roots, home, state: Mutex::new(state), parts: Mutex::default(), pending: Mutex::default(), revs: Mutex::default() }
    }

    pub fn roots_dir(&self) -> &Path {
        &self.roots
    }

    fn save(&self, state: &State) {
        if let Ok(bytes) = serde_json::to_vec(state) {
            let _ = crate::paths::write_atomic(&self.roots.join(".state.json"), &bytes);
        }
    }

    /// A root's folder: `<roots>/<id>`, or `target` (absolute, inside the guest's home).
    fn dir(&self, id: &str, target: &str) -> Result<PathBuf> {
        anyhow::ensure!(root_ok(id), "not a root id: {id:?}");
        if target.is_empty() {
            return Ok(id.split('/').fold(self.roots.clone(), |dir, part| dir.join(part)));
        }
        let path = Path::new(target);
        anyhow::ensure!(path.is_absolute() && path.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir)), "{target:?} is not an absolute guest path");
        anyhow::ensure!(path.starts_with(&self.home) && path != self.home, "{target:?} is outside the guest's home");
        Ok(path.to_path_buf())
    }

    /// The file `name` of a root's folder `dir`; the folders on the way are
    /// made when `create`, never through a link.
    fn path_in(dir: &Path, name: &str, create: bool) -> Result<PathBuf> {
        anyhow::ensure!(name_ok(name), "not a file name: {name:?}");
        if create {
            std::fs::create_dir_all(dir)?;
        }
        let parts: Vec<&str> = name.split('/').collect();
        let mut at = dir.to_path_buf();
        for part in &parts[..parts.len() - 1] {
            at = at.join(part);
            match std::fs::symlink_metadata(&at) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => bail!("{name:?}: {part} is not a folder"),
                Err(_) if create => std::fs::create_dir(&at)?,
                Err(error) => return Err(error.into()),
            }
        }
        let file = at.join(parts[parts.len() - 1]);
        if std::fs::symlink_metadata(&file).is_ok_and(|m| !m.is_file()) {
            bail!("{name:?} is not a file");
        }
        Ok(file)
    }

    /// The revision of a file (`None`: not there), cached by size and time.
    fn rev(&self, path: &Path) -> Option<String> {
        let meta = std::fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
        let modified = meta.modified().ok()?;
        if let Some((len, at, rev)) = lock(&self.revs).get(path)
            && *len == meta.len()
            && *at == modified
        {
            return Some(rev.clone());
        }
        let rev = rev_of(&std::fs::read(path).ok()?);
        lock(&self.revs).insert(path.to_path_buf(), (meta.len(), modified, rev.clone()));
        Some(rev)
    }

    /// Keeps the guest's version of `file` beside it: `<stem> (conflict <time>).<ext>`.
    fn keep_aside(file: &Path) {
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let when = canvas_common::paths::format_rfc3339_ms(crate::paths::now_ms()).get(..16).unwrap_or_default().replace(':', "-");
        let aside = file.with_file_name(format!("{stem} (conflict {when}){ext}"));
        let _ = std::fs::rename(file, aside);
    }

    /// What the guest holds of the hub's roots: a read-only root's files as
    /// the hub gave them, a writable root's files that are there (edits go back on their own).
    pub fn state(&self) -> RootsState {
        let state = lock(&self.state).clone();
        let mut out = RootsState::default();
        for (id, root) in &state.roots {
            let Ok(dir) = self.dir(id, &root.target) else { continue };
            let mut files = BTreeMap::new();
            for (name, rev) in &root.files {
                let Ok(path) = Self::path_in(&dir, name, false) else { continue };
                let keep = if root.writable { path.is_file() } else { self.rev(&path).as_deref() == Some(rev.as_str()) };
                if keep {
                    files.insert(name.clone(), rev.clone());
                }
            }
            out.roots.insert(id.clone(), files);
        }
        out
    }

    /// A root as the hub has it now: where it is, and the files it holds.
    pub fn manifest(&self, id: &str, manifest: RootManifest) -> Result<()> {
        let mut state = lock(&self.state);
        let old = state.roots.get(id).cloned().unwrap_or_default();
        let dir = self.dir(id, if manifest.gone { &old.target } else { &manifest.target })?;
        // Files the hub no longer has go, unless the guest changed them (a writable root's edit goes back first).
        for (name, rev) in &old.files {
            if manifest.files.contains_key(name) && !manifest.gone {
                continue;
            }
            if let Ok(path) = Self::path_in(&dir, name, false)
                && self.rev(&path).as_deref() == Some(rev.as_str())
            {
                let _ = std::fs::remove_file(path);
            }
        }
        if manifest.gone {
            state.roots.remove(id);
        } else {
            for folder in &manifest.folders {
                if name_ok(folder) {
                    let _ = Self::path_in(&dir, &format!("{folder}/.keep"), true);
                }
            }
            let mut files: BTreeMap<String, String> = old.files.into_iter().filter(|(name, _)| manifest.files.contains_key(name)).collect();
            // A file already there with the hub's content needs no copy (a state lost, a root moved).
            for (name, rev) in &manifest.files {
                if !files.contains_key(name)
                    && let Ok(path) = Self::path_in(&dir, name, false)
                    && self.rev(&path).as_deref() == Some(rev.as_str())
                {
                    files.insert(name.clone(), rev.clone());
                }
            }
            state.roots.insert(id.to_owned(), RootState { target: manifest.target, writable: manifest.writable, files });
        }
        self.save(&state);
        Ok(())
    }

    /// A piece of a file of the hub (its manifest came first). The last piece
    /// checks the revision and puts the file in place; a writable root's file
    /// the guest changed meanwhile is kept beside it.
    pub fn file(&self, id: &str, name: &str, rev: &str, offset: u64, last: bool, bytes: &[u8]) -> Result<()> {
        let root = lock(&self.state).roots.get(id).cloned().context("a file of a root the hub has not described")?;
        let dir = self.dir(id, &root.target)?;
        anyhow::ensure!(name_ok(name) && !name.rsplit('/').next().unwrap_or_default().starts_with(PART), "not a file name: {name:?}");
        let key = (id.to_owned(), name.to_owned());
        let mut parts = lock(&self.parts);
        if offset == 0 {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{PART}{}", rev_of(format!("{id}/{name}").as_bytes())));
            std::fs::write(&path, b"")?;
            parts.insert(key.clone(), Part { path, written: 0 });
        }
        let part = parts.get_mut(&key).context("a piece of a file that did not begin")?;
        if part.written != offset {
            let _ = std::fs::remove_file(&part.path);
            parts.remove(&key);
            bail!("{id}/{name}: a piece out of order");
        }
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&part.path)?;
            file.write_all(bytes)?;
        }
        part.written += bytes.len() as u64;
        if !last {
            return Ok(());
        }
        let part = parts.remove(&key).expect("present");
        drop(parts);
        let content = std::fs::read(&part.path)?;
        if rev_of(&content) != rev {
            let _ = std::fs::remove_file(&part.path);
            bail!("{id}/{name}: the file does not match its revision");
        }
        let target = match Self::path_in(&dir, name, true) {
            Ok(target) => target,
            Err(error) => {
                let _ = std::fs::remove_file(&part.path);
                return Err(error);
            }
        };
        let mut state = lock(&self.state);
        let given = state.roots.get(id).and_then(|r| r.files.get(name)).cloned();
        if root.writable && target.is_file() && self.rev(&target) != given && self.rev(&target).as_deref() != Some(rev) {
            Self::keep_aside(&target);
        }
        std::fs::rename(&part.path, &target)?;
        if let Some(root) = state.roots.get_mut(id) {
            root.files.insert(name.to_owned(), rev.to_owned());
        }
        self.save(&state);
        Ok(())
    }

    /// The writable roots' edits not sent yet (or sent long ago without an answer), each with an id now recorded.
    pub fn edits(&self) -> Vec<(String, Edit)> {
        let state = lock(&self.state).clone();
        let mut out = Vec::new();
        for (id, root) in state.roots.iter().filter(|(_, r)| r.writable) {
            let Ok(dir) = self.dir(id, &root.target) else { continue };
            let mut found = Vec::new();
            walk(&dir, &dir, 0, &mut found);
            for name in found {
                let Ok(path) = Self::path_in(&dir, &name, false) else { continue };
                let Some(rev) = self.rev(&path) else { continue };
                let base = root.files.get(&name).cloned();
                if base.as_deref() == Some(rev.as_str()) {
                    continue;
                }
                let waiting = lock(&self.pending).values().any(|(r, n, sent, at)| r == id && *n == name && *sent == rev && at.elapsed() < NOTE_AGAIN);
                if waiting {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes.len() > SAT_FILE_CHUNK {
                    continue;
                }
                let edit = uuid::Uuid::new_v4().simple().to_string();
                lock(&self.pending).insert(edit.clone(), (id.clone(), name.clone(), rev, Instant::now()));
                out.push((edit, Edit { root: id.clone(), name, base_rev: base, bytes }));
            }
        }
        out
    }

    /// The hub's answer to an edit: taken (its revision is the hub's now), or
    /// a conflict (the guest's version is kept beside it; the hub's comes next).
    pub fn note_result(&self, edit: &str, answer: &NoteAnswer) {
        let Some((id, name, sent, _)) = lock(&self.pending).remove(edit) else { return };
        if !answer.ok && !answer.conflict {
            // Refused (a name the root does not take): asked again only after a while.
            lock(&self.pending).insert(edit.to_owned(), (id, name, sent, Instant::now()));
            return;
        }
        let mut state = lock(&self.state);
        let Some(root) = state.roots.get(&id).cloned() else { return };
        let Ok(dir) = self.dir(&id, &root.target) else { return };
        if answer.ok {
            if let Some(root) = state.roots.get_mut(&id) {
                root.files.insert(name, if answer.rev.is_empty() { sent } else { answer.rev.clone() });
            }
        } else {
            if let Ok(path) = Self::path_in(&dir, &name, false) {
                Self::keep_aside(&path);
            }
            if let Some(root) = state.roots.get_mut(&id) {
                root.files.remove(&name);
            }
        }
        self.save(&state);
    }

    /// The link dropped: pieces half received are dropped, edits are sent again.
    pub fn link_lost(&self) {
        for (_, part) in lock(&self.parts).drain() {
            let _ = std::fs::remove_file(part.path);
        }
        lock(&self.pending).clear();
    }
}

/// The files under `dir` (names relative to `base`, `/`-separated), links
/// and part files left out.
fn walk(dir: &Path, base: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 12 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, base, depth + 1, out);
        } else if kind.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            let name = rel.to_string_lossy().replace('\\', "/");
            if !name.rsplit('/').next().unwrap_or_default().starts_with(PART) && !name.starts_with(".state.json") && name_ok(&name) {
                out.push(name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> (PathBuf, Files) {
        let home = std::env::temp_dir().join(format!("atelier-files-{}", uuid::Uuid::new_v4()));
        let roots = home.join(".atelier").join("vm").join("roots");
        std::fs::create_dir_all(&roots).unwrap();
        (home.clone(), Files::open(roots, home))
    }

    fn give(files: &Files, id: &str, name: &str, bytes: &[u8]) {
        let rev = rev_of(bytes);
        let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[]] } else { bytes.chunks(3).collect() };
        let mut offset = 0;
        for (i, piece) in pieces.iter().enumerate() {
            files.file(id, name, &rev, offset, i + 1 == pieces.len(), piece).unwrap();
            offset += piece.len() as u64;
        }
    }

    fn manifest(writable: bool, names: &[(&str, &[u8])]) -> RootManifest {
        RootManifest { writable, files: names.iter().map(|(n, b)| (n.to_string(), rev_of(b))).collect(), ..Default::default() }
    }

    #[test]
    fn a_read_only_root_follows_the_hub_and_survives_a_lost_state() {
        let (home, files) = files();
        files.manifest("plugin", manifest(false, &[("plugin.json", b"{}"), ("skills/a/SKILL.md", b"skill a")])).unwrap();
        give(&files, "plugin", "plugin.json", b"{}");
        give(&files, "plugin", "skills/a/SKILL.md", b"skill a");
        let dir = files.roots_dir().join("plugin");
        assert_eq!(std::fs::read(dir.join("skills/a/SKILL.md")).unwrap(), b"skill a");
        assert_eq!(files.state().roots["plugin"].len(), 2);
        // Changed in the guest: the hub is told it lacks it, and its copy comes back.
        std::fs::write(dir.join("plugin.json"), b"tampered").unwrap();
        assert_eq!(files.state().roots["plugin"].keys().collect::<Vec<_>>(), ["skills/a/SKILL.md"]);
        give(&files, "plugin", "plugin.json", b"{}");
        assert_eq!(std::fs::read(dir.join("plugin.json")).unwrap(), b"{}");
        // A file the hub no longer has goes; a root that goes takes its files.
        files.manifest("plugin", manifest(false, &[("plugin.json", b"{}")])).unwrap();
        assert!(!dir.join("skills/a/SKILL.md").exists());
        // The state lost: files already right are not sent again.
        std::fs::remove_file(files.roots_dir().join(".state.json")).unwrap();
        let again = Files::open(files.roots_dir().to_path_buf(), home.clone());
        assert!(again.state().roots.is_empty());
        again.manifest("plugin", manifest(false, &[("plugin.json", b"{}")])).unwrap();
        assert_eq!(again.state().roots["plugin"].len(), 1);
        again.manifest("plugin", RootManifest { gone: true, ..Default::default() }).unwrap();
        assert!(!dir.join("plugin.json").exists() && again.state().roots.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn pieces_are_checked_and_names_and_targets_contained() {
        let (home, files) = files();
        assert!(files.file("plugin", "a", "x", 0, true, b"x").is_err(), "no manifest yet");
        files.manifest("plugin", manifest(false, &[])).unwrap();
        assert!(files.file("plugin", "a.txt", &rev_of(b"right"), 0, true, b"wrong").unwrap_err().to_string().contains("revision"));
        assert!(files.file("plugin", "b.txt", &rev_of(b"abc"), 1, true, b"bc").is_err(), "out of order");
        for bad in ["../x", "/etc/x", "a/../../x", "C:\\x", "nul.md", ".git/config", "a\\b"] {
            assert!(files.file("plugin", bad, &rev_of(b"x"), 0, true, b"x").is_err(), "{bad:?}");
        }
        for (id, target) in [("../up", ""), ("notes/g", "/etc"), ("notes/g", "relative/path"), ("notes/g", home.parent().unwrap().to_str().unwrap())] {
            assert!(files.manifest(id, RootManifest { target: target.into(), ..Default::default() }).is_err(), "{id} {target}");
        }
        // A link on the way is never followed.
        #[cfg(unix)]
        {
            let outside = home.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, files.roots_dir().join("plugin").join("out")).unwrap();
            assert!(files.file("plugin", "out/x.txt", &rev_of(b"x"), 0, true, b"x").is_err());
            assert!(!outside.join("x.txt").exists());
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_writable_roots_edits_go_back_and_a_conflict_loses_nothing() {
        let (home, files) = files();
        let target = home.join("notes").display().to_string();
        files.manifest("paths/0", RootManifest { target: target.clone(), writable: true, files: [("plan.md".to_owned(), rev_of(b"v1"))].into(), folders: vec!["ai-docs".into()], gone: false }).unwrap();
        assert!(PathBuf::from(&target).join("ai-docs").is_dir(), "its folders are made");
        give(&files, "paths/0", "plan.md", b"v1");
        assert!(files.edits().is_empty(), "nothing changed");
        let dir = PathBuf::from(&target);
        std::fs::write(dir.join("plan.md"), b"v2 from the guest").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new.md"), b"new").unwrap();
        let edits = files.edits();
        assert_eq!(edits.len(), 2);
        assert!(files.edits().is_empty(), "sent once until answered");
        let plan = edits.iter().find(|(_, e)| e.name == "plan.md").unwrap();
        assert_eq!(plan.1.base_rev.as_deref(), Some(rev_of(b"v1").as_str()));
        let new = edits.iter().find(|(_, e)| e.name == "sub/new.md").unwrap();
        assert_eq!(new.1.base_rev, None);
        files.note_result(&new.0, &NoteAnswer { ok: true, rev: rev_of(b"new"), ..Default::default() });
        // The hub's plan.md changed meanwhile: a conflict keeps the guest's version beside it.
        files.note_result(&plan.0, &NoteAnswer { ok: false, conflict: true, ..Default::default() });
        let aside: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("plan (conflict ")).collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(dir.join(&aside[0])).unwrap(), b"v2 from the guest");
        assert!(!files.state().roots["paths/0"].contains_key("plan.md"), "the hub's version comes next");
        files.manifest("paths/0", RootManifest { target: target.clone(), writable: true, files: [("plan.md".to_owned(), rev_of(b"v3 hub")), ("sub/new.md".to_owned(), rev_of(b"new"))].into(), ..Default::default() }).unwrap();
        give(&files, "paths/0", "plan.md", b"v3 hub");
        assert_eq!(std::fs::read(dir.join("plan.md")).unwrap(), b"v3 hub");
        // The copy aside is a new file of the guest: it goes back too.
        let edits = files.edits();
        assert!(edits.len() == 1 && edits[0].1.name.starts_with("plan (conflict ") && edits[0].1.base_rev.is_none());
        // A hub file arriving over an unsent guest edit keeps the edit aside as well.
        std::fs::write(dir.join("sub/new.md"), b"edited again").unwrap();
        give(&files, "paths/0", "sub/new.md", b"hub's newer");
        assert!(std::fs::read_dir(dir.join("sub")).unwrap().flatten().any(|e| std::fs::read(e.path()).unwrap() == b"edited again"));
        let _ = std::fs::remove_dir_all(home);
    }
}
