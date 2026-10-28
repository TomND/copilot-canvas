//! Per-client canvas preferences (view, panels, tiles, selection). They are
//! never shared with other windows; each client keeps its own copy in its own
//! data folder: `<home>/prefs/<instance>-<document key>.json`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::sync::ClientPrefs;

pub struct PrefsFile {
    path: PathBuf,
    /// What the file holds (nothing is written while unchanged).
    saved: Option<ClientPrefs>,
}

impl PrefsFile {
    /// The file for a document key.
    pub fn path(home: &Path, instance: &str, key: &str) -> PathBuf {
        home.join("prefs").join(format!("{instance}-{key}.json"))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path, saved: None }
    }

    /// The stored preferences (`None`: a first visit, or unreadable).
    pub fn load(&mut self) -> Option<ClientPrefs> {
        let prefs: ClientPrefs = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        self.saved = Some(prefs.clone());
        Some(prefs)
    }

    /// Writes the preferences when they changed (atomically).
    pub fn save(&mut self, prefs: &ClientPrefs) -> Result<()> {
        if self.saved.as_ref() == Some(prefs) {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec_pretty(prefs)?)?;
        std::fs::rename(&temp, &self.path)?;
        self.saved = Some(prefs.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_and_are_written_only_when_changed() {
        let home = std::env::temp_dir().join(format!("cc-prefs-{}", uuid::Uuid::new_v4()));
        let path = PrefsFile::path(&home, "default", "canvas-0123");
        let mut file = PrefsFile::new(path.clone());
        assert!(file.load().is_none());
        let mut prefs = ClientPrefs::default();
        prefs.view.zoom = 2;
        prefs.tiles = vec!["a".into()];
        file.save(&prefs).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        file.save(&prefs).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written, "unchanged: not written again");
        assert_eq!(PrefsFile::new(path).load(), Some(prefs));
        std::fs::remove_dir_all(home).unwrap();
    }
}
