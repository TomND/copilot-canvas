//! Tests of the Notebook space's logic (pure functions and the tab state
//! machine with a fake backend).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use canvas_model::notebook::{self as nb, Entry, Info, Kind, NoteText, Place};
use crossbeam_channel::Receiver;

use super::tabs::{self, LinkAt, View};
use super::tree::{self, Filter, Inputs, RowKind};
use super::*;
use crate::app::AppEvent;
use crate::doc::{Doc, Group};
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::protocol::ClientMsg;
use crate::rpc::{Backend, Request, Response};

pub(super) const ROOT: &str = r"C:\nb";
const CANVAS: &str = r"C:\home\canvas-agents\c1";

/// The service's machine: files and folders in memory.
#[derive(Default)]
pub(super) struct Disk {
    pub(super) files: Mutex<BTreeMap<String, String>>,
    dirs: Mutex<BTreeSet<String>>,
    pub(super) writes: Mutex<Vec<String>>,
}

impl Disk {
    pub(super) fn with(files: &[(&str, &str)]) -> Arc<Self> {
        let disk = Disk::default();
        for (path, text) in files {
            disk.put(path, text);
        }
        Arc::new(disk)
    }

    pub(super) fn put(&self, path: &str, text: &str) {
        let mut dir = parent(path);
        while let Some(d) = dir {
            if !is_inside(&d, ROOT) && !is_inside(&d, CANVAS) && !is_inside(&d, r"C:\work") {
                break;
            }
            self.dirs.lock().unwrap().insert(d.clone());
            dir = parent(&d);
        }
        self.files.lock().unwrap().insert(path.to_owned(), text.to_owned());
    }

    pub(super) fn text(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn entries(&self, dir: &str, root: &str) -> Vec<Entry> {
        let place = if is_inside(dir, CANVAS) { Place::CanvasWide } else { Place::Notes };
        let mut out = Vec::new();
        for d in self.dirs.lock().unwrap().iter().filter(|d| parent(d).is_some_and(|p| same_path(&p, dir))) {
            let rel = format!("{}/", tree::rel(root, d));
            out.push(Entry { name: file_name(d), path: d.clone(), rel: rel.clone(), kind: Kind::Folder, provenance: nb::classify(&rel, &nb::Meta::default(), place), title: file_name(d), ..Entry::default() });
        }
        for (path, text) in self.files.lock().unwrap().iter().filter(|(p, _)| parent(p).is_some_and(|d| same_path(&d, dir))) {
            let name = file_name(path);
            let kind = Kind::of(&name);
            let rel = tree::rel(root, path);
            let meta = nb::note_meta(kind, text);
            out.push(Entry { name: name.clone(), path: path.clone(), rel: rel.clone(), kind, size: text.len() as u64, modified_ms: 0, provenance: nb::classify(&rel, &meta, place), title: nb::title(kind, text, &meta, &name) });
        }
        out
    }
}

impl Backend for Disk {
    fn call(&self, request: Request) -> anyhow::Result<Response> {
        let rev = |text: &str| nb::rev_of(text.as_bytes());
        Ok(match request {
            Request::NotebookInfo { root, .. } => Response::NotebookInfo(Info { root: root.unwrap_or_else(|| ROOT.into()), canvas_dir: Some(CANVAS.into()), home: r"C:\home".into() }),
            Request::NotebookList { dir, root } => Response::NoteEntries(self.entries(&dir, &root.unwrap_or_else(|| dir.clone()))),
            Request::NoteRead { path } => match self.text(&path) {
                Some(text) => Response::Note(NoteText { rev: rev(&text), text }),
                None => Response::Error("not found".into()),
            },
            Request::NoteWrite { path, text, base, .. } => {
                let current = self.text(&path);
                match (&current, &base) {
                    (Some(now), Some(base)) if rev(now) != *base => Response::NoteConflict(NoteText { rev: rev(now), text: now.clone() }),
                    (Some(now), None) => Response::NoteConflict(NoteText { rev: rev(now), text: now.clone() }),
                    // As the service: a write based on a revision needs the file.
                    (None, Some(_)) => Response::Error(format!("{path} was moved or deleted; save it as a new note")),
                    _ => {
                        self.put(&path, &text);
                        self.writes.lock().unwrap().push(path);
                        Response::NoteSaved { rev: rev(&text) }
                    }
                }
            }
            Request::NoteRevs { paths } => Response::NoteRevs(paths.iter().map(|p| self.text(p).map(|t| rev(&t))).collect()),
            Request::NoteMkdir { path, .. } => {
                self.dirs.lock().unwrap().insert(path);
                Response::Done
            }
            Request::NoteRename { from, to, .. } => {
                let moved = self.files.lock().unwrap().remove(&from);
                match moved {
                    Some(text) => {
                        self.files.lock().unwrap().insert(to, text);
                        Response::Done
                    }
                    None => Response::Error("not found".into()),
                }
            }
            Request::NoteTrash { path, .. } => {
                self.files.lock().unwrap().remove(&path);
                Response::Done
            }
            Request::NoteSearch { query, .. } => {
                let query = query.to_lowercase();
                let mut hits = Vec::new();
                for (path, text) in self.files.lock().unwrap().iter() {
                    if file_name(path).to_lowercase().contains(&query) {
                        hits.push(nb::Hit { path: path.clone(), line: 0, text: file_name(path) });
                    }
                    for (i, line) in text.lines().enumerate().filter(|(_, l)| l.to_lowercase().contains(&query)) {
                        hits.push(nb::Hit { path: path.clone(), line: i as u32 + 1, text: line.to_owned() });
                    }
                }
                Response::NoteHits(hits)
            }
            Request::ReadBytes { path, .. } => match self.text(&path) {
                Some(text) => Response::Bytes(text.into_bytes()),
                None => Response::Error("not found".into()),
            },
            other => Response::Error(format!("unexpected {}", other.name())),
        })
    }
}

pub(super) struct Fixture {
    pub(super) app: App,
    pub(super) events: Receiver<AppEvent>,
    pub(super) sent: Receiver<ClientMsg>,
}

pub(super) fn fixture(disk: Arc<Disk>, doc: Doc) -> Fixture {
    let (tx, sent) = crossbeam_channel::bounded(256);
    let (_service, service_rx) = crossbeam_channel::bounded(1);
    let (app_tx, events) = crossbeam_channel::unbounded();
    let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), disk, crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
    app.connected = true;
    Fixture { app, events, sent }
}

impl Fixture {
    /// Runs background continuations until `done` (5 s at most).
    pub(super) fn settle(&mut self, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(&self.app) {
            assert!(std::time::Instant::now() < deadline, "background work did not finish");
            if let Ok(event) = self.events.recv_timeout(Duration::from_millis(20)) {
                self.app.on_app_event(event);
            }
        }
    }

