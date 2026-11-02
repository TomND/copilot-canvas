//! The info panel (the right pane in the Notebook, and `RightDetail::Note`):
//! outline, links (group, boards, todos, notes), backlinks, properties and
//! provenance of a note.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use canvas_model::notebook::{self as nb, Kind, Source};

use super::{Backlinks, Detail, Hit, file_name, load, norm, same_path};
use crate::app::App;
use crate::dialogs::Hit as AppHit;
use crate::paint::{ACCENT, BLUE, FAINT, GRAY, LIGHT, MUTED, PURPLE, TEXT, bold, fg, outline, put, truncate};
use crate::spaces::{NavTarget, Space};

/// Backlinks are searched again after this long.
const BACKLINKS_EVERY: Duration = Duration::from_secs(30);
/// Most notes whose backlinks are kept (the open ones always are).
const BACKLINKS_KEPT: usize = 64;
/// Most outline headings listed.
const OUTLINE_MAX: usize = 40;

/// What the panel shows of a note's text (worked out once per version of it).
#[derive(Debug)]
pub struct Analysis {
    kind: Kind,
    title: String,
    meta: nb::Meta,
    headings: Vec<(usize, usize, String)>,
    todos: Vec<String>,
    notes: Vec<String>,
}

impl Analysis {
    fn of(kind: Kind, text: &str, name: &str) -> Self {
        let meta = nb::note_meta(kind, text);
        let markdown = kind == Kind::Markdown;
        Analysis {
            kind,
            title: nb::title(kind, text, &meta, name),
            headings: if markdown { outline_of(text).into_iter().take(OUTLINE_MAX).collect() } else { Vec::new() },
            todos: if markdown { nb::todo_ids(text) } else { Vec::new() },
            notes: if markdown { nb::note_links(text) } else { Vec::new() },
            meta,
        }
    }
}

/// One line of the panel: its text, style, indent and click target.
struct Line {
    text: String,
    style: Style,
    indent: u16,
    target: Option<Hit>,
}

fn line(text: impl Into<String>, style: Style, indent: u16, target: Option<Hit>) -> Line {
    Line { text: text.into(), style, indent, target }
}

/// What the panel shows of a note (its tab's text, else its text loaded once
/// for a detail) and its provenance; the text is gone through once per version.
fn note_text(app: &mut App, path: &str) -> Option<(Arc<Analysis>, nb::Provenance)> {
    let name = file_name(path);
    let cached = |app: &App, key: &str| app.notebook.info_cache.as_ref().filter(|(k, _)| k == key).map(|(_, a)| a.clone());
    let store = |app: &mut App, key: String, analysis: Analysis| {
        let analysis = Arc::new(analysis);
        app.notebook.info_cache = Some((key, analysis.clone()));
        analysis
    };
    if let Some(index) = app.notebook.tab_index(path) {
        let tab = &app.notebook.tabs[index];
        if !tab.loaded {
            return None;
        }
        let (key, kind, provenance) = (format!("{}\ntab {} {}", norm(path), tab.id, tab.version), tab.kind, tab.provenance.clone());
        let analysis = match cached(app, &key) {
            Some(analysis) => analysis,
            None => {
                let analysis = Analysis::of(kind, &app.notebook.tabs[index].text(), &name);
                store(app, key, analysis)
            }
        };
        return Some((analysis, provenance));
    }
    let kind = Kind::of(&name);
    match &app.notebook.detail {
        Some(detail) if same_path(&detail.path, path) => match &detail.text {
            Some(Ok(note)) => {
                let key = format!("{}\nrev {}", norm(path), note.rev);
                let analysis = match cached(app, &key) {
                    Some(analysis) => analysis,
                    None => {
                        let analysis = Analysis::of(kind, &note.text, &name);
                        store(app, key, analysis)
                    }
                };
                let roots = super::tree::roots(app.notebook.info.as_ref(), &app.doc);
                let (rel, place) = super::tree::root_of(&roots, path).map(|r| (super::tree::rel(&r.path, path), r.place)).unwrap_or((name.clone(), nb::Place::Notes));
                let provenance = nb::classify(&rel, &analysis.meta, place);
                Some((analysis, provenance))
            }
            _ => None,
        },
        _ => {
            app.notebook.detail = Some(Detail { path: path.to_owned(), text: None });
            if kind.is_text() {
                let asked = path.to_owned();
                let wanted = asked.clone();
                load::background(
                    app,
                    move |backend| backend.note_read(&asked).map_err(|e| format!("{e:#}")),
                    move |app, result| {
                        if let Some(detail) = app.notebook.detail.as_mut().filter(|d| same_path(&d.path, &wanted)) {
                            detail.text = Some(result);
                        }
                    },
                );
            }
            None
        }
    }
}

