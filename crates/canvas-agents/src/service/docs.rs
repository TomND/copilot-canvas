//! Canvas documents the service owns. A document is opened under its file
//! lock, edited by every client that opened it (edits are applied in arrival
//! order, so the last edit wins per node, group or field), broadcast to those
//! clients as patches, and saved shortly after each change.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use canvas_model::doc::{self, Doc, Store};
use canvas_model::sync::{self, DocReply, Op};

use super::term::lock;

/// Changes are saved this long after the first unsaved one.
pub const SAVE_DELAY: Duration = Duration::from_millis(300);

struct OpenDoc {
    path: PathBuf,
    _store: Store,
    doc: Doc,
    version: u64,
    clients: HashSet<u64>,
    /// Since when changes are unsaved.
    dirty: Option<Instant>,
    /// The last save error (clients are told when it changes).
    error: Option<String>,
}

/// A save outcome to report to the document's clients.
pub struct Saved {
    pub key: String,
    pub version: u64,
    pub error: Option<String>,
    pub clients: Vec<u64>,
}

#[derive(Default)]
pub struct Docs {
    open: Mutex<HashMap<String, OpenDoc>>,
    /// One save at a time (the saver thread and shutdown).
    saving: Mutex<()>,
}

impl Docs {
    /// Opens a document for `client` (joining it when already open) and
    /// hands `reply` the answer: `(key, version, reply)`. `reply` runs under
    /// the documents' lock, so no patch can overtake it.
    pub fn open(&self, home: &Path, path: Option<&str>, demo: bool, cwd: &str, client: u64, reply: impl FnOnce(String, u64, DocReply)) {
        let path = match path.map(str::trim).filter(|p| !p.is_empty()) {
            Some(path) => crate::paths::absolute(Path::new(path)),
            None => home.join(if demo { "demo-canvas.json" } else { "canvas.json" }),
        };
        let shown = path.display().to_string();
        let key = sync::doc_key(&shown);
        let mut open = lock(&self.open);
        if let Some(entry) = open.get_mut(&key) {
            entry.clients.insert(client);
            let answer = DocReply { path: shown, client, doc: Some(entry.doc.clone()), error: None };
            return reply(key, entry.version, answer);
        }
        let loaded = (|| -> Result<OpenDoc> {
            let store = Store::open(&path)?;
            let (doc, created) = match store.load()? {
                Some(doc) => (doc, false),
                None if demo => (doc::demo_doc(cwd), true),
                None => (doc::migrate_v1(&home.join("layout.json")).unwrap_or_default(), true),
            };
            // A new canvas is written at once.
            let dirty = created.then(|| Instant::now().checked_sub(SAVE_DELAY).unwrap_or_else(Instant::now));
            Ok(OpenDoc { path: path.clone(), _store: store, doc, version: 1, clients: HashSet::from([client]), dirty, error: None })
        })();
        match loaded {
            Ok(entry) => {
                let answer = DocReply { path: shown, client, doc: Some(entry.doc.clone()), error: None };
                let version = entry.version;
                open.insert(key.clone(), entry);
                reply(key, version, answer);
            }
            Err(error) => reply(key, 0, DocReply { path: shown, client, doc: None, error: Some(format!("{error:#}")) }),
        }
    }

    /// The current document for a client that must resynchronize.
    pub fn snapshot(&self, key: &str, client: u64, reply: impl FnOnce(u64, DocReply)) {
        let open = lock(&self.open);
        if let Some(entry) = open.get(key).filter(|e| e.clients.contains(&client)) {
            reply(entry.version, DocReply { path: entry.path.display().to_string(), client, doc: Some(entry.doc.clone()), error: None });
        }
    }

    /// Applies an edit from `client` and hands `broadcast` the new version and
    /// the document's clients (under the lock, so patches leave in order).
    pub fn edit(&self, key: &str, client: u64, ops: &[Op], broadcast: impl FnOnce(u64, &HashSet<u64>)) -> Result<()> {
        let mut open = lock(&self.open);
        let entry = open.get_mut(key).context("this canvas is not open")?;
        anyhow::ensure!(entry.clients.contains(&client), "this client has not opened the canvas");
        let mut next = entry.doc.clone();
        for op in ops {
            sync::apply(&mut next, op)?;
        }
        next.validate()?;
        entry.doc = next;
        entry.version += 1;
        entry.dirty.get_or_insert_with(Instant::now);
        broadcast(entry.version, &entry.clients);
        Ok(())
    }