    pub(super) fn key(&mut self, code: KeyCode, modifiers: Modifiers) {
        self.app.on_input(vec![crate::input::InputEvent::Key(KeyEvent::new(code, modifiers))]);
    }

    pub(super) fn open(&mut self, path: &str) {
        self.app.navigate(Space::Notebook, NavTarget::Note(path.into()));
        let path = path.to_owned();
        self.settle(move |app| app.notebook.info.is_some() && app.notebook.tab_index(&path).is_some_and(|i| app.notebook.tabs[i].loaded));
    }

    pub(super) fn tab(&self) -> &Tab {
        self.app.notebook.tab().expect("a tab")
    }

    /// Makes the shown tab's autosave due and runs it.
    pub(super) fn autosave(&mut self) {
        if let Some(tab) = self.app.notebook.tab_mut() {
            tab.changed_at = std::time::Instant::now().checked_sub(Duration::from_secs(2));
        }
        tick(&mut self.app);
        self.settle(|app| app.notebook.tab().is_none_or(|t| !t.saving));
    }
}

pub(super) fn note(path: &str) -> String {
    join(ROOT, &path.replace('/', "\\"))
}

// ---------- pure logic ----------

fn group(id: &str, name: &str, folder: Option<&str>) -> Group {
    Group { id: id.into(), name: name.into(), w: 40, h: 8, folder: folder.map(str::to_owned), ..Default::default() }
}