/// Searches who links to a note (`[[stem`), now and then.
fn backlinks(app: &mut App, path: &str) -> Option<Backlinks> {
    let key = norm(path);
    let due = app.notebook.backlinks.get(&key).is_none_or(|b| !b.loading && b.at.is_none_or(|t| t.elapsed() >= BACKLINKS_EVERY));
    if due && app.notebook.info.is_some() {
        let entry = app.notebook.backlinks.entry(key.clone()).or_default();
        entry.loading = true;
        let stem = nb::stem(&file_name(path)).to_owned();
        let me = path.to_owned();
        load::search(app, &format!("[[{stem}"), 60, move |app, result| {
            let wanted: Vec<String> = ["]]", "|", "#"].iter().map(|end| format!("[[{}{end}", stem.to_lowercase())).collect();
            let hits = result.unwrap_or_default().into_iter().filter(|h| h.line > 0 && !same_path(&h.path, &me) && wanted.iter().any(|w| h.text.to_lowercase().contains(w))).collect();
            let entry = app.notebook.backlinks.entry(norm(&me)).or_default();
            entry.hits = hits;
            entry.loading = false;
            entry.at = Some(Instant::now());
            // Bounded: the open notes' and the shown detail's stay.
            let state = &mut app.notebook;
            if state.backlinks.len() > BACKLINKS_KEPT {
                let keep: Vec<String> = state.tabs.iter().map(|t| norm(&t.path)).chain(state.detail.as_ref().map(|d| norm(&d.path))).chain([norm(&me)]).collect();
                state.backlinks.retain(|key, b| b.loading || keep.contains(key));
            }
        });
    }
    app.notebook.backlinks.get(&key).cloned()
}

/// Markdown headings outside code fences: (line index, level, text).
pub fn outline_of(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let body_start = nb::frontmatter(text).map(|f| text[..f.end].lines().count()).unwrap_or(0);
    for (i, raw) in text.lines().enumerate().skip(body_start) {
        let l = raw.trim_start();
        if let Some(open) = fence {
            if l.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = ["```", "~~~"].into_iter().find(|f| l.starts_with(*f)) {
            fence = Some(open);
            continue;
        }
        let level = l.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && l[level..].starts_with([' ', '\t']) {
            let title = l[level..].trim().trim_end_matches('#').trim().to_owned();
            if !title.is_empty() {
                out.push((i, level, title));
            }
        }
    }
    out
}

/// "from its front matter" and friends.
fn source_label(source: Source) -> &'static str {
    match source {
        Source::Frontmatter => "from its front matter",
        Source::Folder => "from its folder (inferred)",
        Source::FileType => "HTML is for you (inferred)",
        Source::Default => "by default (inferred)",
    }
}