    /// A client disconnected: documents nobody has open close once saved.
    pub fn leave(&self, client: u64) {
        for entry in lock(&self.open).values_mut() {
            entry.clients.remove(&client);
        }
    }

    /// Whether any document is open.
    #[cfg(test)]
    pub fn any_open(&self) -> bool {
        !lock(&self.open).is_empty()
    }

    /// Saves the documents whose changes are due (every unsaved one when
    /// `all`, and at once for documents nobody has open), then closes
    /// documents nobody has open. Returns changed save states to report.
    pub fn save(&self, all: bool) -> Vec<Saved> {
        let _saving = lock(&self.saving);
        let due: Vec<(String, PathBuf, Doc, u64)> = lock(&self.open)
            .iter()
            .filter(|(_, e)| e.dirty.is_some_and(|since| all || e.clients.is_empty() || since.elapsed() >= SAVE_DELAY))
            .map(|(key, e)| (key.clone(), e.path.clone(), e.doc.clone(), e.version))
            .collect();
        let mut out = Vec::new();
        for (key, path, doc, version) in due {
            // Written outside the lock: edits continue meanwhile.
            let error = doc::save_doc(&path, &doc).err().map(|e| format!("{e:#}"));
            let mut open = lock(&self.open);
            let Some(entry) = open.get_mut(&key) else { continue };
            match &error {
                None if entry.version == version => entry.dirty = None,
                None => {}
                // Tried again after the delay.
                Some(_) => entry.dirty = Some(Instant::now()),
            }
            if entry.error != error {
                entry.error = error.clone();
                out.push(Saved { key, version, error, clients: entry.clients.iter().copied().collect() });
            }
        }
        lock(&self.open).retain(|_, e| !e.clients.is_empty() || e.dirty.is_some());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::doc::Node;

    fn node(id: &str) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0 })).unwrap()
    }

    #[test]
    fn documents_open_once_edit_in_order_save_and_close() {
        let home = std::env::temp_dir().join(format!("cc-docs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let docs = Docs::default();
        let mut opened = Vec::new();
        docs.open(&home, None, true, r"C:\w", 1, |key, version, reply| opened.push((key, version, reply)));
        docs.open(&home, None, true, r"C:\w", 2, |key, version, reply| opened.push((key, version, reply)));
        let (key, version, reply) = &opened[0];
        assert_eq!((*version, reply.client, opened[1].0 == *key, opened[1].2.client), (1, 1, true, 2));
        assert!(reply.doc.as_ref().unwrap().node("demo-main").is_some(), "a new demo canvas");
        let path = PathBuf::from(&reply.path);
        assert!(Store::open(&path).is_err(), "the service holds the lock");
        assert!(docs.save(false).is_empty() && path.is_file(), "a new canvas is saved at once");
        let mut patches = Vec::new();
        docs.edit(key, 1, &[Op::PutNode(Box::new(node("x")))], |v, clients| patches.push((v, clients.len()))).unwrap();
        assert!(docs.edit(key, 3, &[Op::DelNode("x".into())], |_, _| {}).is_err(), "only clients that opened it");
        let duplicate = Op::PutGroup(Box::new(canvas_model::doc::Group { id: "x".into(), w: 1, h: 1, ..Default::default() }));
        assert!(docs.edit(key, 2, &[duplicate], |_, _| {}).is_err(), "an edit that breaks the canvas is refused");
        assert_eq!(patches, vec![(2, 2)]);
        docs.save(true);
        let saved: Doc = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.node("x").is_some());
        docs.leave(1);
        docs.leave(2);
        docs.save(false);
        assert!(!docs.any_open() && Store::open(&path).is_ok(), "closed and unlocked when nobody has it open");
        let mut again = None;
        docs.open(&home, None, true, r"C:\w", 4, |_, version, reply| again = Some((version, reply)));
        let (version, reply) = again.unwrap();
        assert_eq!(version, 1);
        assert!(reply.doc.unwrap().node("x").is_some(), "reopened from the file");
        std::fs::write(home.join("broken.json"), "{").unwrap();
        let mut failed = None;
        docs.open(&home, Some(&home.join("broken.json").display().to_string()), false, "", 5, |_, _, reply| failed = reply.error);
        assert!(failed.unwrap().contains("not a valid canvas"));
        drop(docs);
        std::fs::remove_dir_all(home).unwrap();
    }
}