#[test]
fn paths_join_split_compare_and_relate() {
    assert_eq!(join(r"C:\nb\", "a.md"), r"C:\nb\a.md");
    assert_eq!(join("/srv/nb", "a.md"), "/srv/nb/a.md");
    assert_eq!(parent(r"C:\nb\a.md").as_deref(), Some(r"C:\nb"));
    assert_eq!(parent(r"C:\nb").as_deref(), Some(r"C:\"));
    assert_eq!(parent("/srv/nb").as_deref(), Some("/srv"));
    assert_eq!(file_name(r"C:\nb\Inbox\"), "Inbox");
    assert!(same_path(r"c:/NB/Inbox", r"C:\nb\inbox\"));
    assert!(!same_path("/srv/NB", "/srv/nb"), "other systems keep case");
    assert!(is_inside(r"C:\nb\Inbox\a.md", r"C:\nb") && is_inside(r"C:\nb", r"C:\nb\"));
    assert!(!is_inside(r"C:\nb2\a.md", r"C:\nb"), "a sibling with the same start is not inside");
    assert!(is_absolute(r"C:\x") && is_absolute("/x") && !is_absolute("Overview") && !is_absolute("Folder/Note"));
    assert_eq!(tree::rel(r"C:\nb", r"C:\nb\Groups\Docs\a.md"), "Groups/Docs/a.md");
    assert_eq!(tree::rel(r"C:\nb", r"C:\nb"), "");
    assert_eq!(tabs::relative_path(r"C:\nb\Groups\Docs", "../Inbox/b%20c.md#top"), r"C:\nb\Groups\Inbox\b c.md");
    assert_eq!(tabs::relative_path(r"C:\nb", "./img/x.png"), r"C:\nb\img\x.png");
}

#[test]
fn roots_are_the_notebook_group_folders_outside_it_the_canvas_folder_and_mounts() {
    let mut doc = Doc::default();
    doc.groups.push(group("g-in", "Inside", Some(r"C:\nb\Groups\Inside")));
    doc.groups.push(group("g-out", "Payments", Some(r"C:\work\pay")));
    doc.groups.push(group("g-none", "Ideas", None));
    doc.notebook_mounts = vec![r"D:\Vault".into(), r"c:\WORK\pay".into(), "  ".into()];
    let info = Info { root: ROOT.into(), canvas_dir: Some(CANVAS.into()), home: r"C:\home".into() };
    let roots = tree::roots(Some(&info), &doc);
    let labels: Vec<(&str, tree::RootKind)> = roots.iter().map(|r| (r.label.as_str(), r.kind)).collect();
    assert_eq!(labels, [("Notebook", tree::RootKind::Notebook), ("◫ Payments", tree::RootKind::Group), ("Canvas (canvas-wide)", tree::RootKind::CanvasWide), ("Vault", tree::RootKind::Mount)], "a group inside the root and a mount equal to a group folder are not roots again");
    assert_eq!(roots[1].group.as_deref(), Some("g-out"));
    assert_eq!(roots[2].place, Place::CanvasWide);
    // Before the notebook's folders are known: group folders and mounts only.
    let early = tree::roots(None, &doc);
    assert_eq!(early.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["◫ Inside", "◫ Payments", "Vault"]);
    assert_eq!(tree::root_of(&roots, r"C:\work\pay\ai-docs\plan.md").map(|r| r.label.as_str()), Some("◫ Payments"));
    assert_eq!(tree::root_of(&roots, r"C:\nb\Groups\Inside\x.md").map(|r| r.label.as_str()), Some("Notebook"));
    assert!(tree::root_of(&roots, r"E:\elsewhere.md").is_none());
}

fn entry(path: &str, root: &str, text: &str) -> Entry {
    let name = file_name(path);
    let kind = if text == "/" { Kind::Folder } else { Kind::of(&name) };
    let rel = if kind == Kind::Folder { format!("{}/", tree::rel(root, path)) } else { tree::rel(root, path) };
    let meta = nb::note_meta(kind, text);
    Entry { name: name.clone(), path: path.into(), rel: rel.clone(), kind, provenance: nb::classify(&rel, &meta, Place::Notes), title: name, ..Entry::default() }
}

fn listed(state: &mut State, dir: &str, entries: Vec<Entry>) {
    state.folders.insert(norm(dir), Folder { path: dir.into(), root: ROOT.into(), entries, at: Some(std::time::Instant::now()), ..Folder::default() });
}

/// Notebook root with a note for you, an agents' folder (only AI notes), a
/// mixed folder and an internal file.
fn sample_state() -> State {
    let mut state = State::default();
    let r = |p: &str| note(p);
    listed(&mut state, ROOT, vec![entry(&r("ai-docs"), ROOT, "/"), entry(&r("Mixed"), ROOT, "/"), entry(&r("Overview.md"), ROOT, "# Overview"), entry(&r(".hidden.md"), ROOT, "x")]);
    listed(&mut state, &r("ai-docs"), vec![entry(&r("ai-docs/plan.md"), ROOT, "# Plan"), entry(&r("ai-docs/log.md"), ROOT, "# Log")]);
    listed(&mut state, &r("Mixed"), vec![entry(&r("Mixed/mine.md"), ROOT, "# Mine"), entry(&r("Mixed/theirs.md"), ROOT, "---\naudience: ai\n---\n# Theirs")]);
    state.expanded.extend([norm(ROOT), norm(&r("ai-docs")), norm(&r("Mixed"))]);
    state
}

fn labels(t: &tree::Tree) -> Vec<String> {
    t.rows
        .iter()
        .map(|row| {
            let label = match &row.kind {
                RowKind::Root(root) => root.label.clone(),
                RowKind::Dir(e, _) | RowKind::File(e, _) => e.name.clone(),
                RowKind::Reports(n) => format!("Reports ({n})"),
                RowKind::Report(_, title, _) => title.clone(),
                RowKind::Message(text) => text.clone(),
            };
            format!("{}{label}", "  ".repeat(row.depth as usize))
        })
        .collect()
}

fn root_list() -> Vec<tree::Root> {
    vec![tree::Root { kind: tree::RootKind::Notebook, label: "Notebook".into(), path: ROOT.into(), place: Place::Notes, group: None }]
}

#[test]
fn the_tree_filters_audience_hides_internal_files_and_counts_what_it_hides() {
    let mut state = sample_state();
    let doc = Doc::default();
    let all = tree::build(&Inputs::of(&state, &doc), &root_list(), &[]);
    assert_eq!(labels(&all), ["Notebook", "  ai-docs", "    plan.md", "    log.md", "  Mixed", "    mine.md", "    theirs.md", "  Overview.md", "Reports (0)"], "internal files hidden by default");
    assert_eq!(all.hidden, 0);
    // Human: the agents' folder disappears (only hidden notes), its notes are counted.
    state.filter = Filter::Human;
    let human = tree::build(&Inputs::of(&state, &doc), &root_list(), &[]);
    assert_eq!(labels(&human), ["Notebook", "  Mixed", "    mine.md", "  Overview.md", "Reports (0)"]);
    assert_eq!((human.hidden, Filter::Human.hidden_label(human.hidden)), (3, "3 AI hidden".to_owned()));
    // The count does not depend on what is expanded: a closed folder's hidden notes count too.
    state.expanded.remove(&norm(&note("Mixed")));
    assert_eq!(tree::build(&Inputs::of(&state, &doc), &root_list(), &[]).hidden, 3);
    state.expanded.remove(&norm(ROOT));
    assert_eq!(tree::build(&Inputs::of(&state, &doc), &root_list(), &[]).hidden, 3);
    state.expanded.extend([norm(ROOT), norm(&note("Mixed"))]);
    // AI: the notes for you go (and the reports, shown or not).
    state.filter = Filter::Ai;
    let ai = tree::build(&Inputs::of(&state, &doc), &root_list(), &[("r1".into(), "Weekly".into(), true)]);
    assert_eq!(labels(&ai), ["Notebook", "  ai-docs", "    plan.md", "    log.md", "  Mixed", "    theirs.md", "Reports (1)"]);
    assert_eq!(ai.hidden, 3);
    // Show internal files.
    state.filter = Filter::All;
    state.show_internal = true;
    let internal = tree::build(&Inputs::of(&state, &doc), &root_list(), &[]);
    assert!(labels(&internal).contains(&"  .hidden.md".to_owned()));
    assert_eq!(Filter::Human.next(), Filter::All);
    assert_eq!(Filter::Ai.next(), Filter::Human);
}

#[test]
fn the_name_filter_shows_matches_with_their_folders_and_unlisted_folders_load() {
    let mut state = sample_state();
    let doc = Doc::default();
    state.expanded.remove(&norm(&note("Mixed")));
    state.name_filter.set("THEIR");
    let t = tree::build(&Inputs::of(&state, &doc), &root_list(), &[("r1".into(), "Weekly".into(), false)]);
    assert_eq!(labels(&t), ["Notebook", "  Mixed", "    theirs.md", "Reports (1)"], "a collapsed folder with a match opens");
    // A folder not listed yet shows Loading… under it.
    let mut state = State::default();
    state.expanded.insert(norm(ROOT));
    let t = tree::build(&Inputs::of(&state, &doc), &root_list(), &[]);
    assert_eq!(labels(&t), ["Notebook", "  Loading…", "Reports (0)"]);
    // Reports expand to their threads.
    state.expanded.insert(REPORTS.into());
    let t = tree::build(&Inputs::of(&state, &doc), &[], &[("r1".into(), "Weekly".into(), true)]);
    assert_eq!(labels(&t), ["Reports (1)", "  Weekly"]);
    assert_eq!(t.rows[1].key, "report:r1");
}

#[test]
fn group_folders_are_marked_in_the_tree() {
    let mut state = sample_state();
    let mut doc = Doc::default();
    doc.groups.push(group("g-mixed", "Mixed work", Some(&note("Mixed"))));
    state.filter = Filter::All;
    let t = tree::build(&Inputs::of(&state, &doc), &root_list(), &[]);
    let row = t.rows.iter().find(|r| matches!(&r.kind, RowKind::Dir(e, _) if e.name == "Mixed")).unwrap();
    assert_eq!(row.group.as_deref(), Some("g-mixed"));
}

#[test]
fn wiki_names_resolve_by_name_title_and_folder_nearest_first() {
    let known = |path: &str, title: &str| Known { path: path.into(), name: file_name(path), title: title.into(), ai: false };
    let list = vec![known(r"C:\nb\Groups\Docs\Plan.md", "Plan"), known(r"C:\nb\Inbox\plan.md", "Inbox plan"), known(r"C:\nb\Overview.md", "The overview"), known(r"C:\nb\Groups\Docs\ai-docs\GOALS.md", "Goals")];
    assert_eq!(tabs::resolve(&list, "plan", None).as_deref(), Some(r"C:\nb\Inbox\plan.md"), "the shortest path");
    assert_eq!(tabs::resolve(&list, "Plan", Some(r"C:\nb\Groups\Docs")).as_deref(), Some(r"C:\nb\Groups\Docs\Plan.md"), "the same folder first");
    assert_eq!(tabs::resolve(&list, "Plan.md#Steps", None).as_deref(), Some(r"C:\nb\Inbox\plan.md"));
    assert_eq!(tabs::resolve(&list, "The overview|see", None).as_deref(), Some(r"C:\nb\Overview.md"), "by title, alias dropped");
    assert_eq!(tabs::resolve(&list, "ai-docs/goals", None).as_deref(), Some(r"C:\nb\Groups\Docs\ai-docs\GOALS.md"), "a folder part");
    assert_eq!(tabs::resolve(&list, "Docs/plan", None).as_deref(), Some(r"C:\nb\Groups\Docs\Plan.md"));
    assert_eq!(tabs::resolve(&list, "Nothing", None), None);
    assert_eq!(tabs::link_name(" Plan#Steps|the plan "), "Plan");
}

#[test]
fn links_under_the_caret_and_in_rendered_cells() {
    assert_eq!(tabs::link_at("see [[Plan|the plan]] now", 6), Some(LinkAt::Note("Plan|the plan".into())));
    assert_eq!(tabs::link_at("see [[Plan]] now", 2), None);
    assert_eq!(tabs::link_at("fix DEC-031 today", 6), Some(LinkAt::Todo("DEC-031".into())));
    assert_eq!(tabs::link_at("(https://example.com/a).", 5), Some(LinkAt::Url("https://example.com/a".into())));
    let html = r##"<a href="#note:My%20Plan">x</a><a href="#todo:DEC-031">y</a><a href="https://a.b">z</a><a href="img/c d.md">w</a><a href="#top">t</a>"##;
    let rewritten = reader::cell_links(html);
    assert!(rewritten.contains(&format!("href=\"{}note:My%20Plan\"", tabs::LINK_HOST)), "{rewritten}");
    assert!(rewritten.contains(&format!("href=\"{}todo:DEC-031\"", tabs::LINK_HOST)));
    assert!(rewritten.contains("href=\"https://a.b\"") && rewritten.contains("href=\"#top\""));
    assert!(rewritten.contains(&format!("href=\"{}rel/img/c%20d.md\"", tabs::LINK_HOST)));
    // The cells keep these links (richtext keeps http(s) only).
    let lines = crate::richtext::layout(&crate::richtext::html(&reader::cell_links(&nb::markdown_html("See [[My Plan]] and DEC-031."))), 60);
    let mut links: Vec<String> = lines.iter().flat_map(|l| l.spans.iter().filter_map(|(_, _, link)| link.clone())).collect();
    links.dedup();
    assert_eq!(links, [format!("{}note:My%20Plan", tabs::LINK_HOST), format!("{}todo:DEC-031", tabs::LINK_HOST)]);
}

#[test]
fn completions_match_note_names_best_first() {
    let known = |name: &str| Known { path: format!(r"C:\nb\{name}"), name: name.into(), title: String::new(), ai: false };
    let list = vec![known("Overview.md"), known("Plan.md"), known("Old plan.md"), known("plan.md"), known("Release notes.md")];
    assert_eq!(tabs::completions(&list, "pla"), ["Plan", "Old plan"], "case-insensitive, once per name");
    assert_eq!(tabs::completions(&list, "").len(), 4);
}

#[test]
fn compare_marks_mine_and_theirs() {
    let diff = dialogs::diff("a\nb\nmine\nc", "a\nb\ntheirs\nc\nd");
    assert_eq!(diff, [(' ', "a".into()), (' ', "b".into()), ('-', "theirs".into()), ('+', "mine".into()), (' ', "c".into()), ('-', "d".into())]);
    assert_eq!(dialogs::diff("same", "same"), [(' ', "same".to_owned())]);
}

#[test]
fn quick_notes_are_named_by_the_minute_and_carry_their_provenance() {
    let local = crate::bus::Local { year: 2026, month: 10, day: 31, hour: 9, minute: 5 };
    assert_eq!(ops::capture_name(&local, 1), "2026-10-31 0905.md");
    assert_eq!(ops::capture_name(&local, 3), "2026-10-31 0905-3.md");
    let text = ops::new_text(Kind::Markdown, "");
    let meta = nb::markdown_meta(&text);
    assert_eq!((meta.get("audience"), meta.get("author")), (Some("human"), Some("human")));
    assert!(meta.get("created").is_some() && nb::markdown_title(&text).is_none());
    let titled = ops::new_text(Kind::Markdown, "Ideas");
    assert_eq!(nb::markdown_title(&titled).as_deref(), Some("Ideas"));
    let html = ops::new_text(Kind::Html, "A <b>");
    assert_eq!(nb::html_meta(&html).get("audience"), Some("human"));
    assert!(html.contains("<title>A &lt;b&gt;</title>"));
    assert!(ops::revision_message(r"C:\nb\a.md", " shorter ").contains(r"C:\nb\a.md: shorter"));
}

#[test]
fn outline_lists_headings_outside_code_and_front_matter() {
    let text = "---\ntitle: x\n---\n# One\ntext\n```\n# not\n```\n## Two ##\n#nope\n";
    assert_eq!(info::outline_of(text), [(3, 1, "One".to_owned()), (8, 2, "Two".to_owned())]);
}

// ---------- with a backend ----------

pub(super) fn type_text(f: &mut Fixture, text: &str) {
    for c in text.chars() {
        f.key(KeyCode::Char(c), Modifiers::NONE);
    }
}

#[test]
fn opening_lists_the_root_and_selects_the_note() {
    let disk = Disk::with(&[(&note("Overview.md"), "# Overview\n"), (&note("Inbox/idea.md"), "# Idea\n")]);
    let mut f = fixture(disk, Doc::default());
    f.app.switch_space(Space::Notebook);
    tick(&mut f.app);
    f.settle(|app| app.notebook.info.is_some() && app.notebook.folders.get(&norm(ROOT)).is_some_and(|d| d.at.is_some()));
    assert!(app_rows(&f.app).contains(&"  Overview.md".to_owned()));
    f.open(&note("Inbox/idea.md"));
    assert_eq!(f.app.notebook.selected.as_deref(), Some(norm(&note("Inbox/idea.md")).as_str()));
    assert!(f.app.notebook.expanded.contains(&norm(&note("Inbox"))));
    f.settle(|app| app.notebook.folders.get(&norm(&note("Inbox"))).is_some_and(|d| d.at.is_some()));
    assert!(app_rows(&f.app).contains(&"    idea.md".to_owned()));
    assert_eq!((f.tab().title.as_str(), f.tab().view), ("Idea", View::Rendered));
    assert!(goto_items(&f.app).iter().any(|i| i.label.starts_with("Idea") && i.target == NavTarget::Note(note("Inbox/idea.md"))));
    assert_eq!(f.sent.try_iter().filter(|m| matches!(m, ClientMsg::Launch { .. })).count(), 0, "viewing never starts a session");
}

fn app_rows(app: &App) -> Vec<String> {
    labels(&tree::tree(app))
}

#[test]
fn typing_saves_a_second_later_and_an_agents_note_becomes_mixed() {
    let ai = "---\naudience: ai\nauthor: ai\nagent: opencode\n---\n# Plan\n";
    let disk = Disk::with(&[(&note("plan.md"), ai)]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("plan.md"));
    assert_eq!(f.tab().provenance.author, nb::Author::Ai);
    // Ctrl+E from the explorer: editing takes the keyboard (typing goes into the note, not to the explorer's keys).
    f.app.set_focus(crate::app::Focus::Sidebar);
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    assert_eq!((f.tab().view, f.app.focus), (View::Source, crate::app::Focus::Canvas));
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, "More.");
    assert!(f.tab().unsaved && f.tab().text().ends_with("More."));
    // Not yet: autosave waits a second after the last change.
    tick(&mut f.app);
    assert!(!f.tab().saving);
    f.autosave();
    let saved = disk.text(&note("plan.md")).unwrap();
    assert!(saved.contains("author: mixed") && saved.ends_with("More."), "{saved}");
    assert!(!f.tab().unsaved);
    assert_eq!(f.tab().rev.as_deref(), Some(nb::rev_of(saved.as_bytes()).as_str()));
    assert_eq!(f.tab().provenance.author, nb::Author::Mixed);
    // Ctrl+S with nothing to save says so.
    f.key(KeyCode::Char('s'), Modifiers::CONTROL);
    assert!(f.app.notice.as_ref().is_some_and(|n| n.text == "Saved."));
}

#[test]
fn front_matter_written_on_save_keeps_the_caret_on_its_text() {
    // An agent's note by its folder, without front matter: the first save writes `author: mixed` above the caret.
    let disk = Disk::with(&[(&note("ai-docs/log.md"), "# Log\nStarted.")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("ai-docs/log.md"));
    assert_eq!(f.tab().provenance.author, nb::Author::Ai);
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, " One");
    f.autosave();
    assert!(disk.text(&note("ai-docs/log.md")).unwrap().starts_with("---\nauthor: mixed\n---\n"), "front matter written");
    type_text(&mut f, " two");
    assert!(f.tab().text().ends_with("# Log\nStarted. One two"), "typing goes on where it was: {:?}", f.tab().text());
}

#[test]
fn changes_on_disk_reload_a_clean_note_and_conflict_with_a_changed_one() {
    let disk = Disk::with(&[(&note("a.md"), "# A\none\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    // Clean: reloads silently.
    disk.put(&note("a.md"), "# A\ntwo\n");
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.text().contains("two")));
    assert!(f.tab().conflict.is_none() && !f.tab().unsaved);
    // Changed here and there: the banner.
    type_text(&mut f, "X");
    disk.put(&note("a.md"), "# A\nthree\n");
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    assert!(f.tab().text().starts_with('X'), "the buffer stays");
    // Autosave waits while there is a conflict.
    f.autosave();
    assert_eq!(disk.text(&note("a.md")).unwrap(), "# A\nthree\n");
    // Keep mine: written over theirs.
    tabs::keep_mine(&mut f.app, &note("a.md"));
    f.settle(|app| app.notebook.tab().is_some_and(|t| !t.saving));
    assert!(disk.text(&note("a.md")).unwrap().starts_with("X# A"));
    assert!(f.tab().conflict.is_none() && !f.tab().unsaved);
    // Again, and Take theirs.
    type_text(&mut f, "Y");
    disk.put(&note("a.md"), "# A\nfour\n");
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    tabs::take_theirs(&mut f.app, &note("a.md"));
    assert_eq!(f.tab().text(), "# A\nfour\n");
    assert!(!f.tab().unsaved && f.tab().conflict.is_none());
    // A missing file is said; writing it again is possible.
    disk.files.lock().unwrap().remove(&note("a.md"));
    f.app.notebook.poll_at = None;
    tick(&mut f.app);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.missing));
}

#[test]
fn a_save_based_on_an_old_revision_is_a_conflict() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "Z");
    disk.put(&note("a.md"), "# A changed\n");
    f.key(KeyCode::Char('s'), Modifiers::CONTROL);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.conflict.is_some()));
    assert_eq!(f.tab().conflict.as_ref().unwrap().text, "# A changed\n");
    assert_eq!(disk.text(&note("a.md")).unwrap(), "# A changed\n");
    // Compare shows both sides.
    dialogs::compare(&mut f.app);
    let Some(crate::dialogs::Modal::Notebook(dialog)) = f.app.modals.last() else { panic!("compare dialog") };
    let Dialog::Compare { lines, .. } = dialog.as_ref() else { panic!() };
    assert!(lines.contains(&('-', "# A changed".into())) && lines.contains(&('+', "Z# A".into())));
}

#[test]
fn tabs_switch_close_and_closing_saves_first() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("b.md"), "# B\n"), (&note("c.md"), "# C\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    for name in ["a.md", "b.md", "c.md"] {
        f.open(&note(name));
    }
    assert_eq!((f.app.notebook.tabs.len(), f.app.notebook.active), (3, 2));
    f.key(KeyCode::Tab, Modifiers::CONTROL);
    assert_eq!(f.tab().name, "a.md", "around to the first");
    f.key(KeyCode::Tab, Modifiers::CONTROL | Modifiers::SHIFT);
    assert_eq!(f.tab().name, "c.md");
    f.key(KeyCode::PageUp, Modifiers::CONTROL);
    assert_eq!(f.tab().name, "b.md");
    // Opening an open note shows its tab.
    f.open(&note("a.md"));
    assert_eq!((f.app.notebook.tabs.len(), f.tab().name.as_str()), (3, "a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "!");
    f.key(KeyCode::Char('w'), Modifiers::CONTROL);
    assert_eq!(f.app.notebook.tabs.len(), 2);
    f.settle(|app| app.notebook.closing.is_empty());
    assert_eq!(disk.text(&note("a.md")).unwrap(), "!# A\n", "written before it closed");
    assert_eq!(f.app.notebook.tabs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["b.md", "c.md"]);
}

#[test]
fn quitting_writes_what_was_typed_in_the_last_second() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("b.md"), "# B\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "1");
    f.open(&note("b.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    type_text(&mut f, "2");
    assert!(f.app.notebook.tabs.iter().all(|t| t.unsaved), "autosave is not due yet");
    // Quit from another space (no terminals: Ctrl+Q quits at once); the client then saves.
    f.app.switch_space(Space::Canvas);
    f.key(KeyCode::Char('q'), Modifiers::CONTROL);
    assert!(f.app.quit);
    f.app.save_now();
    assert_eq!((disk.text(&note("a.md")).unwrap().as_str(), disk.text(&note("b.md")).unwrap().as_str()), ("1# A\n", "2# B\n"));
    assert!(f.app.notebook.tabs.iter().all(|t| !t.unsaved));
    let writes = disk.writes.lock().unwrap().len();
    f.app.save_now();
    assert_eq!(disk.writes.lock().unwrap().len(), writes, "nothing more to write");
}

#[test]
fn back_and_forward_return_to_the_note_and_the_todo_you_were_at() {
    let disk = Disk::with(&[(&note("a.md"), "# A\nSee BRD-001.\n"), (&note("b.md"), "# B\n")]);
    let mut f = fixture(disk, Doc::default());
    f.app.bus.todos = Some(crate::tasks::tests::book_with("g1", 3, 0, 0));
    f.app.switch_space(Space::Notebook);
    // A note opened from the explorer (no jump), then a todo link in it.
    tabs::open_note(&mut f.app, &note("a.md"));
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.loaded));
    assert_eq!(current_target(&f.app), NavTarget::Note(note("a.md")));
    tabs::follow_link(&mut f.app, "#todo:BRD-001");
    assert_eq!((f.app.space, f.app.tasks.selected.as_deref()), (Space::Tasks, Some("BRD-001")));
    // Another todo selected in Tasks (no jump), then a note from there.
    f.app.run(crate::dialogs::Action::Tasks(crate::tasks::Action::Select("BRD-002".into())));
    assert_eq!(crate::tasks::current_target(&f.app), NavTarget::Todo("BRD-002".into()));
    f.app.navigate(Space::Notebook, NavTarget::Note(note("b.md")));
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.name == "b.md" && t.loaded));
    f.key(KeyCode::Left, Modifiers::ALT);
    assert_eq!((f.app.space, f.app.tasks.selected.as_deref()), (Space::Tasks, Some("BRD-002")), "back to the todo you left");
    f.key(KeyCode::Left, Modifiers::ALT);
    assert_eq!((f.app.space, f.tab().name.as_str()), (Space::Notebook, "a.md"), "back to the note you jumped from");
    f.key(KeyCode::Right, Modifiers::ALT);
    assert_eq!((f.app.space, f.app.tasks.selected.as_deref()), (Space::Tasks, Some("BRD-002")));
    f.key(KeyCode::Right, Modifiers::ALT);
    assert_eq!((f.app.space, f.tab().name.as_str()), (Space::Notebook, "b.md"));
    // Nothing selected in Tasks: its board.
    f.app.tasks.selected = None;
    assert_eq!(crate::tasks::current_target(&f.app), NavTarget::Board("board".into()));
}

#[test]
fn a_notes_todo_links_name_only_todos_the_boards_have() {
    let disk = Disk::with(&[(&note("a.md"), "# A\nFix BRD-002 before the ISO-8601 change.\n")]);
    let mut f = fixture(disk, Doc::default());
    f.app.bus.todos = Some(crate::tasks::tests::book_with("g1", 3, 0, 0));
    f.open(&note("a.md"));
    let area = Rect::new(0, 0, 60, 40);
    let mut buf = Buffer::empty(area);
    draw_info(&mut f.app, &mut buf, area);
    let panel: String = (0..area.height).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect();
    assert!(panel.contains("☑ BRD-002 Todo 1") && !panel.contains("ISO-8601"), "only real todos, with their titles:\n{panel}");
    // A link to an id the boards do not have stays here and says so.
    tabs::follow_link(&mut f.app, "#todo:ISO-8601");
    assert_eq!(f.app.space, Space::Notebook);
    assert!(f.app.notice.as_ref().is_some_and(|n| n.text == "There is no todo ISO-8601 on this canvas's boards."));
    tabs::follow_link(&mut f.app, "#todo:BRD-002");
    assert_eq!((f.app.space, f.app.tasks.selected.as_deref(), f.app.right_detail.clone()), (Space::Tasks, Some("BRD-002"), Some(crate::side::RightDetail::Todo("BRD-002".into()))));
    // Before the boards arrive, any id may be a todo.
    f.app.bus.todos = None;
    f.app.navigate(Space::Tasks, NavTarget::Todo("ISO-8601".into()));
    assert_eq!(f.app.tasks.selected.as_deref(), Some("ISO-8601"));
}

#[test]
fn quick_capture_writes_an_inbox_note_and_opens_it_in_source_mode() {
    let disk = Disk::with(&[(&note("Overview.md"), "# Overview\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    assert_eq!(f.app.space, Space::Canvas);
    let alt_n = KeyEvent::new(KeyCode::Char('n'), Modifiers::ALT);
    assert!(is_capture_key(&alt_n) && !is_capture_key(&KeyEvent::new(KeyCode::Char('n'), Modifiers::ALT | Modifiers::CONTROL)));
    f.key(KeyCode::Char('n'), Modifiers::ALT);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.loaded));
    assert_eq!(f.app.space, Space::Notebook, "from the canvas too");
    let tab = f.tab();
    assert_eq!(tab.view, View::Source);
    assert!(tab.path.starts_with(&note("Inbox\\")) && tab.name.ends_with(".md"), "{}", tab.path);
    let stamp = tab.name.trim_end_matches(".md");
    assert!(stamp.len() == 15 && stamp.as_bytes()[4] == b'-' && stamp.as_bytes()[10] == b' ', "{stamp}");
    let text = disk.text(&tab.path).unwrap();
    assert_eq!(nb::markdown_meta(&text).get("audience"), Some("human"));
    assert_eq!(f.tab().editor.cursor().line, f.tab().editor.line_count() - 1, "the caret at the end");
    // A second one in the same minute does not overwrite it.
    let first = f.tab().path.clone();
    capture(&mut f.app);
    f.settle(|app| app.notebook.tabs.len() == 2 && app.notebook.tab().is_some_and(|t| t.loaded));
    assert_ne!(f.tab().path, first);
}

#[test]
fn names_open_notes_and_unknown_names_offer_a_new_note() {
    let disk = Disk::with(&[(&note("Groups/Docs/Plan.md"), "# Plan\n"), (&note("a.md"), "# A\n[[Plan]]\n")]);
    let mut f = fixture(disk, Doc::default());
    f.open(&note("a.md"));
    // Not listed: found by a name search.
    f.app.navigate(Space::Notebook, NavTarget::Note("Plan".into()));
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.name == "Plan.md"));
    // Nothing called so: the new-note dialog, next to the shown note.
    f.app.navigate(Space::Notebook, NavTarget::Note("Brand new".into()));
    f.settle(|app| !app.modals.is_empty());
    let Some(crate::dialogs::Modal::Notebook(dialog)) = f.app.modals.last() else { panic!("a dialog") };
    let Dialog::Name { purpose: NamePurpose::NewNote { folder }, input, .. } = dialog.as_ref() else { panic!("{dialog:?}") };
    assert_eq!((folder.as_str(), input.text.as_str()), (note("Groups/Docs").as_str(), "Brand new"));
    // Enter creates it and opens it.
    f.key(KeyCode::Enter, Modifiers::NONE);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.name == "Brand new.md" && t.loaded));
    assert_eq!(f.tab().title, "Brand new");
}

