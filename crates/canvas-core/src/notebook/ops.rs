//! What the notebook does to files and the canvas: create, rename, move,
//! trash, mark reviewed, quick capture, open externally, *Ask agent to
//! revise…*, the scripts switch of HTML notes and the settings.

use canvas_model::notebook::{self as nb, Kind, Saved};

use super::tabs::{self, View};
use super::{Action, file_name, is_inside, join, load, norm, parent, same_path, tree};
use crate::app::{App, Focus};
use crate::dialogs::{Action as AppAction, Confirm, Modal};
use crate::spaces::{NavTarget, Space};

/// `<root>/Inbox` (`None` before the notebook's folders are known).
pub(super) fn inbox(app: &App) -> Option<String> {
    app.notebook.info.as_ref().map(|i| join(&i.root, nb::INBOX))
}

/// The quick note's file name for a local time: `2026-10-31 1405.md`.
pub fn capture_name(local: &crate::bus::Local, attempt: u32) -> String {
    let stamp = format!("{} {:02}{:02}", local.date(), local.hour, local.minute);
    if attempt <= 1 { format!("{stamp}.md") } else { format!("{stamp}-{attempt}.md") }
}

/// A new note's text: its provenance (written by you, for you), a title heading for Markdown.
pub fn new_text(kind: Kind, title: &str) -> String {
    nb::your_new_note(kind, title, &crate::paths::rfc3339_now())
}

/// Quick capture (Alt+N anywhere outside terminals, `+ Capture`): a new note
/// `<root>/Inbox/<yyyy-mm-dd HHMM>.md` (for you, by you), opened in Source mode.
pub(super) fn capture(app: &mut App) {
    let Some(inbox) = inbox(app) else {
        app.notebook.pending_capture = true;
        load::ensure_info(app);
        return;
    };
    let roots = load::root_paths(app);
    let local = crate::bus::Local::now();
    let text = new_text(Kind::Markdown, "");
    load::background(
        app,
        move |backend| {
            // Never over another note: `-2`, `-3`… within the same minute.
            for attempt in 1..=20 {
                let path = join(&inbox, &capture_name(&local, attempt));
                match backend.note_write(&path, &text, None, &roots) {
                    Ok(Saved::Written(_)) => return Ok(path),
                    Ok(Saved::Conflict(_)) => continue,
                    Err(error) => return Err(format!("{error:#}")),
                }
            }
            Err("too many quick notes this minute".into())
        },
        |app, result| match result {
            Ok(path) => opened_new(app, &path, true),
            Err(error) => app.notify(format!("Quick note not written: {error}"), true),
        },
    );
}

/// A note written here: its folder is listed again and it opens in Source mode.
fn opened_new(app: &mut App, path: &str, at_end: bool) {
    if let Some(dir) = parent(path) {
        load::relist(app, &dir);
    }
    app.navigate(Space::Notebook, NavTarget::Note(path.to_owned()));
    tabs::set_view(app, View::Source);
    if let Some(index) = app.notebook.tab_index(path) {
        app.notebook.tabs[index].caret_end = at_end;
    }
    app.set_focus(Focus::Canvas);
}

/// A name typed for a file or folder: one name (no separators), not empty.
fn checked_name(name: &str) -> Result<String, String> {
    nb::typed_name(name)
}

/// Creates a note in `folder` (asked from the name dialog; `.md` added
/// unless the name has a note extension) and opens it in Source mode.
pub(super) fn create_note(app: &mut App, folder: &str, name: &str) -> Result<(), String> {
    let name = checked_name(name)?;
    let file = if matches!(Kind::of(&name), Kind::Markdown | Kind::Html | Kind::Text) { name } else { format!("{name}.md") };
    let kind = Kind::of(&file);
    let path = join(folder, &file);
    let text = new_text(kind, nb::stem(&file));
    let roots = load::root_paths(app);
    let asked = path.clone();
    load::background(
        app,
        move |backend| backend.note_write(&asked, &text, None, &roots).map_err(|e| format!("{e:#}")),
        move |app, result| match result {
            Ok(Saved::Written(_)) => opened_new(app, &path, true),
            Ok(Saved::Conflict(_)) => app.notify(format!("{} exists already.", file_name(&path)), true),
            Err(error) => app.notify(format!("Could not create {}: {error}", file_name(&path)), true),
        },
    );
    Ok(())
}

