//! Everything the notebook asks the backend, on background threads: the
//! answers continue on the UI thread (`AppEvent::Run`). Nothing here blocks
//! the UI.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use canvas_model::notebook::{Audience, Entry, Hit};

use super::{Folder, KNOWN_EVERY, KNOWN_MAX, Known, Load, POLL_EVERY, REFRESH_EVERY, norm, tree};
use crate::app::{App, AppEvent};

/// How long a failed `NotebookInfo` waits before it is asked again.
const INFO_RETRY: Duration = Duration::from_secs(10);
/// Folders the known-notes walk lists at most.
const KNOWN_FOLDERS: usize = 60;

/// Runs `then` on the UI thread with the result of `call` (a backend call
/// made on a background thread).
pub(super) fn background<T: Send + 'static>(app: &App, call: impl FnOnce(&dyn crate::rpc::Backend) -> T + Send + 'static, then: impl FnOnce(&mut App, T) + Send + 'static) {
    app.in_background(call, move |result| {
        AppEvent::Run(Box::new(move |app: &mut App| {
            then(app, result);
            app.needs_draw = true;
        }))
    });
}

/// Every explorer root's path (writes, searches).
pub(super) fn root_paths(app: &App) -> Vec<String> {
    tree::roots(app.notebook.info.as_ref(), &app.doc).into_iter().map(|r| r.path).collect()
}

/// Resolves the notebook's folders (`NotebookInfo`) once per setting; the
/// notebook root is expanded and listed the first time.
pub(super) fn ensure_info(app: &mut App) {
    let key = (app.doc.notebook_root.clone().filter(|r| !r.trim().is_empty()), app.doc.canvas_id.clone());
    let state = &mut app.notebook;
    if state.info_key.as_ref() != Some(&key) {
        // The setting changed (here or in another window): ask again.
        let first = state.info_key.is_none();
        state.info_key = Some(key.clone());
        state.info = None;
        state.info_load = Load::Idle;
        state.info_retry_at = None;
        if !first {
            state.folders.clear();
            state.known.clear();
            state.known_at = None;
        }
    }
    match &state.info_load {
        Load::Loading => return,
        Load::Failed(_) if state.info_retry_at.is_some_and(|t| Instant::now() < t) => return,
        _ if state.info.is_some() => return,
        _ => {}
    }
    state.info_load = Load::Loading;
    let (root, canvas) = key.clone();
    background(
        app,
        move |backend| backend.notebook_info(root.as_deref(), canvas.as_deref()).map_err(|e| format!("{e:#}")),
        move |app, result| {
            if app.notebook.info_key.as_ref() != Some(&key) {
                return;
            }
            match result {
                Ok(info) => {
                    let first = !std::mem::replace(&mut app.notebook.root_opened, true);
                    let root = info.root.clone();
                    app.notebook.info = Some(info);
                    app.notebook.info_load = Load::Idle;
                    if first {
                        app.notebook.expanded.insert(norm(&root));
                    }
                    list_expanded(app);
                    super::tabs::reclassify(app);
                    if let Some((path, expand)) = app.notebook.pending_reveal.take() {
                        super::explorer::reveal(app, &path, expand);
                    }
                    if std::mem::take(&mut app.notebook.pending_capture) {
                        super::ops::capture(app);
                    }
                }
                Err(error) => {
                    if std::mem::take(&mut app.notebook.pending_capture) {
                        app.notify(format!("Quick note not written: the notebook is not available ({error})."), true);
                    }
                    app.notebook.info_load = Load::Failed(error);
                    app.notebook.info_retry_at = Some(Instant::now() + INFO_RETRY);
                }
            }
        },
    );
}

/// Asks for the notebook's folders again now.
pub(super) fn retry_info(app: &mut App) {
    app.notebook.info_load = Load::Idle;
    app.notebook.info_retry_at = None;
    app.notebook.info = None;
    ensure_info(app);
}

/// Lists a folder (`root`: the explorer root it is under) unless that is on its way.
pub(super) fn list(app: &mut App, dir: &str, root: &str) {
    let key = norm(dir);
    let folder = app.notebook.folders.entry(key.clone()).or_insert_with(|| Folder { path: dir.to_owned(), root: root.to_owned(), ..Folder::default() });
    if folder.loading {
        return;
    }
    folder.loading = true;
    let (dir, root) = (dir.to_owned(), root.to_owned());
    background(
        app,
        move |backend| backend.notebook_list(&dir, Some(&root)).map_err(|e| format!("{e:#}")),
        move |app, result| {
            let Some(folder) = app.notebook.folders.get_mut(&key) else { return };
            folder.loading = false;
            match result {
                Ok(entries) => {
                    folder.entries = entries;
                    folder.error = None;
                    folder.at = Some(Instant::now());
                    let entries = folder.entries.clone();
                    remember(app, &entries);
                }
                Err(error) => {
                    folder.error = Some(format!("Could not list it: {error}"));
                    // A folder gone since: it is listed as empty.
                    if folder.at.is_some() {
                        folder.entries.clear();
                    }
                }
            }
        },
    );
}

/// Lists every expanded folder that is shown and not listed yet.
pub(super) fn list_expanded(app: &mut App) {
    let wanted: Vec<(String, String)> = tree::tree(app)
        .rows
        .iter()
        .filter(|r| r.expanded)
        .filter_map(|r| match &r.kind {
            tree::RowKind::Root(root) => Some((root.path.clone(), root.path.clone())),
            tree::RowKind::Dir(entry, root) => Some((entry.path.clone(), root.clone())),
            _ => None,
        })
        .filter(|(path, _)| !app.notebook.folders.contains_key(&norm(path)))
        .collect();
    for (path, root) in wanted {
        list(app, &path, &root);
    }
}