#[test]
fn the_explorer_keys_filter_open_rename_move_and_trash() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("ai-docs/plan.md"), "# Plan\n"), (&note("Inbox/x.md"), "x")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.app.switch_space(Space::Notebook);
    tick(&mut f.app);
    f.settle(|app| app.notebook.folders.get(&norm(ROOT)).is_some_and(|d| d.at.is_some()));
    f.app.set_focus(crate::app::Focus::Sidebar);
    assert_eq!(f.app.notebook.filter, Filter::All);
    f.key(KeyCode::Char('h'), Modifiers::ALT);
    assert_eq!(f.app.notebook.filter, Filter::Ai);
    f.key(KeyCode::Char('h'), Modifiers::ALT);
    f.key(KeyCode::Char('h'), Modifiers::ALT);
    assert_eq!(f.app.notebook.filter, Filter::All);
    // Down to a.md (Notebook, Inbox, ai-docs, a.md) and Enter opens it.
    for _ in 0..4 {
        f.key(KeyCode::Down, Modifiers::NONE);
    }
    assert_eq!(f.app.notebook.selected.as_deref(), Some(norm(&note("a.md")).as_str()), "{:?}", app_rows(&f.app));
    f.key(KeyCode::Enter, Modifiers::NONE);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.loaded && t.name == "a.md"));
    // `/` filters names.
    f.key(KeyCode::Char('/'), Modifiers::NONE);
    for c in "pla".chars() {
        f.key(KeyCode::Char(c), Modifiers::NONE);
    }
    assert!(f.app.notebook.filtering && f.app.notebook.name_filter.text == "pla");
    f.key(KeyCode::Esc, Modifiers::NONE);
    assert!(!f.app.notebook.filtering && f.app.notebook.name_filter.text.is_empty());
    // F2 renames (the extension stays) and the tab follows.
    f.app.notebook.selected = Some(norm(&note("a.md")));
    f.key(KeyCode::F(2), Modifiers::NONE);
    assert!(matches!(f.app.modals.last(), Some(crate::dialogs::Modal::Notebook(_))));
    f.key(KeyCode::Char('u'), Modifiers::CONTROL);
    type_text(&mut f, "renamed");
    f.key(KeyCode::Enter, Modifiers::NONE);
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.name == "renamed.md"));
    assert!(disk.text(&note("renamed.md")).is_some() && disk.text(&note("a.md")).is_none());
    // Move to the Inbox.
    ops::move_to(&mut f.app, &note("renamed.md"), &note("Inbox")).unwrap();
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.path == note("Inbox/renamed.md")));
    assert_eq!(ops::move_to(&mut f.app, &note("Inbox/renamed.md"), &note("Inbox")), Err("It is in that folder already.".into()));
    assert!(ops::move_to(&mut f.app, &note("Inbox"), &note("Inbox/sub")).is_err(), "not into itself");
    // Del asks, then the trash takes it and its tab closes.
    let moved = norm(&note("Inbox/renamed.md"));
    f.settle(move |app| tree::tree(app).rows.iter().any(|r| r.key == moved));
    f.app.notebook.selected = Some(norm(&note("Inbox/renamed.md")));
    f.app.set_focus(crate::app::Focus::Sidebar);
    f.key(KeyCode::Delete, Modifiers::NONE);
    assert!(matches!(f.app.modals.last(), Some(crate::dialogs::Modal::Confirm(_))));
    f.key(KeyCode::Enter, Modifiers::NONE);
    f.settle(|app| app.notebook.tabs.is_empty());
    assert!(disk.text(&note("Inbox/renamed.md")).is_none());
}

