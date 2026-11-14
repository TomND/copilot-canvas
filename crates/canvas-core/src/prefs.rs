//! Per-client canvas preferences (view, panels, tiles, selection). They are
//! never shared with other windows; each client keeps its own copy in its own
//! data folder: `<home>/prefs/<instance>-<document key>.json`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::sync::{ClientPrefs, Kept, Op};

/// Canvas changes a client could not send before it closed (offline, or
/// waiting for the canvas again): kept next to its preferences and replayed
/// at its next start where the canvas still holds what they were made from
/// (`sync::replay`), so quitting while "Offline — reconnecting…" loses
/// nothing and overwrites nothing changed since. Read once at start, then
/// removed; older than [`UNSENT_MAX_AGE_MS`] they are dropped.
#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Unsent {
    /// The document they belong to.
    pub key: String,
    pub saved_ms: u64,
    /// The changes with their bases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<Kept>,
    /// An older version's changes, without bases (never written now: an
    /// older version cannot read this file, and leaves it alone).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<Op>,
}

/// Kept changes older than this are dropped at start, not replayed (a day).
pub const UNSENT_MAX_AGE_MS: u64 = 24 * 3600 * 1000;

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

    /// The backend's facts as the last run got them (`App::new` starts with
    /// them and asks the backend again in the background).
    pub fn load_info(&self) -> Option<crate::rpc::BackendInfo> {
        serde_json::from_slice(&std::fs::read(self.path.with_extension("backend.json")).ok()?).ok()
    }

    /// Keeps the backend's facts for the next start (atomically).
    pub fn save_info(&self, info: &crate::rpc::BackendInfo) -> Result<()> {
        let path = self.path.with_extension("backend.json");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(info)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Where this client's unsent canvas changes are kept.
    fn unsent_path(&self) -> PathBuf {
        self.path.with_extension("unsent.json")
    }

    /// Keeps canvas changes that could not be sent (atomically).
    pub fn save_unsent(&self, unsent: &Unsent) -> Result<()> {
        let path = self.unsent_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(unsent)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// The canvas changes kept by the last run, if any.
    pub fn load_unsent(&self) -> Option<Unsent> {
        serde_json::from_slice(&std::fs::read(self.unsent_path()).ok()?).ok()
    }

    /// They were sent: forgotten.
    pub fn clear_unsent(&self) {
        let _ = std::fs::remove_file(self.unsent_path());
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