/// Every few seconds while the Notebook shows: the expanded folders again.
pub(super) fn refresh(app: &mut App) {
    if app.notebook.refresh_at.is_some_and(|t| t.elapsed() < REFRESH_EVERY) {
        return;
    }
    refresh_all(app);
}

/// Lists the expanded (shown) folders again now.
pub(super) fn refresh_all(app: &mut App) {
    app.notebook.refresh_at = Some(Instant::now());
    let shown: Vec<(String, String)> = tree::tree(app)
        .rows
        .iter()
        .filter(|r| r.expanded)
        .filter_map(|r| match &r.kind {
            tree::RowKind::Root(root) => Some((root.path.clone(), root.path.clone())),
            tree::RowKind::Dir(entry, root) => Some((entry.path.clone(), root.clone())),
            _ => None,
        })
        .collect();
    for (path, root) in shown {
        list(app, &path, &root);
    }
}

/// Lists a folder again (after a change made here) when it was listed.
pub(super) fn relist(app: &mut App, dir: &str) {
    let Some(folder) = app.notebook.folders.get(&norm(dir)) else { return };
    let (path, root) = (folder.path.clone(), folder.root.clone());
    list(app, &path, &root);
}

/// Listed notes join the known notes (Go to, `[[` completion).
fn remember(app: &mut App, entries: &[Entry]) {
    let known = &mut app.notebook.known;
    // By normalized path (a listing can hold thousands of notes).
    let mut at: HashMap<String, usize> = known.iter().enumerate().map(|(i, k)| (norm(&k.path), i)).collect();
    for entry in entries.iter().filter(|e| !e.is_dir() && !e.provenance.internal && e.kind.is_text()) {
        let item = Known { path: entry.path.clone(), name: entry.name.clone(), title: entry.title.clone(), ai: entry.provenance.audience == Audience::Ai };
        let key = norm(&entry.path);
        match at.get(&key) {
            Some(&i) => known[i] = item,
            None if known.len() < KNOWN_MAX => {
                at.insert(key, known.len());
                known.push(item);
            }
            None => {}
        }
    }
    super::tabs::refresh_completion(app);
}

/// Now and then while the Notebook shows: walks the roots (breadth first,
/// at most [`KNOWN_FOLDERS`] folders) for the known-notes list.
pub(super) fn refresh_known(app: &mut App) {
    let state = &app.notebook;
    if state.known_loading || state.info.is_none() || state.known_at.is_some_and(|t| t.elapsed() < KNOWN_EVERY) {
        return;
    }
    let roots = root_paths(app);
    if roots.is_empty() {
        return;
    }
    app.notebook.known_loading = true;
    app.notebook.known_at = Some(Instant::now());
    background(
        app,
        move |backend| {
            let mut queue: VecDeque<(String, String)> = roots.iter().map(|r| (r.clone(), r.clone())).collect();
            let (mut notes, mut listed) = (Vec::new(), 0);
            while let Some((dir, root)) = queue.pop_front() {
                if listed >= KNOWN_FOLDERS || notes.len() >= KNOWN_MAX {
                    break;
                }
                listed += 1;
                let Ok(entries) = backend.notebook_list(&dir, Some(&root)) else { continue };
                for entry in entries.into_iter().filter(|e| !e.provenance.internal) {
                    if entry.is_dir() {
                        if !entry.name.eq_ignore_ascii_case("node_modules") {
                            queue.push_back((entry.path, root.clone()));
                        }
                    } else if entry.kind.is_text() && notes.len() < KNOWN_MAX {
                        notes.push(entry);
                    }
                }
            }
            notes
        },
        |app, mut notes| {
            app.notebook.known_loading = false;
            // The walk replaces the list (notes deleted or moved elsewhere
            // meanwhile go); the listed folders' notes stay.
            app.notebook.known.clear();
            notes.extend(app.notebook.folders.values().flat_map(|f| f.entries.iter().cloned()));
            remember(app, &notes);
        },
    );
}

/// Every [`POLL_EVERY`]: the open notes' revisions (`NoteRevs`).
pub(super) fn poll(app: &mut App) {
    let state = &app.notebook;
    if state.polling || state.tabs.is_empty() || state.poll_at.is_some_and(|t| t.elapsed() < POLL_EVERY) {
        return;
    }
    let paths: Vec<String> = state.tabs.iter().filter(|t| t.rev.is_some()).map(|t| t.path.clone()).collect();
    app.notebook.poll_at = Some(Instant::now());
    if paths.is_empty() {
        return;
    }
    app.notebook.polling = true;
    let asked = paths.clone();
    background(
        app,
        move |backend| backend.note_revs(paths),
        move |app, result| {
            app.notebook.polling = false;
            if let Ok(revs) = result {
                for (path, rev) in asked.into_iter().zip(revs) {
                    super::tabs::on_rev(app, &path, rev);
                }
            }
        },
    );
}

/// Searches the notebook's roots (names and contents); `then` gets the hits.
pub(super) fn search(app: &App, query: &str, limit: u32, then: impl FnOnce(&mut App, Result<Vec<Hit>, String>) + Send + 'static) {
    let roots = root_paths(app);
    let query = query.to_owned();
    background(app, move |backend| if roots.is_empty() { Ok(Vec::new()) } else { backend.note_search(&roots, &query, limit).map_err(|e| format!("{e:#}")) }, then);
}