#[test]
fn mark_reviewed_writes_the_mark_into_open_and_closed_notes() {
    let ai = "---\naudience: ai\nauthor: ai\n---\n# Plan\n";
    let disk = Disk::with(&[(&note("plan.md"), ai), (&note("other.md"), ai)]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("plan.md"));
    click(&mut f.app, Hit::MarkReviewed, crate::input::MouseEvent::at_cell(crate::input::MouseEventKind::Down(crate::input::MouseButton::Left), 0, 0, Modifiers::NONE));
    f.settle(|app| app.notebook.tab().is_some_and(|t| t.provenance.reviewed));
    let text = disk.text(&note("plan.md")).unwrap();
    assert_eq!(nb::markdown_meta(&text).get("reviewed"), Some("true"));
    assert_eq!(nb::markdown_meta(&text).get("author"), Some("ai"), "a mark is not an edit");
    assert!(!f.tab().unsaved && f.tab().text() == text, "the open tab shows it");
    run(&mut f.app, Action::MarkReviewed(note("other.md")));
    f.settle(|app| app.notice.as_ref().is_some_and(|n| n.text.starts_with("other.md marked reviewed")));
    assert_eq!(nb::markdown_meta(&disk.text(&note("other.md")).unwrap()).get("reviewed"), Some("true"));
    // With unsaved changes the mark joins them (and the note becomes mixed).
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, "edit");
    let text = f.tab().text().replace("reviewed: true\n", "");
    f.app.notebook.tab_mut().unwrap().editor.set_text(&text);
    f.app.notebook.tab_mut().unwrap().changed();
    run(&mut f.app, Action::MarkReviewed(note("plan.md")));
    f.settle(|app| app.notebook.tab().is_some_and(|t| !t.saving && !t.unsaved));
    let text = disk.text(&note("plan.md")).unwrap();
    assert_eq!((nb::markdown_meta(&text).get("reviewed"), nb::markdown_meta(&text).get("author")), (Some("true"), Some("mixed")));
    assert!(text.ends_with("edit"));
}

