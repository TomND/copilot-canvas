//! Where a session's history is (plan VM-27, D17): the canvas's own
//! Copilot folder, or the mirror of a VM session's guest on this hub
//! (`<home>/vm/mirrors/<sandbox>/copilot/`). File-backed, so the service and
//! a `LocalBackend` resolve a session alike. Copilot only until Phase 5.

use std::path::{Path, PathBuf};

/// A Copilot session's history root (the folder holding `<id>/events.jsonl`).
#[derive(Debug, Clone, PartialEq)]
pub enum Root {
    Local(PathBuf),
    Mirror { sandbox: String, root: PathBuf },
}

impl Root {
    pub fn path(&self) -> &Path {
        match self {
            Root::Local(path) => path,
            Root::Mirror { root, .. } => root,
        }
    }
}

/// Session `id`'s Copilot history: the canvas's own folder when it is
/// there, else a VM mirror that has it, else the canvas's own folder.
pub fn copilot(home: &Path, sessions_dir: &Path, id: &str) -> Root {
    let has = |root: &Path| crate::index::is_session_id(id) && root.join(id).join("events.jsonl").is_file();
    if has(sessions_dir) {
        return Root::Local(sessions_dir.to_path_buf());
    }
    for root in super::thin::copilot_mirrors(home) {
        if has(&root) {
            let sandbox = root.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Root::Mirror { sandbox, root };
        }
    }
    Root::Local(sessions_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_sessions_history_is_found_in_its_mirror() {
        let home = std::env::temp_dir().join(format!("atelier-store-{}", uuid::Uuid::new_v4()));
        let sessions = home.join("sessions");
        let id = "0b1e5ca1-0000-4000-8000-00000000c10d";
        assert_eq!(copilot(&home, &sessions, id), Root::Local(sessions.clone()), "nowhere: the canvas's own folder");
        let mirror = super::super::thin::mirror_dir(&home, "sbx-0123456789ab").join("copilot");
        std::fs::create_dir_all(mirror.join(id)).unwrap();
        std::fs::write(mirror.join(id).join("events.jsonl"), b"{}\n").unwrap();
        assert_eq!(copilot(&home, &sessions, id), Root::Mirror { sandbox: "sbx-0123456789ab".into(), root: mirror.clone() });
        std::fs::create_dir_all(sessions.join(id)).unwrap();
        std::fs::write(sessions.join(id).join("events.jsonl"), b"{}\n").unwrap();
        assert_eq!(copilot(&home, &sessions, id), Root::Local(sessions.clone()), "the canvas's own folder first");
        assert_eq!(copilot(&home, &sessions, "../x"), Root::Local(sessions), "never a path");
        let _ = std::fs::remove_dir_all(home);
    }
}