/// Creates a folder in `folder`.
pub(super) fn create_folder(app: &mut App, folder: &str, name: &str) -> Result<(), String> {
    let path = join(folder, &checked_name(name)?);
    let roots = load::root_paths(app);
    let (asked, parent_dir) = (path.clone(), folder.to_owned());
    load::background(
        app,
        move |backend| backend.note_mkdir(&asked, &roots).map_err(|e| format!("{e:#}")),
        move |app, result| match result {
            Ok(()) => {
                load::relist(app, &parent_dir);
                super::explorer::reveal(app, &path, true);
            }
            Err(error) => app.notify(format!("Could not create the folder: {error}"), true),
        },
    );
    Ok(())
}

/// Renames a note or folder in its folder (a name without extension keeps the old one).
pub(super) fn rename(app: &mut App, path: &str, name: &str) -> Result<(), String> {
    let mut name = checked_name(name)?;
    let old = file_name(path);
    let is_folder = super::explorer::is_folder(app, path);
    if !is_folder && !name.contains('.') && let Some((_, ext)) = old.rsplit_once('.') {
        name = format!("{name}.{ext}");
    }
    if name == old {
        return Ok(());
    }
    let dir = parent(path).ok_or("A root cannot be renamed here.")?;
    relocate(app, path, &join(&dir, &name))
}

/// Moves a note or folder into `folder`.
pub(super) fn move_to(app: &mut App, path: &str, folder: &str) -> Result<(), String> {
    if parent(path).is_some_and(|p| same_path(&p, folder)) {
        return Err("It is in that folder already.".into());
    }
    if is_inside(folder, path) {
        return Err("A folder cannot move into itself.".into());
    }
    relocate(app, path, &join(folder, &file_name(path)))
}

/// `NoteRename` from → to; open tabs follow, both folders are listed again.
/// Tabs being saved wait (refused); the others neither save nor poll until
/// the answer (a save then would write the old path again).
fn relocate(app: &mut App, from: &str, to: &str) -> Result<(), String> {
    let state = &mut app.notebook;
    if state.tabs.iter().chain(&state.closing).any(|t| t.saving && is_inside(&t.path, from)) {
        return Err(format!("{} is being saved: try again in a moment.", file_name(from)));
    }
    let moving: Vec<u64> = state.tabs.iter_mut().filter(|t| is_inside(&t.path, from)).map(|t| {
        t.moving = true;
        t.id
    }).collect();
    let roots = load::root_paths(app);
    let (from, to) = (from.to_owned(), to.to_owned());
    let (asked_from, asked_to) = (from.clone(), to.clone());
    load::background(
        app,
        move |backend| backend.note_rename(&asked_from, &asked_to, &roots).map_err(|e| format!("{e:#}")),
        move |app, result| {
            for id in &moving {
                if let Some(tab) = app.notebook.tab_by_id(*id) {
                    tab.moving = false;
                }
            }
            match result {
                Ok(()) => {
                    retarget(app, &from, &to);
                    for dir in [parent(&from), parent(&to)].into_iter().flatten() {
                        load::relist(app, &dir);
                    }
                    super::explorer::reveal(app, &to, false);
                }
                Err(error) => app.notify(format!("Could not move {}: {error}", file_name(&from)), true),
            }
        },
    );
    Ok(())
}

/// `path` after `from` moved to `to` (`None`: not inside `from`).
fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    if same_path(path, from) {
        return Some(to.to_owned());
    }
    if !is_inside(path, from) {
        return None;
    }
    let rest = tree::rel(from, path);
    Some(rest.split('/').filter(|p| !p.is_empty()).fold(to.to_owned(), |dir, part| join(&dir, part)))
}