#[test]
fn ask_agent_sends_the_path_to_a_session_of_the_group() {
    let disk = Disk::with(&[(r"C:\work\pay\ai-docs\plan.md", "# Plan\n")]);
    let mut doc = Doc::default();
    doc.groups.push(group("g-pay", "Payments", Some(r"C:\work\pay")));
    let node = |id: &str, name: &str, y: i32| serde_json::from_value::<crate::doc::Node>(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": 2, "y": y, "cwd": r"C:\work" })).unwrap();
    doc.nodes.push(node("s-pay", "Pay work", 2));
    doc.nodes.push(node("s-out", "Elsewhere", 30));
    let mut f = fixture(disk, doc);
    f.open(r"C:\work\pay\ai-docs\plan.md");
    assert_eq!(dialogs::sessions_for(&f.app, r"C:\work\pay\ai-docs\plan.md"), [("s-pay".to_owned(), "Pay work".to_owned())], "the group's sessions");
    dialogs::ask_agent(&mut f.app);
    for c in "shorter".chars() {
        f.key(KeyCode::Char(c), Modifiers::NONE);
    }
    f.key(KeyCode::Enter, Modifiers::NONE);
    assert!(f.app.modals.is_empty());
    let sent: Vec<serde_json::Value> = f.sent.try_iter().filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str(&json).ok() } else { None }).collect();
    let event = &sent.last().expect("a bus message")["event"];
    assert_eq!((event["type"].as_str(), event["to"].as_str(), event["group"].as_str()), (Some("message"), Some("s-pay"), Some("g-pay")));
    assert!(event["data"]["text"].as_str().unwrap().contains(r"C:\work\pay\ai-docs\plan.md: shorter"));
    assert_eq!(f.app.launching.len(), 0, "asking never starts a session");
}

