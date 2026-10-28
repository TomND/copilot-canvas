//! The corpus directory: `corpus/<name>/stream.vtrec`, `meta.json`,
//! `expected.<engine>.json`, optional `input.script` and
//! `reviewed-differences.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use canvas_host::emulator::Engine;
use serde::{Deserialize, Serialize};

use crate::replay::{Expected, compare, replay};
use crate::vtrec::Recording;

/// What an entry is and where it came from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub description: String,
    /// `recorded` (the ConPTY recorder), `session` (the service tap) or `synthetic`.
    pub source: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub tags: Vec<String>,
    /// How it was recorded (program and arguments), for re-recording.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Environment the recorder added (`NAME=value`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recorded_on: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recorded_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// The result of checking one entry with one engine.
#[derive(Debug)]
pub enum Outcome {
    Match { screens: usize },
    Mismatch(Vec<String>),
    /// No `expected.<engine>.json` yet.
    Missing,
}

/// What blessing changed.
#[derive(Debug, PartialEq)]
pub enum Blessed {
    New,
    Unchanged,
    /// With the differences from the previous expectation.
    Updated(Vec<String>),
}

pub struct Corpus {
    pub dir: PathBuf,
}

/// The repository's corpus (compile-time path; tests and development builds).
pub fn repo_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("corpus")
}

impl Corpus {
    /// `explicit`, else `COPILOT_CANVAS_CORPUS`, `./corpus`, then the repository's corpus.
    pub fn locate(explicit: Option<&Path>) -> Result<Self> {
        if let Some(dir) = explicit {
            ensure!(dir.is_dir(), "corpus folder {} does not exist", dir.display());
            return Ok(Self { dir: dir.to_path_buf() });
        }
        let candidates = [std::env::var_os("COPILOT_CANVAS_CORPUS").map(PathBuf::from), Some(PathBuf::from("corpus")), Some(repo_dir())];
        match candidates.into_iter().flatten().find(|dir| dir.join("README.md").is_file()) {
            Some(dir) => Ok(Self { dir: std::path::absolute(&dir).unwrap_or(dir) }),
            None => bail!("no corpus folder found (pass --corpus DIR or run from the repository)"),
        }
    }

    /// Entry names (folders with a `stream.vtrec`), sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir).with_context(|| format!("cannot list {}", self.dir.display()))? {
            let entry = entry?;
            if entry.path().join("stream.vtrec").is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// The named entries (all when `names` is empty); unknown names are errors.
    pub fn select(&self, names: &[&str]) -> Result<Vec<String>> {
        let all = self.names()?;
        if names.is_empty() {
            return Ok(all);
        }
        names
            .iter()
            .map(|name| {
                let name = name.trim_end_matches(['/', '\\']);
                let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
                all.iter().find(|n| *n == name).cloned().with_context(|| format!("no corpus entry {name:?}"))
            })
            .collect()
    }

    pub fn entry_dir(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn stream_path(&self, name: &str) -> PathBuf {
        self.entry_dir(name).join("stream.vtrec")
    }

    pub fn meta_path(&self, name: &str) -> PathBuf {
        self.entry_dir(name).join("meta.json")
    }

    pub fn expected_path(&self, name: &str, engine: Engine) -> PathBuf {
        self.entry_dir(name).join(format!("expected.{}.json", engine.name()))
    }

    pub fn recording(&self, name: &str) -> Result<Recording> {
        Recording::read(&self.stream_path(name))
    }

    pub fn meta(&self, name: &str) -> Result<Meta> {
        let path = self.meta_path(name);
        let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn expected(&self, name: &str, engine: Engine) -> Result<Option<Expected>> {
        let path = self.expected_path(name, engine);
        match std::fs::read_to_string(&path) {
            Ok(text) => Expected::from_json(&text).map(Some).with_context(|| format!("in {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn check(&self, name: &str, engine: Engine) -> Result<Outcome> {
        let Some(expected) = self.expected(name, engine)? else { return Ok(Outcome::Missing) };
        let actual = replay(engine, &self.recording(name)?)?;
        let diff = compare(&expected, &actual);
        Ok(if diff.is_empty() { Outcome::Match { screens: actual.screens.len() } } else { Outcome::Mismatch(diff) })
    }

    /// (Re)writes `expected.<engine>.json` from a fresh replay.
    pub fn bless(&self, name: &str, engine: Engine) -> Result<Blessed> {
        let actual = replay(engine, &self.recording(name)?)?;
        let previous = self.expected(name, engine).ok().flatten();
        let path = self.expected_path(name, engine);
        let result = match &previous {
            None => Blessed::New,
            Some(previous) => {
                let diff = compare(previous, &actual);
                if diff.is_empty() { Blessed::Unchanged } else { Blessed::Updated(diff) }
            }
        };
        let text = actual.to_json();
        if result != Blessed::Unchanged || std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
        }
        Ok(result)
    }

    /// Writes a new or re-recorded entry (stream, meta and the input script).
    pub fn write_entry(&self, name: &str, recording: &Recording, meta: &Meta, script: Option<&str>) -> Result<()> {
        ensure!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "entry names are [A-Za-z0-9_-]"
        );
        let dir = self.entry_dir(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        recording.write(&self.stream_path(name))?;
        let meta_text = serde_json::to_string_pretty(meta)? + "\n";
        std::fs::write(self.meta_path(name), meta_text)?;
        let script_path = dir.join("input.script");
        match script {
            Some(script) => std::fs::write(&script_path, script)?,
            None => {
                let _ = std::fs::remove_file(&script_path);
            }
        }
        Ok(())
    }
}