/// Tabs of a moved note (or of notes in a moved folder) follow it, as do
/// the known notes.
pub(super) fn retarget(app: &mut App, from: &str, to: &str) {
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    let from_key = norm(from);
    for tab in app.notebook.tabs.iter_mut().chain(app.notebook.closing.iter_mut()) {
        let Some(new_path) = moved(&tab.path, from, to) else { continue };
        tab.path = new_path;
        tab.name = file_name(&tab.path);
        tab.missing = false;
        if let Some(root) = tree::root_of(&roots, &tab.path) {
            tab.rel = tree::rel(&root.path, &tab.path);
            tab.place = root.place;
        }
        tab.classify();
    }
    for known in &mut app.notebook.known {
        if let Some(new_path) = moved(&known.path, from, to) {
            known.name = file_name(&new_path);
            known.path = new_path;
        }
    }
    if app.notebook.selected.as_deref() == Some(from_key.as_str()) {
        app.notebook.selected = Some(norm(to));
    }
}

/// Asks before moving a note or folder to its root's trash.
pub(super) fn confirm_trash(app: &mut App, path: &str) {
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    let Some(root) = tree::root_of(&roots, path).filter(|r| !same_path(&r.path, path)) else {
        return app.notify("Only notes and folders inside the notebook's roots go to the trash.", true);
    };
    app.modals.push(Modal::Confirm(Confirm {
        title: format!("Delete {}?", file_name(path)),
        body: vec![format!("It moves to {} (it can be taken back from there).", join(&root.path, nb::TRASH)), "Open tabs of it close without saving.".into()],
        buttons: vec![("Move to the trash".into(), AppAction::Notebook(Action::Trash(path.to_owned()))), ("Cancel".into(), AppAction::Nothing)],
        sel: 0,
    }));
}

/// Moves a note or folder to its root's `.trash`; its tabs close.
pub(super) fn trash(app: &mut App, path: String) {
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    let Some(root) = tree::root_of(&roots, &path).map(|r| r.path.clone()) else { return };
    let asked = path.clone();
    load::background(
        app,
        move |backend| backend.note_trash(&asked, &root).map_err(|e| format!("{e:#}")),
        move |app, result| match result {
            Ok(()) => {
                let state = &mut app.notebook;
                let before = state.tabs.len();
                let shown = state.tab().map(|t| t.path.clone());
                state.tabs.retain(|t| !is_inside(&t.path, &path));
                state.closing.retain(|t| !is_inside(&t.path, &path));
                state.known.retain(|k| !is_inside(&k.path, &path));
                state.backlinks.retain(|key, _| !is_inside(key, &path));
                if state.tabs.len() != before {
                    state.active = shown.and_then(|p| state.tab_index(&p)).unwrap_or(state.tabs.len().saturating_sub(1));
                }
                if let Some(dir) = parent(&path) {
                    load::relist(app, &dir);
                }
                app.notify(format!("{} is in the trash.", file_name(&path)), false);
            }
            Err(error) => app.notify(format!("Could not delete {}: {error}", file_name(&path)), true),
        },
    );
}

/// A note in its program, a folder in the file manager (remote: VS Code or the viewer).
pub(super) fn open_externally(app: &mut App, path: &str) {
    if super::explorer::is_folder(app, path) {
        app.open_folder(path);
    } else {
        app.open_target(path);
    }
}