#[test]
fn settings_rows_edit_the_notebook_folder_and_mounts() {
    let mut f = fixture(Disk::with(&[]), Doc::default());
    let rows = setting_rows(&f.app);
    assert_eq!(rows.iter().map(|(r, l, v)| (*r, l.as_str(), v.as_str())).collect::<Vec<_>>(), [(SettingRow::Folder, "Notebook folder", r"<home>\notebook (default)"), (SettingRow::Mounts, "Notebook mounts…", "none")]);
    run(&mut f.app, Action::Setting(SettingRow::Folder));
    type_text(&mut f, r"D:\Notes");
    f.key(KeyCode::Enter, Modifiers::NONE);
    assert_eq!(f.app.doc.notebook_root.as_deref(), Some(r"D:\Notes"));
    assert!(f.app.dirty_at.is_some(), "a document edit");
    f.settle(|app| app.notebook.info.as_ref().is_some_and(|i| i.root == r"D:\Notes"));
    run(&mut f.app, Action::Setting(SettingRow::Folder));
    f.key(KeyCode::Char('u'), Modifiers::CONTROL);
    f.key(KeyCode::Enter, Modifiers::NONE);
    assert_eq!(f.app.doc.notebook_root, None, "empty: the default");
    run(&mut f.app, Action::Setting(SettingRow::Mounts));
    type_text(&mut f, r"D:\Vault");
    f.key(KeyCode::Enter, Modifiers::NONE);
    type_text(&mut f, r"d:\vault");
    f.key(KeyCode::Enter, Modifiers::NONE);
    type_text(&mut f, r"E:\Other");
    f.key(KeyCode::Enter, Modifiers::NONE);
    assert_eq!(f.app.doc.notebook_mounts, [r"D:\Vault", r"E:\Other"], "once each");
    f.key(KeyCode::Delete, Modifiers::NONE);
    assert_eq!(f.app.doc.notebook_mounts, [r"E:\Other"]);
    f.key(KeyCode::Esc, Modifiers::NONE);
    assert!(f.app.modals.is_empty());
    assert_eq!(setting_rows(&f.app)[1].2, "1 folder");
    // The Settings view lists both rows; Enter on one opens its dialog.
    f.app.side_view = crate::side::SideView::Settings;
    let rows = f.app.build_list();
    let at = rows.iter().position(|r| r.item == crate::side::ItemRef::Notebook(SettingRow::Folder)).expect("the Notebook folder row");
    assert_eq!((rows[at].label.as_str(), rows[at + 1].item.clone()), ("Notebook folder", crate::side::ItemRef::Notebook(SettingRow::Mounts)));
    f.app.list_activate(at);
    assert!(matches!(f.app.modals.last(), Some(crate::dialogs::Modal::Notebook(d)) if matches!(d.as_ref(), Dialog::Name { purpose: NamePurpose::RootFolder, .. })));
}

