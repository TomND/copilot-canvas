//! Open notes: tabs, loading, saving (autosave, conflicts, `author: mixed`),
//! change polling results, find, `[[` completion and the reader's keys,
//! text and mouse.

use std::sync::Arc;
use std::time::{Duration, Instant};

use canvas_model::notebook::{self as nb, Author, Kind, Meta, NoteText, Place, Provenance, Saved};

use super::{AUTOSAVE_AFTER, Completion, Known, file_name, load, norm, parent, same_path, tree};
use crate::app::App;
use crate::dialogs::TextInput;
use crate::editor::{Editor, Mode, Outcome, Pos};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::spaces::{NavTarget, Space};

/// Most tabs open at once (the oldest saved one closes).
pub const MAX_TABS: usize = 20;
/// Largest image the reader shows (its bytes travel inside the page).
pub const IMAGE_MAX: u64 = 1_400_000;
/// A failed save is tried again this long after.
const SAVE_RETRY: Duration = Duration::from_secs(5);

/// How the reader shows a note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// Formatted (the GUI's HTML pane; cells in the TUI).
    #[default]
    Rendered,
    /// The editor on the source.
    Source,
    /// The editor's live preview (Markdown).
    Live,
}

impl View {
    pub const ALL: [View; 3] = [View::Rendered, View::Source, View::Live];

    pub fn label(self) -> &'static str {
        match self {
            View::Rendered => "Rendered",
            View::Source => "Source",
            View::Live => "Live",
        }
    }
}

/// The find bar (`Ctrl+F`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Find {
    pub input: TextInput,
    /// Rendered view: the line of the current match.
    pub line: Option<usize>,
}

impl Find {
    /// The query the editor and the rendered view look for (`None`: only spaces typed).
    pub fn query(&self) -> Option<&str> {
        Some(self.input.text.as_str()).filter(|q| !q.trim().is_empty())
    }
}