/// Writes `reviewed: true` into a note (read, changed and written based on
/// its revision; an open clean tab shows the result). A tab with unsaved
/// changes takes the mark into its buffer and is saved with them.
pub(super) fn mark_reviewed(app: &mut App, path: String) {
    let kind = Kind::of(&file_name(&path));
    if !matches!(kind, Kind::Markdown | Kind::Html) {
        return app.notify("Only Markdown and HTML notes carry a reviewed mark.", true);
    }
    let change = [("reviewed", Some("true"))];
    if let Some(index) = app.notebook.tab_index(&path).filter(|i| app.notebook.tabs[*i].editable() && app.notebook.tabs[*i].unsaved) {
        let tab = &mut app.notebook.tabs[index];
        let text = tab.text();
        let marked = nb::set_meta(kind, &text, &change);
        // An undo step of its own; the caret stays on its text.
        if tab.editor.apply(&marked) {
            tab.changed();
            tab.classify();
        }
        tabs::save(app, index, true);
        return app.notify("Marked reviewed.", false);
    }
    let roots = load::root_paths(app);
    let asked = path.clone();
    load::background(
        app,
        move |backend| -> Result<Option<(String, nb::NoteText)>, String> {
            let note = backend.note_read(&asked).map_err(|e| format!("{e:#}"))?;
            let marked = nb::set_meta(kind, &note.text, &change);
            if marked == note.text {
                return Ok(None);
            }
            match backend.note_write(&asked, &marked, Some(&note.rev), &roots).map_err(|e| format!("{e:#}"))? {
                Saved::Written(rev) => Ok(Some((note.rev, nb::NoteText { text: marked, rev }))),
                Saved::Conflict(_) => Err("it changed meanwhile; try again".into()),
            }
        },
        move |app, result| match result {
            Ok(written) => {
                if let (Some((read, note)), Some(index)) = (written, app.notebook.tab_index(&path)) {
                    let tab = &mut app.notebook.tabs[index];
                    if tab.unsaved && !tab.saving && tab.conflict.is_none() && tab.rev.as_deref() == Some(read.as_str()) {
                        // Typed into meanwhile, on the version marked: the mark joins the buffer, now based on the marked file.
                        let marked = nb::set_meta(kind, &tab.text(), &change);
                        tab.editor.apply(&marked);
                        tab.rev = Some(note.rev);
                        tab.changed();
                        tab.classify();
                    } else {
                        tabs::changed_on_disk(tab, note);
                    }
                }
                if let Some(dir) = parent(&path) {
                    load::relist(app, &dir);
                }
                app.notify(format!("{} marked reviewed.", file_name(&path)), false);
            }
            Err(error) => app.notify(format!("Could not mark {} reviewed: {error}", file_name(&path)), true),
        },
    );
}

/// *Ask agent to revise…*: the note's path (and what to change) as a
/// message to a session, typed in when it is idle (as `atelier send`).
pub(super) fn ask_revision(app: &mut App, node: &str, name: &str, path: &str, what: &str) {
    let text = revision_message(path, what);
    let group = app.doc.node(node).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()).unwrap_or_default();
    let event = crate::bus::Event {
        kind: "message".into(),
        group,
        to: node.to_owned(),
        source_name: "you".into(),
        data: serde_json::json!({ "text": text }),
        canvas: app.doc.canvas_id.clone().unwrap_or_default(),
        ..Default::default()
    };
    if !app.connected {
        return app.notify("Offline: the message was not sent.", true);
    }
    app.bus_action(serde_json::json!({ "action": "emit", "event": event }));
    app.notify(format!("Asked “{name}” to revise {} (typed in when it is idle).", file_name(path)), false);
}

/// The message *Ask agent to revise…* sends.
pub fn revision_message(path: &str, what: &str) -> String {
    let what = what.trim();
    if what.is_empty() { format!("Please revise the note {path} (read it, update it in place, keep its front matter).") } else { format!("Please revise the note {path}: {what} (update it in place, keep its front matter).") }
}

/// Lets an HTML note run its own scripts (offline only), or stops them.
pub(super) fn set_scripts(app: &mut App, path: &str, on: bool) {
    let had = app.doc.notebook_scripts.iter().any(|p| same_path(p, path));
    if on && !had {
        app.doc.notebook_scripts.push(path.to_owned());
        app.notify(format!("Scripts of {} may run (offline: no network).", file_name(path)), false);
    } else if !on && had {
        app.doc.notebook_scripts.retain(|p| !same_path(p, path));
        app.notify(format!("Scripts of {} are off.", file_name(path)), false);
    } else {
        return;
    }
    app.mark_dirty();
}