#[test]
fn the_html_pane_is_published_in_reading_mode_only() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n\nSee [[B]].\n"), (&note("page.html"), "<html><body><h1>P</h1></body></html>")]);
    let mut f = fixture(disk, Doc::default());
    f.open(&note("a.md"));
    let area = ratatui::layout::Rect::new(0, 0, 150, 40);
    let render = |app: &mut App| {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        crate::ui::render(app, &mut buf, area, None)
    };
    f.app.render_mode = crate::view::RenderMode::Cells;
    assert!(render(&mut f.app).html.is_none(), "the TUI draws cells");
    assert!(f.app.layout.hits.iter().any(|(_, h)| matches!(h, crate::dialogs::Hit::Notebook(Hit::Link(l)) if l.ends_with("note:B"))), "a [[link]] is clickable in the cells");
    f.app.render_mode = crate::view::RenderMode::Gpu;
    let view = render(&mut f.app);
    let pane = view.html.expect("the pane");
    assert!(pane.id.starts_with(&format!("note:{}#", note("a.md"))));
    assert!(matches!(&pane.source, crate::view::HtmlSource::Document(page) if page.contains("#note:B")));
    // Not with a dialog open, not in Source.
    f.app.modals.push(crate::dialogs::Modal::Help);
    assert!(render(&mut f.app).html.is_none());
    f.app.modals.clear();
    tabs::set_view(&mut f.app, View::Source);
    assert!(render(&mut f.app).html.is_none());
    // An HTML note is served from its file, scripts as the canvas says.
    f.open(&note("page.html"));
    f.app.doc.notebook_scripts.push(note("page.html"));
    let view = render(&mut f.app);
    assert!(matches!(view.html.map(|p| p.source), Some(crate::view::HtmlSource::File { path, root, scripts: true }) if path == note("page.html") && root == ROOT));
    // Print asks the GUI once.
    click(&mut f.app, Hit::Print, crate::input::MouseEvent::at_cell(crate::input::MouseEventKind::Down(crate::input::MouseButton::Left), 0, 0, Modifiers::NONE));
    assert!(render(&mut f.app).html_print);
    assert!(!render(&mut f.app).html_print);
}

#[test]
fn completion_offers_note_names_after_double_brackets() {
    let disk = Disk::with(&[(&note("a.md"), "# A\n"), (&note("Planning.md"), "# Planning\n"), (&note("Plans.md"), "# Plans\n")]);
    let mut f = fixture(disk.clone(), Doc::default());
    f.open(&note("a.md"));
    f.key(KeyCode::Char('e'), Modifiers::CONTROL);
    f.key(KeyCode::End, Modifiers::CONTROL);
    type_text(&mut f, "[[plann");
    f.settle(|app| app.notebook.completion.as_ref().is_some_and(|c| c.items.first().map(String::as_str) == Some("Planning")));
    f.key(KeyCode::Tab, Modifiers::NONE);
    assert!(f.tab().text().ends_with("[[Planning]]"), "{}", f.tab().text());
    assert!(f.app.notebook.completion.is_none());
    let line = f.tab().editor.cursor().line;
    assert_eq!(tabs::link_at(f.tab().editor.line(line), 3), Some(LinkAt::Note("Planning".into())));
}