/// Lowercased character by character, as the editor's find compares.
fn lower(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Ids of tabs: background answers find their tab by it (its path may change).
static NEXT_TAB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One open note.
#[derive(Clone, Debug)]
pub struct Tab {
    /// Unique in this process.
    pub id: u64,
    pub path: String,
    pub name: String,
    pub kind: Kind,
    pub view: View,
    pub editor: Editor,
    /// The revision the buffer is based on (`None`: not read yet).
    pub rev: Option<String>,
    pub loaded: bool,
    pub error: Option<String>,
    /// Changes not written yet.
    pub unsaved: bool,
    /// Bumped on every change of the text (typing, loading, reloading).
    pub version: u64,
    pub changed_at: Option<Instant>,
    pub saving: bool,
    /// The text being written (while `saving`).
    pub sent: Option<Arc<String>>,
    pub save_error: Option<(String, Instant)>,
    /// The file changed on disk while the buffer had changes: theirs.
    pub conflict: Option<NoteText>,
    /// The file is gone (polling): saving writes it again.
    pub missing: bool,
    /// A rename or move of it is on its way: no saves, no polling meanwhile.
    pub moving: bool,
    /// A read of the newer version is on its way.
    pub reading: bool,
    pub provenance: Provenance,
    pub meta: Meta,
    pub title: String,
    /// Relative to its explorer root, and the root's place.
    pub rel: String,
    pub place: Place,
    /// Rendered view: first line shown, rows and lines at the last draw.
    pub scroll: usize,
    pub rows: usize,
    pub total: usize,
    /// Rendered lines: (version, width, lines).
    pub(super) lines: Option<(u64, usize, Arc<Vec<crate::richtext::Line>>)>,
    /// The page for the GUI's pane: (version, page).
    pub(super) document: Option<(u64, Arc<String>)>,
    /// Rendered find: (version, width, lowercased query, matching lines).
    pub(super) found: Option<(u64, usize, String, Arc<Vec<usize>>)>,
    /// An image's page (or why there is none).
    pub(super) image: Option<Result<Arc<String>, String>>,
    /// A new note: the caret goes to its end once it is read.
    pub caret_end: bool,
}

impl Tab {
    pub fn new(path: &str, root: Option<&tree::Root>) -> Self {
        let name = file_name(path);
        let kind = Kind::of(&name);
        let view = if kind == Kind::Text { View::Source } else { View::Rendered };
        let mut editor = Editor::new("");
        editor.set_read_only(true);
        let (rel, place) = match root {
            Some(root) => (tree::rel(&root.path, path), root.place),
            None => (name.clone(), Place::Notes),
        };
        let mut tab = Tab {
            id: NEXT_TAB.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            path: path.to_owned(),
            title: nb::stem(&name).to_owned(),
            name,
            kind,
            view,
            editor,
            rev: None,
            loaded: false,
            error: None,
            unsaved: false,
            version: 0,
            changed_at: None,
            saving: false,
            sent: None,
            save_error: None,
            conflict: None,
            missing: false,
            moving: false,
            reading: false,
            provenance: Provenance::default(),
            meta: Meta::default(),
            rel,
            place,
            scroll: 0,
            rows: 0,
            total: 0,
            lines: None,
            document: None,
            found: None,
            image: None,
            caret_end: false,
        };
        tab.classify();
        tab
    }

    /// The note's text as edited.
    pub fn text(&self) -> String {
        self.editor.text()
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// Notes Source and Live can change (text files are read only).
    pub fn editable(&self) -> bool {
        self.loaded && matches!(self.kind, Kind::Markdown | Kind::Html) && self.error.is_none()
    }

    /// Reads metadata, provenance and title from the text again.
    pub fn classify(&mut self) {
        let text = self.editor.text();
        self.meta = nb::note_meta(self.kind, &text);
        self.provenance = nb::classify(&self.rel, &self.meta, self.place);
        self.title = nb::title(self.kind, &text, &self.meta, &self.name);
    }

    /// The text was loaded or reloaded from the file (`rev`).
    pub fn loaded_text(&mut self, text: &str, rev: String, keep_place: bool) {
        if keep_place {
            self.editor.reload(text);
        } else {
            self.editor.set_text(text);
            if std::mem::take(&mut self.caret_end) {
                let last = self.editor.line_count().saturating_sub(1);
                self.editor.set_cursor(Pos::new(last, usize::MAX));
            }
        }
        self.editor.set_read_only(!self.editable_kind());
        self.rev = Some(rev);
        self.loaded = true;
        self.error = None;
        self.unsaved = false;
        self.missing = false;
        self.conflict = None;
        self.version += 1;
        self.classify();
    }

    fn editable_kind(&self) -> bool {
        matches!(self.kind, Kind::Markdown | Kind::Html)
    }

    /// The text changed here.
    pub fn changed(&mut self) {
        self.unsaved = true;
        self.version += 1;
        self.changed_at = Some(Instant::now());
        self.save_error = None;
    }

    /// You changed the text: an agent's note becomes `author: mixed` with
    /// that change (the line joins its undo step; undoing it takes both back).
    pub fn edited(&mut self) {
        if self.editor.can_redo() {
            // Undone: who wrote what is is read again (the line may have gone).
            self.classify();
        } else if self.provenance.author == Author::Ai {
            let text = self.text();
            let mixed = nb::set_meta(self.kind, &text, &[("author", Some(Author::Mixed.id()))]);
            if mixed != text {
                self.editor.amend(&mixed);
            }
            self.classify();
        }
        self.changed();
    }

    /// Autosave is due.
    pub fn save_due(&self, now: Instant) -> bool {
        self.unsaved
            && !self.saving
            && !self.moving
            && self.conflict.is_none()
            && self.editable()
            && self.changed_at.is_some_and(|t| now.duration_since(t) >= AUTOSAVE_AFTER)
            && self.save_error.as_ref().is_none_or(|(_, at)| now.duration_since(*at) >= SAVE_RETRY)
    }

    /// Views the tab offers: Live only for Markdown; Rendered not for text files.
    pub fn views(&self) -> Vec<View> {
        match self.kind {
            Kind::Markdown => View::ALL.to_vec(),
            Kind::Html => vec![View::Rendered, View::Source],
            Kind::Text => vec![View::Source],
            _ => vec![View::Rendered],
        }
    }
}

// ---------- opening and closing ----------

/// Opens a note in a tab (or shows its tab) and selects it in the explorer.
pub(super) fn open_note(app: &mut App, path: &str) {
    open_note_as(app, path, None);
}

/// As [`open_note`], in `view` when given (quick capture: Source).
pub(super) fn open_note_as(app: &mut App, path: &str, view: Option<View>) {
    if let Some(index) = app.notebook.tab_index(path) {
        show_tab(app, index);
        if let Some(view) = view {
            set_view(app, view);
        }
        return;
    }
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    // Closed while its last changes are written: that tab comes back (the
    // file may not hold them yet).
    let closed = app.notebook.closing.iter().position(|t| same_path(&t.path, path));
    let reopened = closed.is_some();
    let mut tab = match closed {
        Some(at) => app.notebook.closing.remove(at),
        None => Tab::new(path, tree::root_of(&roots, path)),
    };
    if let Some(view) = view.filter(|v| tab.views().contains(v)) {
        tab.view = view;
        tab.editor.set_mode(if view == View::Live { Mode::Live } else { Mode::Source });
    }
    let state = &mut app.notebook;
    // Too many tabs: the oldest saved one (not the shown one) goes.
    if state.tabs.len() >= MAX_TABS
        && let Some(old) = state.tabs.iter().enumerate().position(|(i, t)| i != state.active && !t.unsaved && !t.saving && !t.moving)
    {
        state.tabs.remove(old);
        if old < state.active {
            state.active -= 1;
        }
    }
    let at = if state.tabs.is_empty() { 0 } else { (state.active + 1).min(state.tabs.len()) };
    state.tabs.insert(at, tab);
    state.active = at;
    drop_find(state);
    if !reopened {
        load_tab(app, path);
    }
    super::explorer::reveal(app, path, false);
    app.needs_draw = true;
}

/// Another tab shows: the find bar and the `[[` list close, and no editor
/// keeps lighting the old query's matches.
fn drop_find(state: &mut super::State) {
    state.find = None;
    state.completion = None;
    for tab in &mut state.tabs {
        tab.editor.set_find(None);
    }
}

/// The roots changed (the notebook's folders became known): each tab's
/// place and provenance again.
pub(super) fn reclassify(app: &mut App) {
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    for tab in &mut app.notebook.tabs {
        if let Some(root) = tree::root_of(&roots, &tab.path) {
            tab.rel = tree::rel(&root.path, &tab.path);
            tab.place = root.place;
            tab.classify();
        }
    }
}

/// Reads a tab's note (text kinds) or image.
pub(super) fn load_tab(app: &mut App, path: &str) {
    let Some(index) = app.notebook.tab_index(path) else { return };
    let (kind, id) = (app.notebook.tabs[index].kind, app.notebook.tabs[index].id);
    let asked = path.to_owned();
    match kind {
        Kind::Markdown | Kind::Html | Kind::Text => load::background(
            app,
            move |backend| backend.note_read(&asked).map_err(|e| format!("{e:#}")),
            move |app, result| {
                let Some(tab) = app.notebook.tabs.iter_mut().find(|t| t.id == id) else { return };
                match result {
                    Ok(note) => tab.loaded_text(&note.text, note.rev, false),
                    Err(error) => tab.error = Some(format!("Could not read it: {error}")),
                }
            },
        ),
        Kind::Image => load::background(
            app,
            move |backend| backend.read_bytes(&asked, IMAGE_MAX).map_err(|e| format!("{e:#}")),
            move |app, result| {
                let Some(tab) = app.notebook.tabs.iter_mut().find(|t| t.id == id) else { return };
                tab.loaded = true;
                tab.image = Some(result.map(|bytes| Arc::new(image_page(&tab.name, &bytes))).map_err(|e| format!("The image is not shown here: {e}")));
            },
        ),
        _ => app.notebook.tabs[index].loaded = true,
    }
}

/// A page showing an image (its bytes inline: the pane loads nothing).
pub fn image_page(name: &str, bytes: &[u8]) -> String {
    use base64::Engine;
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let alt = name.replace(['"', '<', '>', '&'], "_");
    crate::report_html::document(&format!("<p><img src=\"data:{mime};base64,{data}\" alt=\"{alt}\"></p>"), &crate::report_html::NOTEBOOK)
}

/// Shows a tab.
pub(super) fn show_tab(app: &mut App, index: usize) {
    if index < app.notebook.tabs.len() && index != app.notebook.active {
        app.notebook.active = index;
        drop_find(&mut app.notebook);
        let path = app.notebook.tabs[index].path.clone();
        super::explorer::reveal(app, &path, false);
    }
    app.needs_draw = true;
}

/// `Ctrl+Tab` / `Ctrl+Shift+Tab`: the next or previous tab (around).
pub(super) fn step_tab(app: &mut App, forward: bool) {
    let len = app.notebook.tabs.len();
    if len < 2 {
        return;
    }
    let active = app.notebook.active;
    show_tab(app, if forward { (active + 1) % len } else { (active + len - 1) % len });
}

/// Closes a tab; one with unsaved changes is written first (it comes back
/// with the conflict banner when the file changed meanwhile).
pub(super) fn close_tab(app: &mut App, index: usize) {
    if index >= app.notebook.tabs.len() {
        return;
    }
    let needs_save = {
        let tab = &app.notebook.tabs[index];
        tab.unsaved && tab.editable()
    };
    if needs_save && app.notebook.tabs[index].conflict.is_some() {
        show_tab(app, index);
        return app.notify("This note changed on disk: choose Keep mine or Take theirs before closing it.", true);
    }
    if needs_save && app.notebook.tabs[index].moving {
        show_tab(app, index);
        return app.notify("This note is being moved: close it once that is done.", true);
    }
    if needs_save {
        save(app, index, true);
    }
    let state = &mut app.notebook;
    let tab = state.tabs.remove(index);
    if needs_save {
        state.closing.push(tab);
    }
    if state.active > index || state.active >= state.tabs.len() {
        state.active = state.active.saturating_sub(1);
    }
    drop_find(state);
    app.needs_draw = true;
}

/// The tab's view (one it offers).
pub(super) fn set_view(app: &mut App, view: View) {
    let Some(tab) = app.notebook.tab_mut() else { return };
    if !tab.views().contains(&view) {
        return;
    }
    tab.view = view;
    tab.editor.set_mode(if view == View::Live { Mode::Live } else { Mode::Source });
    // The HTML view shows the file: what was typed is written first.
    let write = view == View::Rendered && tab.kind == Kind::Html && tab.unsaved;
    app.notebook.completion = None;
    if let Some(find) = &mut app.notebook.find {
        find.line = None;
    }
    if write {
        save(app, app.notebook.active, true);
    }
    app.needs_draw = true;
}

/// `Ctrl+E`: Rendered ⇄ Source (Live goes to Rendered). Editing takes the
/// keyboard, from the explorer too: what is typed next goes into the note.
pub(super) fn toggle_rendered(app: &mut App) {
    let Some(tab) = app.notebook.tab() else { return };
    let next = if tab.view == View::Rendered { View::Source } else { View::Rendered };
    set_view(app, next);
    if next != View::Rendered && app.notebook.tab().is_some_and(|t| t.view == next) {
        app.set_focus(crate::app::Focus::Canvas);
    }
}

/// `[[Name]]` (maybe `Name#heading`, `Name|alias`, `Folder/Name`): the note it means.
pub fn link_name(link: &str) -> &str {
    let link = link.split('|').next().unwrap_or(link);
    link.split('#').next().unwrap_or(link).trim()
}

/// The note a `[[name]]` means among `known` notes: same file name (any
/// case, with or without extension) or title; `Folder/Name` matches the end
/// of the path; the nearest to `near` (a folder) first, then the shortest path.
pub fn resolve(known: &[Known], name: &str, near: Option<&str>) -> Option<String> {
    let name = link_name(name);
    if name.is_empty() {
        return None;
    }
    let wanted = name.replace('\\', "/").to_lowercase();
    let (dir_part, base) = match wanted.rsplit_once('/') {
        Some((dir, base)) => (Some(dir.to_owned()), base.to_owned()),
        None => (None, wanted.clone()),
    };
    let matches = |k: &Known| {
        let file = k.name.to_lowercase();
        let hit = file == base || nb::stem(&file) == base || (dir_part.is_none() && k.title.to_lowercase() == base);
        let in_dir = dir_part.as_ref().is_none_or(|dir| {
            let parent = parent(&k.path).map(|p| norm(&p).replace('\\', "/")).unwrap_or_default();
            parent.ends_with(&format!("/{dir}")) || parent == *dir
        });
        hit && in_dir
    };
    let near = near.map(norm);
    known
        .iter()
        .filter(|k| matches(k))
        .min_by_key(|k| {
            let here = near.as_ref().is_some_and(|n| parent(&k.path).is_some_and(|p| norm(&p) == *n));
            let file = k.name.to_lowercase();
            let exact_name = nb::stem(&file) == base || file == base;
            // Equal otherwise: the path decides (the known notes come in no fixed order).
            (!here, !exact_name, k.path.len(), norm(&k.path))
        })
        .map(|k| k.path.clone())
}

/// Every note this window knows of (open tabs, listings, the known list).
pub(super) fn known_notes(app: &App) -> Vec<Known> {
    let state = &app.notebook;
    let mut out: Vec<Known> = state.tabs.iter().map(|t| Known { path: t.path.clone(), name: t.name.clone(), title: t.title.clone(), ai: t.provenance.audience == nb::Audience::Ai }).collect();
    for folder in state.folders.values() {
        out.extend(folder.entries.iter().filter(|e| !e.is_dir()).map(|e| Known { path: e.path.clone(), name: e.name.clone(), title: e.title.clone(), ai: e.provenance.audience == nb::Audience::Ai }));
    }
    out.extend(state.known.iter().cloned());
    out
}

/// Opens a note by its `[[name]]`: a known one, else a name search; when
/// there is none, offers to create it next to the shown note.
pub(super) fn open_name(app: &mut App, name: &str) {
    let near = app.notebook.tab().and_then(|t| parent(&t.path));
    if let Some(path) = resolve(&known_notes(app), name, near.as_deref()) {
        return open_note(app, &path);
    }
    let wanted = link_name(name).to_owned();
    if wanted.is_empty() {
        return;
    }
    let base = file_name(&wanted.replace('/', "\\"));
    let query = base.clone();
    load::search(app, &query, 50, move |app, result| {
        let found: Vec<Known> = result.unwrap_or_default().into_iter().filter(|h| h.line == 0).map(|h| Known { name: file_name(&h.path), title: String::new(), path: h.path, ai: false }).collect();
        match resolve(&found, &wanted, near.as_deref()) {
            Some(path) => open_note(app, &path),
            None => {
                let folder = near.or_else(|| super::ops::inbox(app));
                match folder {
                    Some(folder) => super::dialogs::ask_name_prefilled(app, super::NamePurpose::NewNote { folder }, &base),
                    None => app.notify(format!("No note named “{wanted}”."), true),
                }
            }
        }
    });
}

// ---------- saving and changes on disk ----------

/// Writes a tab's text based on its revision (an agent's note became
/// `author: mixed` as you typed). `quiet`: no notice when there is nothing to save.
pub(super) fn save(app: &mut App, index: usize, quiet: bool) {
    let Some(tab) = app.notebook.tabs.get(index) else { return };
    if !tab.editable() || tab.saving || tab.moving || tab.conflict.is_some() {
        return;
    }
    if !tab.unsaved {
        if !quiet {
            app.notify("Saved.", false);
        }
        return;
    }
    start_write(app, tab.id);
}

/// `NoteWrite` of a tab (open, or closed while saving) in the background;
/// [`saved`] gets the answer. A file gone from disk is written again (it
/// must not exist then: a file put back meanwhile is a conflict).
fn start_write(app: &mut App, id: u64) {
    let roots = load::root_paths(app);
    let Some(tab) = app.notebook.tab_by_id(id) else { return };
    let text = Arc::new(tab.text());
    tab.saving = true;
    tab.sent = Some(text.clone());
    let base = if tab.missing { None } else { tab.rev.clone() };
    let (path, version) = (tab.path.clone(), tab.version);
    load::background(
        app,
        move |backend| backend.note_write(&path, &text, base.as_deref(), &roots).map_err(|e| format!("{e:#}")),
        move |app, result| saved(app, id, version, result),
    );
}

/// A save's answer for the tab `id` (open, or closed meanwhile; its path
/// may have changed since by a rename).
pub(super) fn saved(app: &mut App, id: u64, version: u64, result: Result<Saved, String>) {
    let state = &mut app.notebook;
    let closing = state.closing.iter().position(|t| t.id == id);
    let tab = match closing {
        Some(at) => &mut state.closing[at],
        None => match state.tabs.iter_mut().find(|t| t.id == id) {
            Some(tab) => tab,
            None => return,
        },
    };
    tab.saving = false;
    tab.sent = None;
    let path = tab.path.clone();
    match result {
        Ok(Saved::Written(rev)) => {
            tab.rev = Some(rev);
            tab.missing = false;
            tab.save_error = None;
            if tab.version == version {
                tab.unsaved = false;
                tab.editor.mark_saved();
            }
            tab.classify();
            if let Some(at) = closing {
                // Changed after this write started (closed while saving): write again.
                if tab.unsaved {
                    return start_write(app, id);
                }
                app.notebook.closing.remove(at);
            }
            if let Some(dir) = parent(&path) {
                load::relist(app, &dir);
            }
        }
        Ok(Saved::Conflict(theirs)) => {
            tab.conflict = Some(theirs);
            if let Some(at) = closing {
                reopen_closing(app, at);
                app.notify("The note changed on disk while you closed it: choose Keep mine or Take theirs.", true);
            }
        }
        Err(error) => {
            // Said once (and again after more typing); autosave tries again now and then.
            let first = tab.save_error.is_none();
            tab.save_error = Some((error.clone(), Instant::now()));
            // Polling soon tells a file that is gone: the next try writes it again.
            app.notebook.poll_at = None;
            if let Some(at) = closing {
                reopen_closing(app, at);
            }
            if first || closing.is_some() {
                app.notify(format!("Could not save {}: {error}", file_name(&path)), true);
            }
        }
    }
}

/// A tab closed while saving whose write failed or conflicted comes back (shown).
fn reopen_closing(app: &mut App, at: usize) {
    let tab = app.notebook.closing.remove(at);
    app.notebook.tabs.push(tab);
    app.notebook.active = app.notebook.tabs.len() - 1;
    drop_find(&mut app.notebook);
}

/// Every UI pass: saves tabs a second after their last change.
pub(super) fn autosave(app: &mut App) {
    let now = Instant::now();
    let due: Vec<usize> = app.notebook.tabs.iter().enumerate().filter(|(_, t)| t.save_due(now)).map(|(i, _)| i).collect();
    for index in due {
        save(app, index, true);
    }
}

/// A polled revision of an open note: a clean buffer reloads, one with
/// changes gets the conflict banner, a missing file is said.
pub(super) fn on_rev(app: &mut App, path: &str, rev: Option<String>) {
    let Some(index) = app.notebook.tab_index(path) else { return };
    let tab = &mut app.notebook.tabs[index];
    if tab.saving || tab.reading || tab.moving || tab.rev.is_none() {
        return;
    }
    let Some(rev) = rev else {
        tab.missing = true;
        return;
    };
    tab.missing = false;
    if tab.rev.as_deref() == Some(rev.as_str()) || tab.conflict.as_ref().is_some_and(|c| c.rev == rev) {
        return;
    }
    tab.reading = true;
    let (id, base) = (tab.id, tab.rev.clone());
    let asked = path.to_owned();
    let read = asked.clone();
    load::background(
        app,
        move |backend| backend.note_read(&read).map_err(|e| format!("{e:#}")),
        move |app, result| {
            let Some(tab) = app.notebook.tabs.iter_mut().find(|t| t.id == id) else { return };
            tab.reading = false;
            // Saved, reloaded or moved since the read started: that version
            // may be older than the buffer's; the next poll looks again.
            if tab.rev != base || !same_path(&tab.path, &asked) {
                return;
            }
            if let Ok(theirs) = result {
                changed_on_disk(tab, theirs);
            }
        },
    );
}

/// Another version of the file arrived: reload a clean buffer, else conflict.
pub fn changed_on_disk(tab: &mut Tab, theirs: NoteText) {
    if tab.saving || tab.rev.as_deref() == Some(theirs.rev.as_str()) {
        return;
    }
    if tab.unsaved {
        tab.conflict = Some(theirs);
    } else {
        tab.loaded_text(&theirs.text, theirs.rev, true);
    }
}

/// *Keep mine* for the tab of `path`: write the buffer over their version.
pub(super) fn keep_mine(app: &mut App, path: &str) {
    let Some(index) = app.notebook.tab_index(path) else { return };
    let tab = &mut app.notebook.tabs[index];
    let Some(theirs) = tab.conflict.take() else { return };
    tab.rev = Some(theirs.rev);
    tab.unsaved = true;
    tab.version += 1;
    save(app, index, true);
}

/// *Take theirs* for the tab of `path`: the file's version replaces the buffer.
pub(super) fn take_theirs(app: &mut App, path: &str) {
    let Some(index) = app.notebook.tab_index(path) else { return };
    let tab = &mut app.notebook.tabs[index];
    let Some(theirs) = tab.conflict.take() else { return };
    tab.loaded_text(&theirs.text, theirs.rev, true);
    app.needs_draw = true;
}

// ---------- find ----------

/// The find query for the shown tab's editor (it lowercases and keeps the matches).
fn editor_find(app: &mut App) {
    let query = app.notebook.find.as_ref().and_then(|f| f.query().map(str::to_owned));
    if let Some(tab) = app.notebook.tab_mut() {
        tab.editor.set_find(query.as_deref());
    }
}

/// `Ctrl+F`: the find bar (with the selection, else the last query). Over
/// the GUI's HTML pane it finds in the source (the page is the web view's).
pub(super) fn open_find(app: &mut App) {
    if app.notebook.pane && app.notebook.tab().is_some_and(|t| t.view == View::Rendered && t.editable()) {
        set_view(app, View::Source);
    }
    let selected = app.notebook.tab().and_then(|t| t.editor.selected_text()).filter(|s| !s.contains('\n'));
    let find = app.notebook.find.get_or_insert_with(Find::default);
    if let Some(text) = selected {
        find.input.set(&text);
    }
    editor_find(app);
    app.needs_draw = true;
}

pub(super) fn close_find(app: &mut App) {
    app.notebook.find = None;
    if let Some(tab) = app.notebook.tab_mut() {
        tab.editor.set_find(None);
    }
    app.needs_draw = true;
}

/// Lines of the rendered view holding `query` (any case), kept per version and width.
pub fn rendered_hits(tab: &mut Tab, query: &str) -> Arc<Vec<usize>> {
    let query = lower(query);
    let Some((version, width, lines)) = tab.lines.clone() else { return Arc::default() };
    if let Some((v, w, q, hits)) = &tab.found
        && (*v, *w) == (version, width)
        && *q == query
    {
        return hits.clone();
    }
    let hits: Arc<Vec<usize>> = Arc::new(lines.iter().enumerate().filter(|(_, l)| lower(&l.text()).contains(&query)).map(|(i, _)| i).collect());
    tab.found = Some((version, width, query, hits.clone()));
    hits
}

/// The next (or previous) match: the editor's, or a line of the rendered view.
pub(super) fn find_step(app: &mut App, forward: bool) {
    let Some(find) = app.notebook.find.clone() else { return open_find(app) };
    let Some(query) = find.query() else { return };
    let Some(tab) = app.notebook.tabs.get_mut(app.notebook.active) else { return };
    if tab.view == View::Rendered {
        let hits = rendered_hits(tab, query);
        let at = find.line;
        let next = if forward { hits.iter().find(|&&i| at.is_none_or(|a| i > a)).or(hits.first()) } else { hits.iter().rev().find(|&&i| at.is_none_or(|a| i < a)).or(hits.last()) };
        if let Some(&line) = next {
            tab.scroll = line.saturating_sub(tab.rows / 3);
            if let Some(find) = &mut app.notebook.find {
                find.line = Some(line);
            }
        }
    } else {
        tab.editor.set_find(Some(query));
        tab.editor.find_next(forward);
    }
    app.needs_draw = true;
}

/// Matches of the find bar's query: (current, count).
pub fn find_count(tab: &mut Tab, find: &Find) -> (usize, usize) {
    let Some(query) = find.query() else { return (0, 0) };
    if tab.view == View::Rendered {
        let hits = rendered_hits(tab, query);
        let current = find.line.and_then(|line| hits.iter().position(|h| *h == line)).map(|i| i + 1).unwrap_or(0);
        (current, hits.len())
    } else {
        tab.editor.find_status().unwrap_or((0, 0))
    }
}

/// A key while the find bar is open (true: taken).
fn find_key(app: &mut App, key: &KeyEvent) -> bool {
    let shift = key.modifiers.contains(Modifiers::SHIFT);
    match key.code {
        KeyCode::Esc => close_find(app),
        KeyCode::Enter | KeyCode::F(3) => find_step(app, !shift),
        KeyCode::Char('f' | 'F') if key.modifiers.contains(Modifiers::CONTROL) => {}
        _ => {
            let Some(find) = &mut app.notebook.find else { return false };
            let before = find.input.text.clone();
            if !find.input.handle(key) {
                return false;
            }
            if find.input.text != before {
                find.line = None;
                editor_find(app);
                if let Some(tab) = app.notebook.tab_mut()
                    && tab.view != View::Rendered
                {
                    // Search from where the caret is.
                    let at = tab.editor.selection().map(|(s, _)| s).unwrap_or(tab.editor.cursor());
                    tab.editor.set_cursor(at);
                }
                find_step(app, true);
            }
        }
    }
    true
}

// ---------- [[ completion ----------

/// Note names (file stems) matching a typed prefix, best first (at most 8).
pub fn completions(known: &[Known], prefix: &str) -> Vec<String> {
    let mut scored: Vec<(i32, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for k in known {
        let name = nb::stem(&k.name).to_owned();
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        let score = if prefix.is_empty() { Some(0) } else { crate::spaces::fuzzy(prefix, &name).map(|(s, _)| s) };
        if let Some(score) = score {
            scored.push((score, name));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(8).map(|(_, n)| n).collect()
}

/// After a change or a move in the editor: the completion list for the
/// `[[` being typed (and one name search for more per `[[`, from its first
/// two letters on).
pub(super) fn update_completion(app: &mut App) {
    let prefix = app.notebook.tab().filter(|t| t.view != View::Rendered && t.editable()).and_then(|t| t.editor.completion_prefix());
    let Some(prefix) = prefix.filter(|p| p.chars().count() <= 60 && !p.contains('[')) else {
        app.notebook.completion = None;
        return;
    };
    let items = completions(&known_notes(app), &prefix);
    let lower = prefix.to_lowercase();
    let searched = app.notebook.completion.as_ref().map(|c| c.searched.clone()).filter(|s| !s.is_empty() && lower.contains(s.as_str())).unwrap_or_default();
    let sel = app.notebook.completion.as_ref().filter(|c| c.prefix == prefix).map(|c| c.sel).unwrap_or(0);
    let search = searched.is_empty() && prefix.chars().count() >= 2 && app.notebook.info.is_some();
    app.notebook.completion = Some(Completion { sel: sel.min(items.len().saturating_sub(1)), items, prefix: prefix.clone(), searched: if search { lower.clone() } else { searched } });
    if search {
        load::search(app, &lower, 40, move |app, result| {
            let hits = result.unwrap_or_default();
            for hit in hits.into_iter().filter(|h| h.line == 0) {
                if !app.notebook.known.iter().any(|k| same_path(&k.path, &hit.path)) && app.notebook.known.len() < super::KNOWN_MAX {
                    app.notebook.known.push(Known { name: file_name(&hit.path), title: String::new(), path: hit.path, ai: false });
                }
            }
            refresh_completion(app);
        });
    }
}

/// More notes became known (a listing, a search): the open completion list again.
pub(super) fn refresh_completion(app: &mut App) {
    let Some(prefix) = app.notebook.completion.as_ref().map(|c| c.prefix.clone()) else { return };
    let items = completions(&known_notes(app), &prefix);
    if let Some(c) = &mut app.notebook.completion {
        c.sel = c.sel.min(items.len().saturating_sub(1));
        c.items = items;
    }
}

/// Completes the `[[` with an item (`None`: the selected one).
pub(super) fn complete(app: &mut App, index: Option<usize>) {
    let Some(completion) = app.notebook.completion.take() else { return };
    let Some(name) = completion.items.get(index.unwrap_or(completion.sel)).cloned() else { return };
    let Some(tab) = app.notebook.tab_mut() else { return };
    tab.editor.replace_before_cursor(completion.prefix.chars().count(), &format!("{name}]]"));
    tab.edited();
    app.needs_draw = true;
}

/// A key while the completion list shows (true: taken).
fn completion_key(app: &mut App, key: &KeyEvent) -> bool {
    let Some(completion) = &mut app.notebook.completion else { return false };
    if completion.items.is_empty() || !key.modifiers.difference(Modifiers::SHIFT).is_empty() {
        return false;
    }
    let len = completion.items.len();
    match key.code {
        KeyCode::Up => completion.sel = (completion.sel + len - 1) % len,
        KeyCode::Down => completion.sel = (completion.sel + 1) % len,
        KeyCode::Tab | KeyCode::Enter => complete(app, None),
        KeyCode::Esc => app.notebook.completion = None,
        _ => return false,
    }
    app.needs_draw = true;
    true
}

// ---------- links ----------

/// The host rendered links of the cells view point at (`richtext` keeps
/// only http(s) links): `#note:`/`#todo:` and relative links travel as
/// `https://app.canvas.invalid/…` and come back here.
pub const LINK_HOST: &str = "https://app.canvas.invalid/";

/// A link under the caret in Source or Live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkAt {
    /// `[[Name]]` (as written inside the brackets).
    Note(String),
    /// A todo id (`DEC-031`).
    Todo(String),
    Url(String),
}

/// The link at character `col` of `line`: a `[[…]]`, an http(s) address or a todo id.
pub fn link_at(line: &str, col: usize) -> Option<LinkAt> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '['
            && chars[i + 1] == '['
            && let Some(end) = (i + 2..chars.len().saturating_sub(1)).find(|&j| chars[j] == ']' && chars[j + 1] == ']')
        {
            if col >= i && col <= end + 1 {
                let inner: String = chars[i + 2..end].iter().collect();
                return (!inner.trim().is_empty()).then(|| LinkAt::Note(inner.trim().to_owned()));
            }
            i = end + 2;
            continue;
        }
        i += 1;
    }
    let col = col.min(chars.len());
    let span = |keep: &dyn Fn(char) -> bool| {
        let mut start = col;
        while start > 0 && keep(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && keep(chars[end]) {
            end += 1;
        }
        chars[start..end].iter().collect::<String>()
    };
    let token = span(&|c: char| !c.is_whitespace());
    let token = token.trim_start_matches(['(', '<', '[', '"', '\'']);
    let lower = token.to_ascii_lowercase();
    if let Some(at) = lower.find("http://").or_else(|| lower.find("https://")) {
        let url = token[at..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '>', ']', '"', '\'']);
        return Some(LinkAt::Url(url.to_owned()));
    }
    let word = span(&|c: char| c.is_alphanumeric() || c == '-' || c == '_');
    nb::is_todo_id(&word).then_some(LinkAt::Todo(word))
}

/// `href` relative to the folder `dir` (`..` and `.` resolved; fragment dropped).
pub fn relative_path(dir: &str, href: &str) -> String {
    let href = href.split(['#', '?']).next().unwrap_or_default();
    let decoded = percent_encoding::percent_decode_str(href).decode_utf8_lossy().into_owned();
    let mut path = dir.trim_end_matches(['\\', '/']).to_owned();
    for part in decoded.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            path = parent(&path).unwrap_or(path);
        } else {
            path = super::join(&path, part);
        }
    }
    path
}