/// The Settings row *Notebook folder* (empty: `<home>/notebook`).
pub(super) fn set_root(app: &mut App, text: &str) {
    let value = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
    if value == app.doc.notebook_root {
        return;
    }
    app.doc.notebook_root = value;
    app.mark_dirty();
    app.notebook.expanded.clear();
    app.notebook.root_opened = false;
    app.notebook.selected = None;
    load::ensure_info(app);
    app.notify("Notebook folder changed.", false);
}

/// *Notebook mounts…*: adds a folder (once).
pub(super) fn add_mount(app: &mut App, path: &str) {
    if app.doc.notebook_mounts.iter().any(|m| same_path(m, path)) {
        return;
    }
    app.doc.notebook_mounts.push(path.to_owned());
    app.mark_dirty();
    app.notebook.expanded.insert(norm(path));
}

/// *Notebook mounts…*: removes one.
pub(super) fn remove_mount(app: &mut App, index: usize) {
    if index < app.doc.notebook_mounts.len() {
        app.doc.notebook_mounts.remove(index);
        app.mark_dirty();
    }
}

/// Writes every unsaved note now, waiting for the answers (for quitting:
/// the last second of typing is not lost). A note whose save is on its way
/// is written again on top of it; one that cannot be written (a conflict
/// you did not settle, a refused write) keeps your text in a copy beside
/// it, `<name> (unsaved <date> <time>).<ext>`. What could not be kept at
/// all is reported in `save_error`.
pub(super) fn flush(app: &mut App) {
    let roots = load::root_paths(app);
    let backend = app.backend.clone();
    let local = crate::bus::Local::now();
    let mut lost = Vec::new();
    for tab in app.notebook.tabs.iter_mut().chain(app.notebook.closing.iter_mut()) {
        if !tab.unsaved || !tab.editable() {
            continue;
        }
        let text = tab.text();
        if tab.conflict.is_none() && write_now(&*backend, tab, &text, &roots) {
            continue;
        }
        if let Err(error) = write_copy(&*backend, &tab.path, &text, &roots, &local) {
            lost.push(format!("{} ({error})", tab.name));
        }
    }
    if !lost.is_empty() {
        app.save_error = Some(format!("notes not saved: {}", lost.join(", ")));
    }
}

/// Writes a tab's text over its revision (a gone file: anew); true when written.
fn write_now(backend: &dyn crate::rpc::Backend, tab: &mut tabs::Tab, text: &str, roots: &[String]) -> bool {
    let base = if tab.missing { None } else { tab.rev.clone() };
    let mut answer = backend.note_write(&tab.path, text, base.as_deref(), roots);
    // The write on its way when quitting landed first: on top of it.
    if let Ok(Saved::Conflict(theirs)) = &answer
        && tab.sent.as_ref().is_some_and(|sent| nb::rev_of(sent.as_bytes()) == theirs.rev)
    {
        answer = backend.note_write(&tab.path, text, Some(&theirs.rev), roots);
    }
    match answer {
        Ok(Saved::Written(rev)) => {
            tab.rev = Some(rev);
            tab.unsaved = false;
            true
        }
        _ => false,
    }
}

/// Keeps `text` (a note that could not be saved) in a new file beside it.
fn write_copy(backend: &dyn crate::rpc::Backend, path: &str, text: &str, roots: &[String], local: &crate::bus::Local) -> Result<String, String> {
    let dir = parent(path).ok_or("no folder")?;
    let name = file_name(path);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    let stamp = format!("{} {:02}{:02}", local.date(), local.hour, local.minute);
    for attempt in 1..=20 {
        let suffix = if attempt == 1 { String::new() } else { format!("-{attempt}") };
        let copy = join(&dir, &format!("{stem} (unsaved {stamp}{suffix}){ext}"));
        match backend.note_write(&copy, text, None, roots) {
            Ok(Saved::Written(_)) => return Ok(copy),
            Ok(Saved::Conflict(_)) => continue,
            Err(error) => return Err(format!("{error:#}")),
        }
    }
    Err("too many copies".into())
}
