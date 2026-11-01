//! The explorer's roots and rows: pure functions of the notebook's state,
//! the document and the reports inbox.

use std::collections::{HashMap, HashSet};

use canvas_model::notebook::{Audience, Entry, Info, Place};

use super::{Folder, REPORTS, State, file_name, is_inside, norm};
use crate::app::App;
use crate::doc::Doc;

/// Whose notes the explorer shows (the Human | All | AI segment, `Alt+H`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Notes for you only.
    Human,
    #[default]
    All,
    /// Notes for agents only.
    Ai,
}

impl Filter {
    pub const ALL: [Filter; 3] = [Filter::Human, Filter::All, Filter::Ai];

    pub fn label(self) -> &'static str {
        match self {
            Filter::Human => "Human",
            Filter::All => "All",
            Filter::Ai => "AI",
        }
    }

    /// `Alt+H`: Human → All → AI → Human.
    pub fn next(self) -> Self {
        match self {
            Filter::Human => Filter::All,
            Filter::All => Filter::Ai,
            Filter::Ai => Filter::Human,
        }
    }

    pub fn shows(self, audience: Audience) -> bool {
        match self {
            Filter::All => true,
            Filter::Human => audience == Audience::Human,
            Filter::Ai => audience == Audience::Ai,
        }
    }

    /// "N AI hidden" / "N for you hidden".
    pub fn hidden_label(self, count: usize) -> String {
        match self {
            Filter::Ai => format!("{count} for you hidden"),
            _ => format!("{count} AI hidden"),
        }
    }
}

/// What an explorer root is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// The canvas's notebook root.
    Notebook,
    /// A group's folder outside the root (`◫ <group>`).
    Group,
    /// The canvas-wide folder (written by and for agents).
    CanvasWide,
    /// An extra folder (`Doc::notebook_mounts`).
    Mount,
}

/// One explorer root.
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    pub kind: RootKind,
    pub label: String,
    pub path: String,
    pub place: Place,
    /// The group (id) whose folder it is.
    pub group: Option<String>,
}

/// The explorer's roots: the notebook root, each group folder outside it,
/// the canvas-wide folder and the mounts (once each).
pub fn roots(info: Option<&Info>, doc: &Doc) -> Vec<Root> {
    let mut out: Vec<Root> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |out: &mut Vec<Root>, root: Root| {
        if !root.path.trim().is_empty() && seen.insert(norm(&root.path)) {
            out.push(root);
        }
    };
    let notebook = info.map(|i| i.root.clone()).filter(|r| !r.trim().is_empty());
    if let Some(root) = &notebook {
        push(&mut out, Root { kind: RootKind::Notebook, label: "Notebook".into(), path: root.clone(), place: Place::Notes, group: None });
    }
    for group in &doc.groups {
        let Some(folder) = group.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) else { continue };
        if notebook.as_deref().is_some_and(|root| is_inside(folder, root)) {
            continue;
        }
        push(&mut out, Root { kind: RootKind::Group, label: format!("◫ {}", group.name), path: folder.to_owned(), place: Place::Notes, group: Some(group.id.clone()) });
    }
    if let Some(dir) = info.and_then(|i| i.canvas_dir.clone()) {
        push(&mut out, Root { kind: RootKind::CanvasWide, label: "Canvas (canvas-wide)".into(), path: dir, place: Place::CanvasWide, group: None });
    }
    for mount in &doc.notebook_mounts {
        let mount = mount.trim();
        let name = file_name(mount);
        let label = if name.is_empty() { mount.to_owned() } else { name };
        push(&mut out, Root { kind: RootKind::Mount, label, path: mount.to_owned(), place: Place::Notes, group: None });
    }
    out
}

/// The root a path is in (the innermost one).
pub fn root_of<'a>(roots: &'a [Root], path: &str) -> Option<&'a Root> {
    roots.iter().filter(|r| is_inside(path, &r.path)).max_by_key(|r| norm(&r.path).len())
}

/// `path` relative to `root`, `/` separators (`""`: the root itself).
pub fn rel(root: &str, path: &str) -> String {
    if norm(path) == norm(root) || !is_inside(path, root) {
        return String::new();
    }
    // `path` starts with `root` (any case, either separator): the same length.
    let offset = root.trim().trim_end_matches(['\\', '/']).len();
    let path = path.trim();
    path.get(offset..).unwrap_or_default().trim_start_matches(['\\', '/']).replace('\\', "/")
}