/// Opens what a link means: a note (`[[name]]`, a relative path), a todo
/// (in Tasks), else a web address (after asking).
pub(super) fn follow_link(app: &mut App, href: &str) {
    let href = href.trim();
    let inner = href.strip_prefix(LINK_HOST);
    let target = match inner {
        Some(rest) if rest.starts_with("note:") || rest.starts_with("todo:") => nb::link_target(&format!("#{rest}")),
        _ => nb::link_target(href),
    };
    match target {
        Some(nb::LinkTarget::Note(name)) => return app.navigate(Space::Notebook, NavTarget::Note(link_name(&name).to_owned())),
        Some(nb::LinkTarget::Todo(id)) => return app.navigate(Space::Tasks, NavTarget::Todo(id)),
        None => {}
    }
    if let Some(rel) = inner.and_then(|rest| rest.strip_prefix("rel/")) {
        let Some(dir) = app.notebook.tab().and_then(|t| parent(&t.path)) else { return };
        let path = relative_path(&dir, rel);
        return app.navigate(Space::Notebook, NavTarget::Note(path));
    }
    if href.starts_with('#') {
        return;
    }
    app.run(crate::dialogs::Action::Report(crate::reports::ReportAction::NoteLink(href.to_owned())));
}

/// Opens the link under the caret (Ctrl+click in Source or Live).
fn follow_caret_link(app: &mut App) -> bool {
    let Some(tab) = app.notebook.tab() else { return false };
    let Pos { line, col } = tab.editor.cursor();
    match link_at(tab.editor.line(line), col) {
        Some(LinkAt::Note(name)) => app.navigate(Space::Notebook, NavTarget::Note(link_name(&name).to_owned())),
        Some(LinkAt::Todo(id)) => app.navigate(Space::Tasks, NavTarget::Todo(id)),
        Some(LinkAt::Url(url)) => app.run(crate::dialogs::Action::Report(crate::reports::ReportAction::NoteLink(url))),
        None => return false,
    }
    true
}

