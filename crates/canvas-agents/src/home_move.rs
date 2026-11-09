// rename:keep-begin
//! Moving the data home of an install from before the rename (Copilot
//! Canvas's `~/.copilot-canvas`) to Atelier's `~/.atelier`, once, on the first
// rename:keep-end
//! start of the new build (docs/DEVELOPMENT.md, *Renaming to Atelier*):
//!
//! - only the default home moves (`--home` and `ATELIER_HOME` are the user's
//!   choice), and only when the new one does not exist yet (both: the new one
//!   is used and the old one is left alone);
//! - never while a service (of either build) runs for the old home: that
//!   start keeps using the old home, `doctor` says so, and `atelier migrate`
//!   finishes the move;
//! - the folder is renamed (nothing is copied or deleted), then absolute
//!   paths into it are rewritten in its settings, canvases, stores and
//!   registries, and each window's preferences follow their canvas's new
//!   key. A marker in the folder makes an interrupted move finish on the next
//!   start; running it again changes nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use canvas_common::legacy;
use canvas_common::paths::{HOME_DIR, Instance};

/// In the home while its paths are being rewritten (holds the old path).
pub const MARKER: &str = ".atelier-moving";

/// Written in the new home after a move: the old path and when.
pub use canvas_common::paths::{MOVED_FROM, moved_from};

/// Folders of a home whose files are never rewritten: programs and the
/// plugin (written again by the service), and the user's own content.
const SKIP_DIRS: [&str; 10] = ["bin", "cli", "plugin", "run", "uploads", "notebook", "reports", "recordings", "group-skills", "spool"];

/// Files whose text may hold paths into the home.
const TEXT_EXTENSIONS: [&str; 6] = ["json", "yaml", "yml", "toml", "plist", "jsonl"];

/// What became of the old home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Moved {
    /// Nothing to move: a new install, or moved before.
    Nothing,
    /// Moved now (or a move interrupted before finished now).
    Moved { from: PathBuf, to: PathBuf, rewritten: usize },
    /// Both exist: the new home is used, the old one is left as it is.
    Both { old: PathBuf, new: PathBuf },
    /// Not moved: something uses the old home (it stays in use this time).
    Deferred { old: PathBuf, why: String },
    /// The move failed; the old home stays in use.
    Failed { old: PathBuf, error: String },
}

impl Moved {
    /// One line for the user (`None`: nothing worth saying).
    pub fn describe(&self) -> Option<String> {
        match self {
            Moved::Nothing => None,
            Moved::Moved { from, to, rewritten } => Some(format!("Moved your data from {} to {} (the rename to Atelier; {rewritten} file(s) updated).", from.display(), to.display())),
            Moved::Both { old, new } => Some(format!("Using {}; {} (from before the rename to Atelier) is left as it is.", new.display(), old.display())),
            Moved::Deferred { old, why } => Some(format!("Your data stays in {} for now: {why}. Run `atelier migrate` to finish the move to ~/{HOME_DIR}.", old.display())),
            Moved::Failed { old, error } => Some(format!("Your data stays in {}: it could not be moved to ~/{HOME_DIR} ({error}); `atelier migrate` tries again.", old.display())),
        }
    }
}

/// Moves the default home of this user when it is safe (see the module).
/// Skipped when the home is chosen (`ATELIER_HOME`, or its old name).
pub fn move_default_home() -> Moved {
    if canvas_common::env::var_os("ATELIER_HOME").is_some_and(|h| !h.is_empty()) {
        return Moved::Nothing;
    }
    let Some(user) = dirs::home_dir() else { return Moved::Nothing };
    move_home(&user, &service_running)
}

/// Moves the old home in `user` ([`legacy::HOME_DIR`]) to `<user>/.atelier`; `in_use` says why the
/// old home must stay (a running service), `None` when it is free.
pub fn move_home(user: &Path, in_use: &dyn Fn(&Path) -> Option<String>) -> Moved {
    let old = user.join(legacy::HOME_DIR);
    let new = user.join(HOME_DIR);
    // A move interrupted after the rename: finish rewriting.
    if let Ok(from) = std::fs::read_to_string(new.join(MARKER)) {
        let from = PathBuf::from(from.trim());
        let rewritten = finish(&from, &new);
        return Moved::Moved { from, to: new, rewritten };
    }
    if new.exists() {
        return if old.is_dir() { Moved::Both { old, new } } else { Moved::Nothing };
    }
    if !old.is_dir() {
        return Moved::Nothing;
    }
    if let Some(why) = in_use(&old) {
        return Moved::Deferred { old, why };
    }
    if let Err(error) = std::fs::write(old.join(MARKER), old.display().to_string()) {
        return Moved::Failed { old, error: error.to_string() };
    }
    if let Err(error) = std::fs::rename(&old, &new) {
        let _ = std::fs::remove_file(old.join(MARKER));
        return Moved::Failed { old, error: error.to_string() };
    }
    let rewritten = finish(&old, &new);
    Moved::Moved { from: old, to: new, rewritten }
}