/// Where a path is, for people: "Notebook › Inbox".
pub fn place_label(app: &App, path: &str) -> String {
    let roots = roots(app.notebook.info.as_ref(), &app.doc);
    match root_of(&roots, path) {
        Some(root) => {
            let rel = rel(&root.path, path);
            let mut parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
            parts.pop();
            std::iter::once(root.label.as_str()).chain(parts).collect::<Vec<_>>().join(" › ")
        }
        None => super::parent(path).unwrap_or_default(),
    }
}

/// One explorer row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Its key: a normalized path, [`REPORTS`], `report:<id>`, `msg:<n>`.
    pub key: String,
    pub depth: u16,
    pub kind: RowKind,
    pub expanded: bool,
    /// The group (id) whose folder this is.
    pub group: Option<String>,
}

/// What a row shows.
#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Root(Root),
    /// A folder (the entry, the root it is under).
    Dir(Entry, String),
    /// A file (the entry, the root it is under).
    File(Entry, String),
    /// The Reports section (how many reports).
    Reports(usize),
    /// A report (id, title, unread).
    Report(String, String, bool),
    /// Loading…, (empty), an error.
    Message(String),
}

impl Row {
    /// The path of a root, folder or file row.
    pub fn path(&self) -> Option<&str> {
        match &self.kind {
            RowKind::Root(root) => Some(&root.path),
            RowKind::Dir(entry, _) | RowKind::File(entry, _) => Some(&entry.path),
            _ => None,
        }
    }

    /// A folder (or root) that opens and closes.
    pub fn is_folder(&self) -> bool {
        matches!(self.kind, RowKind::Root(_) | RowKind::Dir(..) | RowKind::Reports(_))
    }
}

/// The explorer's rows and how many notes the audience filter hides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tree {
    pub rows: Vec<Row>,
    pub hidden: usize,
}

/// Notes of a loaded subtree: shown, hidden by the audience filter, and
/// whether some of it is not listed yet.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    shown: usize,
    hidden: usize,
    unknown: bool,
}

/// What [`build`] works from.
pub struct Inputs<'a> {
    pub folders: &'a HashMap<String, Folder>,
    pub expanded: &'a HashSet<String>,
    pub filter: Filter,
    /// *Show internal files*.
    pub internal: bool,
    /// The `/` name filter (any case).
    pub query: String,
    /// Groups with a folder: (id, normalized folder).
    pub groups: Vec<(String, String)>,
}

impl<'a> Inputs<'a> {
    pub fn of(state: &'a State, doc: &Doc) -> Self {
        let groups = doc.groups.iter().filter_map(|g| g.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(|f| (g.id.clone(), norm(f)))).collect();
        Inputs { folders: &state.folders, expanded: &state.expanded, filter: state.filter, internal: state.show_internal, query: state.name_filter.text.trim().to_lowercase(), groups }
    }
}

struct Builder<'a> {
    input: &'a Inputs<'a>,
    tree: Tree,
    messages: usize,
    memo: HashMap<String, Counts>,
}