/// An outline heading (line of the note): the editor goes there; the
/// rendered cells scroll to it (the HTML pane: Source shows it).
pub(super) fn jump_to_line(app: &mut App, line: usize) {
    let pane = app.notebook.pane;
    let Some(tab) = app.notebook.tab_mut() else { return };
    if tab.view == View::Rendered && !pane {
        let heading = tab.editor.line(line).trim_start_matches('#').trim().to_lowercase();
        let found = tab.lines.as_ref().and_then(|(_, _, lines)| lines.iter().position(|l| l.text().trim().to_lowercase() == heading));
        if let Some(at) = found {
            tab.scroll = at;
            app.needs_draw = true;
            return;
        }
    }
    if tab.view == View::Rendered {
        set_view(app, View::Source);
    }
    if let Some(tab) = app.notebook.tab_mut() {
        tab.editor.goto_line(line);
    }
    app.needs_draw = true;
}

// ---------- keys, text and mouse of the reader ----------

/// Puts copied or cut text on this machine's clipboard.
fn clipboard(app: &mut App, text: &str) {
    if let Err(error) = app.platform.set_clipboard_text(text) {
        app.notify(format!("Copy failed: {error:#}"), true);
    }
}

/// A key for the reader (the main area has the keyboard).
pub(super) fn key(app: &mut App, key: KeyEvent) -> bool {
    if app.notebook.find.is_some() && find_key(app, &key) {
        return true;
    }
    if completion_key(app, &key) {
        return true;
    }
    let ctrl = key.modifiers.contains(Modifiers::CONTROL) && !key.modifiers.contains(Modifiers::ALT);
    let index = app.notebook.active;
    let Some(tab) = app.notebook.tabs.get_mut(index) else { return false };
    if tab.view == View::Rendered || !tab.loaded {
        let page = tab.rows.max(2) - 1;
        let end = tab.total.saturating_sub(tab.rows);
        let scroll = match key.code {
            KeyCode::Up => Some(tab.scroll.saturating_sub(1)),
            KeyCode::Down => Some(tab.scroll + 1),
            KeyCode::PageUp => Some(tab.scroll.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char(' ') if !ctrl => Some(tab.scroll + page),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(end),
            _ => None,
        };
        match (scroll, key.code) {
            (Some(scroll), _) => tab.scroll = scroll.min(end),
            (None, KeyCode::Char('f' | 'F')) if ctrl => open_find(app),
            (None, KeyCode::F(3)) => find_step(app, !key.modifiers.contains(Modifiers::SHIFT)),
            _ => return false,
        }
        app.needs_draw = true;
        return true;
    }
    if key.code == KeyCode::F(3) {
        find_step(app, !key.modifiers.contains(Modifiers::SHIFT));
        return true;
    }
    match tab.editor.key(&key) {
        Outcome::Unhandled => return false,
        Outcome::Handled => {}
        Outcome::Changed => tab.edited(),
        Outcome::Copy(text) => clipboard(app, &text),
        Outcome::Cut(text) => {
            tab.edited();
            clipboard(app, &text);
        }
        Outcome::Save => save(app, index, false),
        Outcome::Find => open_find(app),
    }
    update_completion(app);
    app.needs_draw = true;
    true
}

/// Pasted or IME text for the reader: the find bar, else the editor.
pub(super) fn text(app: &mut App, text: &str) -> bool {
    if let Some(find) = &mut app.notebook.find {
        find.input.insert(text);
        find.line = None;
        editor_find(app);
        find_step(app, true);
        return true;
    }
    let Some(tab) = app.notebook.tab_mut() else { return false };
    if tab.view == View::Rendered || !tab.editable() {
        return false;
    }
    if tab.editor.insert_text(text) == Outcome::Changed {
        tab.edited();
    }
    update_completion(app);
    true
}

/// The mouse in the reader (its content) or the info panel (scrolling).
pub(super) fn mouse(app: &mut App, mouse: MouseEvent) {
    let (x, y) = (mouse.column, mouse.row);
    let content = app.notebook.content;
    let in_content = crate::paint::rect_contains(content, x, y);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta: i32 = if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
            if crate::paint::rect_contains(app.layout.terms, x, y) {
                app.notebook.info_scroll = (app.notebook.info_scroll as i64 + delta as i64).max(0) as usize;
            } else if let Some(tab) = app.notebook.tab_mut() {
                if tab.view == View::Rendered || !tab.loaded {
                    let end = tab.total.saturating_sub(tab.rows);
                    tab.scroll = ((tab.scroll as i64 + delta as i64).max(0) as usize).min(end);
                } else {
                    tab.editor.scroll(delta);
                }
            }
            app.needs_draw = true;
        }
        MouseEventKind::Down(MouseButton::Left) if in_content => {
            app.notebook.dragging = true;
            let now = Instant::now();
            let double = app.notebook.last_click.is_some_and(|(at, cx, cy)| now.duration_since(at) < Duration::from_millis(450) && cx == x && cy == y);
            app.notebook.last_click = Some((now, x, y));
            let ctrl = mouse.modifiers.contains(Modifiers::CONTROL);
            let shift = mouse.modifiers.contains(Modifiers::SHIFT);
            let Some(tab) = app.notebook.tab_mut() else { return };
            if tab.view == View::Rendered || !tab.loaded {
                return;
            }
            if double {
                tab.editor.double_click(content, x, y);
            } else {
                tab.editor.click(content, x, y, shift);
                if ctrl && follow_caret_link(app) {
                    return;
                }
            }
            update_completion(app);
            app.needs_draw = true;
        }
        MouseEventKind::Down(_) => app.notebook.dragging = false,
        MouseEventKind::Drag(MouseButton::Left) if app.notebook.dragging => {
            if let Some(tab) = app.notebook.tab_mut().filter(|t| t.view != View::Rendered && t.loaded) {
                tab.editor.drag(content, x, y);
                app.needs_draw = true;
            }
        }
        MouseEventKind::Up(_) => app.notebook.dragging = false,
        _ => {}
    }
}