/// After the rename: paths rewritten, preferences renamed, the move recorded.
fn finish(from: &Path, to: &Path) -> usize {
    let rewritten = rewrite_paths(to, &from.display().to_string(), &to.display().to_string());
    rename_prefs(to, from, to);
    let _ = std::fs::write(to.join(MOVED_FROM), format!("{}\n{}\n", from.display(), canvas_common::paths::rfc3339_now()));
    let _ = std::fs::remove_file(to.join(MARKER));
    rewritten
}

/// The instances that ever ran a service in `home` (their token files), and `default`.
pub fn instances(home: &Path) -> Vec<String> {
    let mut names = vec!["default".to_owned()];
    for entry in std::fs::read_dir(home).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_prefix("service-").and_then(|f| f.strip_suffix(".token"))
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

/// A service (of this build or the one before the rename) runs for `home`
/// ([`instances`]).
pub fn service_running(home: &Path) -> Option<String> {
    for name in instances(home) {
        let instance = Instance { name: name.clone(), home: home.to_path_buf(), sessions_dir: home.to_path_buf() };
        let endpoints = std::iter::once(instance.pipe_name()).chain(instance.legacy_pipe_name());
        if endpoints.into_iter().any(|e| canvas_sys::ipc::connect(&e, Duration::from_millis(300)).is_ok()) {
            return Some(format!("its terminal service (instance {name}) is running"));
        }
    }
    None
}

/// Rewrites absolute paths into the home (`from`, its old path) to `to` in
/// the home's settings, canvases, stores and registries; returns how many
/// files changed. Idempotent: a rewritten file holds no old path.
pub fn rewrite_paths(home: &Path, from: &str, to: &str) -> usize {
    let mut changed = 0;
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !(dir == home && SKIP_DIRS.contains(&name.as_str())) {
                    stack.push(path);
                }
                continue;
            }
            let text_file = path.extension().and_then(|e| e.to_str()).is_some_and(|e| TEXT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            if !kind.is_file() || !text_file || entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > 32 << 20 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let new = replace_path(&text, from, to);
            if new != text && canvas_common::paths::write_atomic(&path, new.as_bytes()).is_ok() {
                changed += 1;
            }
        }
    }
    changed
}

/// `text` with every path that starts with `from` (whole folder names) made
/// to start with `to`: as written, as JSON writes it (backslashes doubled)
/// and with forward slashes; without case on Windows-style paths.
pub fn replace_path(text: &str, from: &str, to: &str) -> String {
    let windows = from.contains('\\') || from.as_bytes().get(1) == Some(&b':');
    let mut forms = vec![(from.to_owned(), to.to_owned())];
    if windows {
        forms.push((from.replace('\\', r"\\"), to.replace('\\', r"\\")));
        forms.push((from.replace('\\', "/"), to.replace('\\', "/")));
    }
    let mut out = text.to_owned();
    for (old, new) in forms {
        out = replace_prefix(&out, &old, &new, windows);
    }
    out
}