impl Builder<'_> {
    fn group_of(&self, key: &str) -> Option<String> {
        self.input.groups.iter().find(|(_, folder)| folder == key).map(|(id, _)| id.clone())
    }

    fn name_hit(&self, name: &str) -> bool {
        self.input.query.is_empty() || name.to_lowercase().contains(&self.input.query)
    }

    fn message(&mut self, text: &str, depth: u16) {
        self.messages += 1;
        self.tree.rows.push(Row { key: format!("msg:{}", self.messages), depth, kind: RowKind::Message(text.to_owned()), expanded: false, group: None });
    }

    /// Notes under a listed folder (by key), through the listed subfolders.
    fn counts(&mut self, key: &str, depth: usize) -> Counts {
        if let Some(counts) = self.memo.get(key) {
            return *counts;
        }
        let input = self.input;
        let Some(folder) = input.folders.get(key).filter(|f| f.at.is_some()) else { return Counts { unknown: true, ..Counts::default() } };
        let mut counts = Counts::default();
        for entry in &folder.entries {
            if entry.provenance.internal && !input.internal {
                continue;
            }
            if entry.is_dir() {
                let inner = if depth < 24 { self.counts(&norm(&entry.path), depth + 1) } else { Counts { unknown: true, ..Counts::default() } };
                counts.shown += inner.shown;
                counts.hidden += inner.hidden;
                counts.unknown |= inner.unknown;
            } else if self.name_hit(&entry.name) {
                if input.filter.shows(entry.provenance.audience) {
                    counts.shown += 1;
                } else {
                    counts.hidden += 1;
                }
            }
        }
        self.memo.insert(key.to_owned(), counts);
        counts
    }

    fn children(&mut self, dir: &str, root: &str, depth: u16) {
        let input = self.input;
        let key = norm(dir);
        let Some(folder) = input.folders.get(&key) else { return self.message("Loading…", depth) };
        if folder.at.is_none() {
            let text = folder.error.clone().unwrap_or_else(|| "Loading…".into());
            return self.message(&text, depth);
        }
        if folder.entries.is_empty() {
            return self.message("(empty)", depth);
        }
        let filtering = !input.query.is_empty();
        for entry in &folder.entries {
            if entry.provenance.internal && !input.internal {
                continue;
            }
            let entry_key = norm(&entry.path);
            if entry.is_dir() {
                let counts = self.counts(&entry_key, 0);
                let name_hit = filtering && self.name_hit(&entry.name);
                if filtering && !name_hit && counts.shown == 0 {
                    self.tree.hidden += counts.hidden;
                    continue;
                }
                // A folder that holds only notes the filter hides disappears.
                if input.filter != Filter::All && !name_hit && !counts.unknown && counts.shown == 0 && counts.hidden > 0 {
                    self.tree.hidden += counts.hidden;
                    continue;
                }
                let open = input.expanded.contains(&entry_key) || (filtering && counts.shown > 0);
                let group = self.group_of(&entry_key);
                self.tree.rows.push(Row { key: entry_key, depth, kind: RowKind::Dir(entry.clone(), root.to_owned()), expanded: open, group });
                if open {
                    self.children(&entry.path, root, depth + 1);
                } else {
                    // Closed: what it hides counts all the same.
                    self.tree.hidden += counts.hidden;
                }
            } else {
                if !self.name_hit(&entry.name) {
                    continue;
                }
                if !input.filter.shows(entry.provenance.audience) {
                    self.tree.hidden += 1;
                    continue;
                }
                self.tree.rows.push(Row { key: entry_key, depth, kind: RowKind::File(entry.clone(), root.to_owned()), expanded: false, group: None });
            }
        }
    }
}

/// The explorer's rows: each root (expanded ones with their listed
/// folders), then the Reports section (`reports`: id, title, unread).
pub fn build(input: &Inputs, roots: &[Root], reports: &[(String, String, bool)]) -> Tree {
    let mut b = Builder { input, tree: Tree::default(), messages: 0, memo: HashMap::new() };
    let filtering = !input.query.is_empty();
    for root in roots {
        let key = norm(&root.path);
        let counts = b.counts(&key, 0);
        let open = input.expanded.contains(&key) || (filtering && counts.shown > 0);
        let group = root.group.clone().or_else(|| b.group_of(&key));
        b.tree.rows.push(Row { key, depth: 0, kind: RowKind::Root(root.clone()), expanded: open, group });
        if open {
            b.children(&root.path, &root.path, 1);
        } else {
            b.tree.hidden += counts.hidden;
        }
    }
    let open = input.expanded.contains(REPORTS);
    b.tree.rows.push(Row { key: REPORTS.into(), depth: 0, kind: RowKind::Reports(reports.len()), expanded: open, group: None });
    let matching: Vec<&(String, String, bool)> = reports.iter().filter(|(_, title, _)| b.name_hit(title)).collect();
    if !input.filter.shows(Audience::Human) {
        b.tree.hidden += matching.len();
    } else if open && reports.is_empty() {
        b.message("(no reports)", 1);
    } else if open {
        for (id, title, unread) in matching {
            b.tree.rows.push(Row { key: format!("report:{id}"), depth: 1, kind: RowKind::Report(id.clone(), title.clone(), *unread), expanded: false, group: None });
        }
    }
    b.tree
}

/// The explorer of `app` as it is now.
pub fn tree(app: &App) -> Tree {
    let roots = roots(app.notebook.info.as_ref(), &app.doc);
    let reports: Vec<(String, String, bool)> = app.reports.threads().into_iter().map(|t| (t.latest.id.clone(), t.latest.title.clone(), t.unread > 0)).collect();
    build(&Inputs::of(&app.notebook, &app.doc), &roots, &reports)
}