/// The panel's lines for a note.
fn lines_for(app: &mut App, path: &str) -> Vec<Line> {
    let head = bold(GRAY);
    let mut out = Vec::new();
    let name = file_name(path);
    let Some((note, provenance)) = note_text(app, path) else {
        out.push(line(format!("✎ {name}"), bold(TEXT), 0, None));
        let why = match app.notebook.detail.as_ref().and_then(|d| d.text.as_ref()) {
            Some(Err(error)) => error.clone(),
            _ if !kind_is_text(&name) => "No details for this kind of file.".into(),
            _ => "Loading…".into(),
        };
        out.push(line(why, fg(MUTED), 0, None));
        return out;
    };
    let (kind, meta) = (note.kind, &note.meta);
    out.push(line(format!("✎ {}", note.title), bold(TEXT), 0, None));
    out.push(line(name.clone(), fg(MUTED), 0, None));
    // Outline (Markdown).
    if !note.headings.is_empty() {
        out.push(line("", fg(TEXT), 0, None));
        out.push(line("OUTLINE", head, 0, None));
        let shown = app.notebook.tab().is_some_and(|t| same_path(&t.path, path));
        for (at, level, title) in &note.headings {
            let target = shown.then_some(Hit::Outline(*at));
            out.push(line(title.clone(), if target.is_some() { fg(TEXT) } else { fg(GRAY) }, (*level as u16 - 1) * 2, target));
        }
    }
    // Linked: the group, its boards, todos and notes the text names.
    let mut linked = Vec::new();
    if let Some(group) = super::explorer::group_of_path(app, path).and_then(|id| app.doc.group(&id).map(|g| (g.id.clone(), g.name.clone()))) {
        linked.push(line(format!("◫ {}", group.1), fg(BLUE), 0, Some(Hit::Go(Space::Canvas, NavTarget::Group(group.0.clone())))));
        if let Some(counts) = crate::tasks::group_counts(app, &group.0) {
            let text = format!("☑ {} open · {} in progress · {} proposed", counts.open, counts.in_progress, counts.proposed);
            linked.push(line(text, fg(BLUE), 0, Some(Hit::Go(Space::Tasks, NavTarget::Group(group.0)))));
        }
    }
    if kind == Kind::Markdown {
        // Ids the todo boards do not have (`ISO-8601`, `RFC-3339`) are no todos once the boards are known.
        let book = crate::tasks::book(app);
        for id in &note.todos {
            match book.map(|b| b.todo(id)) {
                Some(None) => {}
                Some(Some(todo)) => linked.push(line(format!("☑ {} {}", todo.id, todo.title), fg(BLUE), 0, Some(Hit::Go(Space::Tasks, NavTarget::Todo(todo.id.clone()))))),
                None => linked.push(line(format!("☑ {id}"), fg(BLUE), 0, Some(Hit::Go(Space::Tasks, NavTarget::Todo(id.clone()))))),
            }
        }
        for link in &note.notes {
            linked.push(line(format!("✎ {link}"), fg(BLUE), 0, Some(Hit::OpenNote(super::tabs::link_name(link).to_owned()))));
        }
    }
    if !linked.is_empty() {
        out.push(line("", fg(TEXT), 0, None));
        out.push(line("LINKED", head, 0, None));
        out.extend(linked);
    }
    // Backlinks.
    out.push(line("", fg(TEXT), 0, None));
    out.push(line("BACKLINKS", head, 0, None));
    match backlinks(app, path) {
        Some(found) if !found.hits.is_empty() => {
            for hit in found.hits.iter().take(30) {
                out.push(line(format!("✎ {}:{}", file_name(&hit.path), hit.line), fg(BLUE), 0, Some(Hit::OpenNote(hit.path.clone()))));
                out.push(line(hit.text.trim().to_owned(), fg(MUTED), 2, None));
            }
        }
        Some(found) if found.loading && found.at.is_none() => out.push(line("Searching…", fg(MUTED), 0, None)),
        _ if app.notebook.info.is_none() => out.push(line("(the notebook is not available)", fg(MUTED), 0, None)),
        _ => out.push(line("(none)", fg(MUTED), 0, None)),
    }
    // Properties.
    if !meta.is_empty() {
        out.push(line("", fg(TEXT), 0, None));
        out.push(line("PROPERTIES", head, 0, None));
        for (key, value) in &meta.0 {
            out.push(line(format!("{key}: {value}"), fg(TEXT), 0, None));
        }
    }
    // Provenance.
    out.push(line("", fg(TEXT), 0, None));
    out.push(line("PROVENANCE", head, 0, None));
    let (audience, color) = match provenance.audience {
        nb::Audience::Human => ("● For you", ACCENT),
        nb::Audience::Ai => ("◇ For agents", PURPLE),
    };
    out.push(line(format!("{audience}, {}", source_label(provenance.source)), fg(color), 0, None));
    out.push(line(super::reader::author_line(&provenance), fg(GRAY), 0, None));
    if let Some(session) = &provenance.session {
        match super::reader::session_node(app, session) {
            Some((id, name)) => {
                out.push(line(format!("session {name}"), fg(GRAY), 0, None));
                out.push(line("View session", fg(BLUE).add_modifier(Modifier::UNDERLINED), 2, Some(Hit::ViewSession(id))));
            }
            None => out.push(line(format!("session {session} (not on this canvas)"), fg(GRAY), 0, None)),
        }
    }
    if let Some(created) = &provenance.created {
        let age = super::reader::age(Some(created)).map(|a| format!(" ({a})")).unwrap_or_default();
        out.push(line(format!("created {created}{age}"), fg(GRAY), 0, None));
    }
    out.push(line(if provenance.reviewed { "✓ reviewed" } else { "not reviewed" }, fg(GRAY), 0, None));
    if provenance.passive {
        out.push(line("a passive session's memory", fg(GRAY), 0, None));
    }
    if provenance.internal {
        out.push(line("an internal file of the canvas", fg(GRAY), 0, None));
    }
    out
}

fn kind_is_text(name: &str) -> bool {
    Kind::of(name).is_text()
}

/// Draws the panel for `path` (`None`: no note is open).
pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect, path: Option<&str>) -> Option<(u16, u16)> {
    outline(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, &LIGHT, fg(FAINT), area);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    if inner.width < 4 || inner.height < 2 {
        return None;
    }
    put(buf, inner.x as i32, inner.y as i32, "NOTE INFO", bold(ACCENT), inner);
    let lines = match path {
        Some(path) => lines_for(app, path),
        None => {
            let root = app.notebook.info.as_ref().map(|i| i.root.clone());
            let mut out = vec![line(super::EMPTY, fg(TEXT), 0, None)];
            if let Some(root) = root {
                out.push(line("", fg(TEXT), 0, None));
                out.push(line("NOTEBOOK", bold(GRAY), 0, None));
                out.push(line(root, fg(MUTED), 0, None));
            }
            out
        }
    };
    let body = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(2));
    let rows = body.height as usize;
    app.notebook.info_scroll = app.notebook.info_scroll.min(lines.len().saturating_sub(rows));
    for (i, l) in lines.iter().enumerate().skip(app.notebook.info_scroll).take(rows) {
        let y = body.y as i32 + (i - app.notebook.info_scroll) as i32;
        let x = body.x as i32 + l.indent as i32;
        let end = put(buf, x, y, &truncate(&l.text, (body.right() as i32 - x).max(0) as usize), l.style, body);
        if let Some(target) = l.target.clone()
            && end > x
        {
            app.layout.hits.push((Rect::new(x as u16, y as u16, (end - x) as u16, 1), AppHit::Notebook(target)));
        }
    }
    None
}