fn replace_prefix(text: &str, old: &str, new: &str, ignore_case: bool) -> String {
    if old.is_empty() {
        return text.to_owned();
    }
    let hay = if ignore_case { text.to_ascii_lowercase() } else { text.to_owned() };
    let needle = if ignore_case { old.to_ascii_lowercase() } else { old.to_owned() };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = hay[at..].find(&needle) {
        let start = at + found;
        let end = start + needle.len();
        // Only the whole folder: `<old home>-mock` is another one.
        let next = text[end..].chars().next();
        let whole = next.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')));
        out.push_str(&text[at..start]);
        out.push_str(if whole { new } else { &text[start..end] });
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// Each window's preferences are named after its canvas's key, which comes
/// from the canvas's path: the canvases in the home get new keys.
fn rename_prefs(home: &Path, from: &Path, to: &Path) {
    let prefs = home.join("prefs");
    let Ok(entries) = std::fs::read_dir(&prefs) else { return };
    let saved: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let canvases: Vec<String> = std::fs::read_dir(home).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".json")).collect();
    for canvas in canvases {
        let old_key = canvas_model::sync::doc_key(&from.join(&canvas).display().to_string());
        let new_key = canvas_model::sync::doc_key(&to.join(&canvas).display().to_string());
        if old_key == new_key {
            continue;
        }
        for file in &saved {
            if let Some(instance) = file.strip_suffix(&format!("-{old_key}.json")) {
                let target = prefs.join(format!("{instance}-{new_key}.json"));
                if !target.exists() {
                    let _ = std::fs::rename(prefs.join(file), target);
                }
            }
        }
    }
}

/// `doctor`'s line about the data home (`None`: a plain new install).
pub fn status(home: &Path) -> Option<String> {
    let user = dirs::home_dir()?;
    let (old, new) = (user.join(legacy::HOME_DIR), user.join(HOME_DIR));
    if home == old {
        let why = service_running(&old).unwrap_or_else(|| "it has not been moved yet".into());
        return Some(format!("{} is from before the rename to Atelier and still in use ({why}): run `atelier migrate` to move it to {}", old.display(), new.display()));
    }
    if let Some(from) = moved_from(home) {
        return Some(format!("moved from {} (the rename to Atelier)", from.display()));
    }
    (home == new && old.is_dir()).then(|| format!("{} (from before the rename to Atelier) is left as it is; this build uses {}", old.display(), new.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-move-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn json_path(path: &Path) -> String {
        serde_json::to_string(&path.display().to_string()).unwrap()
    }

    /// An old home with paths into it in the files the move rewrites.
    fn old_home(user: &Path) -> PathBuf {
        let old = user.join(legacy::HOME_DIR);
        let agent = old.join("canvas-agents").join("c1").join("agents").join("digest");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(old.join("prefs")).unwrap();
        std::fs::create_dir_all(old.join("remote")).unwrap();
        std::fs::create_dir_all(old.join("notebook")).unwrap();
        let canvas = format!(r#"{{"nodes":[{{"id":"a","cwd":{}}}],"notebook_root":{}}}"#, json_path(&agent), json_path(&old.join("notebook")));
        std::fs::write(old.join("canvas.json"), canvas).unwrap();
        std::fs::write(old.join("settings.json"), format!(r#"{{"automations_dir":{}}}"#, json_path(&old.join("automations")))).unwrap();
        std::fs::write(old.join("remote").join("servers.json"), format!(r#"{{"x":{}, "mock":{}}}"#, json_path(&old.join("remote")), json_path(&user.join(".copilot-canvas-mock")))).unwrap();
        std::fs::write(old.join("notebook").join("note.json"), json_path(&old)).unwrap();
        let key = canvas_model::sync::doc_key(&old.join("canvas.json").display().to_string());
        std::fs::write(old.join("prefs").join(format!("default-{key}.json")), "{}").unwrap();
        old
    }

    #[test]
    fn an_old_home_alone_is_moved_and_paths_into_it_follow() {
        let user = user();
        let old = old_home(&user);
        let new = user.join(HOME_DIR);
        let moved = move_home(&user, &|_| None);
        assert!(matches!(&moved, Moved::Moved { from, to, rewritten } if *from == old && *to == new && *rewritten == 3), "{moved:?}");
        assert!(!old.exists() && new.join("canvas.json").is_file());
        let canvas = std::fs::read_to_string(new.join("canvas.json")).unwrap();
        assert!(canvas.contains(&json_path(&new.join("canvas-agents").join("c1").join("agents").join("digest"))[1..]) && !canvas.contains(legacy::HOME_DIR), "{canvas}");
        let servers = std::fs::read_to_string(new.join("remote").join("servers.json")).unwrap();
        assert!(servers.contains(".copilot-canvas-mock"), "another folder with the same start is not touched: {servers}");
        assert!(std::fs::read_to_string(new.join("notebook").join("note.json")).unwrap().contains(legacy::HOME_DIR), "the user's notes are left as written");
        let key = canvas_model::sync::doc_key(&new.join("canvas.json").display().to_string());
        assert!(new.join("prefs").join(format!("default-{key}.json")).is_file(), "the window's preferences follow its canvas");
        assert_eq!(moved_from(&new), Some(old.clone()));
        assert!(!new.join(MARKER).exists());
        // Again: nothing to do.
        assert_eq!(move_home(&user, &|_| None), Moved::Nothing);
        std::fs::remove_dir_all(user).unwrap();
    }

    #[test]
    fn with_both_homes_the_new_one_is_used_and_the_old_one_untouched() {
        let user = user();
        let old = old_home(&user);
        let new = user.join(HOME_DIR);
        std::fs::create_dir_all(&new).unwrap();
        let before = std::fs::read_to_string(old.join("canvas.json")).unwrap();
        assert_eq!(move_home(&user, &|_| None), Moved::Both { old: old.clone(), new: new.clone() });
        assert_eq!(std::fs::read_to_string(old.join("canvas.json")).unwrap(), before);
        assert_eq!(canvas_common::paths::home_in(&user), new);
        std::fs::remove_dir_all(user).unwrap();
    }

    #[test]
    fn a_running_service_keeps_the_old_home_in_use_and_says_why() {
        let user = user();
        let old = old_home(&user);
        let moved = move_home(&user, &|home| (home == old).then(|| "its terminal service (instance default) is running".to_owned()));
        assert_eq!(moved, Moved::Deferred { old: old.clone(), why: "its terminal service (instance default) is running".into() });
        assert!(moved.describe().unwrap().contains("atelier migrate"));
        assert!(old.join("canvas.json").is_file() && !user.join(HOME_DIR).exists() && !old.join(MARKER).exists());
        assert_eq!(canvas_common::paths::home_in(&user), old, "this start keeps using it");
        std::fs::remove_dir_all(user).unwrap();
    }

    #[test]
    fn a_service_on_the_old_endpoint_is_found_and_none_otherwise() {
        let user = user();
        let old = old_home(&user);
        assert_eq!(service_running(&old), None);
        std::fs::write(old.join("service-work.token"), "x").unwrap();
        let instance = Instance { name: "work".into(), home: old.clone(), sessions_dir: old.clone() };
        let endpoint = instance.legacy_pipe_name().unwrap_or_else(|| instance.pipe_name());
        let _server = canvas_sys::ipc::PipeServer::bind(&endpoint).unwrap();
        assert_eq!(service_running(&old).as_deref(), Some("its terminal service (instance work) is running"));
        drop(_server);
        std::fs::remove_dir_all(user).unwrap();
    }

    #[test]
    fn an_interrupted_move_finishes_on_the_next_start() {
        let user = user();
        let old = old_home(&user);
        let new = user.join(HOME_DIR);
        std::fs::write(old.join(MARKER), old.display().to_string()).unwrap();
        std::fs::rename(&old, &new).unwrap();
        assert!(matches!(move_home(&user, &|_| None), Moved::Moved { rewritten: 3, .. }));
        assert!(!std::fs::read_to_string(new.join("canvas.json")).unwrap().contains(legacy::HOME_DIR));
        assert_eq!(move_home(&user, &|_| None), Moved::Nothing);
        std::fs::remove_dir_all(user).unwrap();
    }

    // rename:keep-begin
    #[test]
    fn windows_paths_are_rewritten_as_written_in_json_and_with_slashes_in_any_case() {
        let from = r"C:\Users\Me\.copilot-canvas";
        let to = r"C:\Users\Me\.atelier";
        let text = r#"{"a":"C:\\Users\\Me\\.copilot-canvas\\canvas-agents","b":"c:/users/me/.copilot-canvas/x","c":"C:\\Users\\Me\\.copilot-canvas-mock","d":"C:\\Users\\Me\\.copilot-canvas"}"#;
        assert_eq!(replace_path(text, from, to), r#"{"a":"C:\\Users\\Me\\.atelier\\canvas-agents","b":"C:/Users/Me/.atelier/x","c":"C:\\Users\\Me\\.copilot-canvas-mock","d":"C:\\Users\\Me\\.atelier"}"#);
        assert_eq!(replace_path("/Users/me/.copilot-canvas/x /Users/Me/.copilot-canvas", "/Users/me/.copilot-canvas", "/Users/me/.atelier"), "/Users/me/.atelier/x /Users/Me/.copilot-canvas", "POSIX paths keep their case");
    }
    // rename:keep-end
}
