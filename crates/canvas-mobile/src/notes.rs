//! The notebook on the phone (the desktop's Notebook space; read-only but
//! for *Mark reviewed*): the explorer's roots, folder listings with each
//! note's provenance (for you or for agents, who wrote it), and a note as one
//! sanitized page for the app's locked-down web view. Markdown is rendered
//! with `notebook::markdown_html` (`[[Note]]` and todo ids become `#note:` /
//! `#todo:` links, see [`link`]), HTML notes are sanitized like reports,
//! other text is shown as a block. Pictures a note shows from its own root
//! come inline as `data:` URLs (the web view loads nothing).
//!
//! Paths are the service's machine's (Windows or POSIX): handled as text.

use std::collections::HashMap;
use std::ops::Range;

use canvas_model::doc::Doc;
use canvas_model::notebook::{self, Audience, Author, Entry, Hit, Info, Kind, NoteText, Place, Provenance};
use canvas_model::report_html::{self, notes::ORIGIN};

use crate::model::{NoteAudience, NoteAuthor, NoteEntry, NoteFilter, NoteHit, NoteKind, NoteLink, NoteListing, NotePage, NoteProvenance, NotebookRoot, NotebookRootKind};

/// Pictures inlined into one page at most, the largest one, and all of them together.
pub const MAX_PICTURES: usize = 16;
pub const MAX_PICTURE: u64 = 4 << 20;
pub const MAX_PICTURE_BYTES: u64 = 12 << 20;
/// The largest picture file shown on its own.
pub const MAX_IMAGE: u64 = 8 << 20;

impl From<Audience> for NoteAudience {
    fn from(audience: Audience) -> Self {
        match audience {
            Audience::Human => NoteAudience::Human,
            Audience::Ai => NoteAudience::Ai,
        }
    }
}

impl From<Author> for NoteAuthor {
    fn from(author: Author) -> Self {
        match author {
            Author::Human => NoteAuthor::Human,
            Author::Ai => NoteAuthor::Ai,
            Author::Mixed => NoteAuthor::Mixed,
        }
    }
}

impl From<Kind> for NoteKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Folder => NoteKind::Folder,
            Kind::Markdown => NoteKind::Markdown,
            Kind::Html => NoteKind::Html,
            Kind::Text => NoteKind::Text,
            Kind::Image => NoteKind::Image,
            Kind::Other => NoteKind::Other,
        }
    }
}

/// A path compared as text: `/` separators, no trailing one, lower case.
fn norm(path: &str) -> String {
    path.trim().replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// `path` is `root` or inside it.
pub fn is_inside(path: &str, root: &str) -> bool {
    let (path, root) = (norm(path), norm(root));
    !root.is_empty() && (path == root || path.starts_with(&format!("{root}/")))
}

/// The last part of a path.
pub fn file_name(path: &str) -> &str {
    path.trim().trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default()
}

/// The folder a path is in (empty: none).
fn parent(path: &str) -> &str {
    path.trim().trim_end_matches(['/', '\\']).rsplit_once(['/', '\\']).map_or("", |(folder, _)| folder)
}

/// `path` relative to `root` (inside it), `/` separators (`""`: the root itself).
fn rel(root: &str, path: &str) -> String {
    let offset = root.trim().trim_end_matches(['\\', '/']).len();
    path.trim().get(offset..).unwrap_or_default().trim_start_matches(['\\', '/']).replace('\\', "/")
}

fn push_root(out: &mut Vec<NotebookRoot>, root: NotebookRoot) {
    if !root.path.trim().is_empty() && !out.iter().any(|r| norm(&r.path) == norm(&root.path)) {
        out.push(root);
    }
}

/// The explorer's roots, as the desktop's: the notebook root, each group
/// folder outside it, the canvas-wide folder, the mounts (once each).
pub fn roots(info: &Info, doc: &Doc) -> Vec<NotebookRoot> {
    let mut out = Vec::new();
    let root = |kind, label: String, path: &str, group: &str| NotebookRoot { kind, label, path: path.trim().to_owned(), group: group.to_owned() };
    push_root(&mut out, root(NotebookRootKind::Notebook, "Notebook".into(), &info.root, ""));
    for group in &doc.groups {
        let Some(folder) = group.folder.as_deref().filter(|f| !f.trim().is_empty()) else { continue };
        if !is_inside(folder, &info.root) {
            push_root(&mut out, root(NotebookRootKind::Group, format!("◫ {}", group.name), folder, &group.id));
        }
    }
    if let Some(dir) = &info.canvas_dir {
        push_root(&mut out, root(NotebookRootKind::CanvasWide, "Canvas (canvas-wide)".into(), dir, ""));
    }
    for mount in &doc.notebook_mounts {
        let name = file_name(mount);
        push_root(&mut out, root(NotebookRootKind::Mount, if name.is_empty() { mount.trim().to_owned() } else { name.to_owned() }, mount, ""));
    }
    out
}

/// The root a path is in (the innermost one).
pub fn root_of<'a>(roots: &'a [NotebookRoot], path: &str) -> Option<&'a NotebookRoot> {
    roots.iter().filter(|r| is_inside(path, &r.path)).max_by_key(|r| norm(&r.path).len())
}

/// A provenance with the writing session's box name on this canvas.
pub fn provenance(p: &Provenance, doc: &Doc) -> NoteProvenance {
    let session = p.session.clone().unwrap_or_default();
    NoteProvenance {
        audience: p.audience.into(),
        author: p.author.into(),
        inferred: p.inferred,
        internal_file: p.internal,
        passive: p.passive,
        agent: p.agent.clone().unwrap_or_default(),
        session_name: Some(session.as_str()).filter(|s| !s.is_empty()).and_then(|s| doc.node(s)).map(|n| n.name.clone()).unwrap_or_default(),
        session,
        created: p.created.clone().unwrap_or_default(),
        reviewed: p.reviewed,
    }
}

pub fn entry(e: &Entry, doc: &Doc) -> NoteEntry {
    NoteEntry { name: e.name.clone(), path: e.path.clone(), rel: e.rel.clone(), kind: e.kind.into(), size: e.size, modified_ms: e.modified_ms, title: e.title.clone(), provenance: provenance(&e.provenance, doc) }
}

/// A folder's entries as the phone lists them: the canvas's own files left
/// out unless `show_internal`, notes for the other audience left out by
/// `filter` (folders always stay).
pub fn listing(folder: &str, entries: &[Entry], filter: NoteFilter, show_internal: bool, doc: &Doc) -> NoteListing {
    let mut out = NoteListing { folder: folder.to_owned(), entries: Vec::new(), hidden: 0, internal_files: 0 };
    for e in entries {
        if e.provenance.internal && !show_internal {
            out.internal_files += 1;
            continue;
        }
        let shown = e.is_dir()
            || match filter {
                NoteFilter::All => true,
                NoteFilter::Human => e.provenance.audience == Audience::Human,
                NoteFilter::Ai => e.provenance.audience == Audience::Ai,
            };
        if shown {
            out.entries.push(entry(e, doc));
        } else {
            out.hidden += 1;
        }
    }
    out
}

pub fn hit(hit: Hit) -> NoteHit {
    NoteHit { path: hit.path, line: hit.line, text: hit.text }
}

/// What a link of a note page opens (`None`: another link).
pub fn link(href: &str) -> Option<NoteLink> {
    match notebook::link_target(href)? {
        notebook::LinkTarget::Note(name) => Some(NoteLink::Note { name }),
        notebook::LinkTarget::Todo(id) => Some(NoteLink::Todo { id }),
    }
}

/// The note a `[[Name]]` link names among search hits: a file whose name,
/// with or without its extension, is the name (`Folder/Name`: in such a
/// folder), the shortest path first.
pub fn find_note(name: &str, hits: &[Hit]) -> Option<String> {
    let name = name.split(['#', '|']).next().unwrap_or_default().trim();
    let wanted = file_name(name).to_lowercase();
    let folder = Some(norm(parent(name))).filter(|f| !f.is_empty());
    hits.iter()
        .filter(|h| h.line == 0 && !wanted.is_empty())
        .map(|h| h.path.as_str())
        .filter(|path| {
            let found = file_name(path).to_lowercase();
            let place = norm(parent(path));
            (found == wanted || notebook::stem(&found) == wanted) && folder.as_ref().is_none_or(|f| place == *f || place.ends_with(&format!("/{f}")))
        })
        .min_by_key(|path| (path.len(), path.to_lowercase()))
        .map(str::to_owned)
}

// ---------- pages ----------

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A note's title and provenance from its text (its root decides the folder rules).
pub fn classify(path: &str, text: &str, roots: &[NotebookRoot]) -> (String, Provenance) {
    let name = file_name(path);
    let kind = Kind::of(name);
    let meta = notebook::note_meta(kind, text);
    let root = root_of(roots, path);
    let rel = root.map(|r| rel(&r.path, path)).unwrap_or_else(|| name.to_owned());
    let place = if root.is_some_and(|r| r.kind == NotebookRootKind::CanvasWide) { Place::CanvasWide } else { Place::Notes };
    (notebook::title(kind, text, &meta, name), notebook::classify(&rel, &meta, place))
}

/// What a note shows, before sanitizing: Markdown rendered, HTML as
/// written, other text as a block (pictures: see [`page`]).
pub fn body_html(kind: Kind, name: &str, text: &str) -> String {
    match kind {
        Kind::Markdown => notebook::markdown_html(text),
        Kind::Html => text.to_owned(),
        Kind::Text => format!("<pre>{}</pre>", escape(text)),
        Kind::Image | Kind::Folder | Kind::Other => format!("<p><em>{}</em> cannot be shown on the phone.</p>", escape(name)),
    }
}

/// A note's page: `note` read from the service (a picture: `pictures` holds
/// its `data:` URL under its path), `pictures` the `data:` URLs of the
/// pictures it shows (by `src` as written, [`local_pictures`]).
pub fn page(path: &str, note: &NoteText, roots: &[NotebookRoot], doc: &Doc, pictures: &HashMap<String, String>) -> NotePage {
    let name = file_name(path);
    let kind = Kind::of(name);
    let text = if kind.is_text() { note.text.as_str() } else { "" };
    let (title, found) = classify(path, text, roots);
    let body = match (kind, pictures.get(path)) {
        (Kind::Image, Some(url)) => format!("<img src=\"{url}\" alt=\"{}\">", escape(name)),
        _ => inline_pictures(&body_html(kind, name, text), pictures),
    };
    NotePage { path: path.to_owned(), title, kind: kind.into(), rev: note.rev.clone(), text: text.to_owned(), html: report_html::document(&body, &crate::reports::PHONE), provenance: provenance(&found, doc) }
}

/// The `src` values of `<img>` tags (where each value is, and the value as written).
fn img_sources(html: &str) -> Vec<(Range<usize>, String)> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("<img") {
        let mut i = from + at + 4;
        from = i;
        if !bytes.get(i).is_some_and(|b| b.is_ascii_whitespace() || *b == b'/') {
            continue;
        }
        loop {
            while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] == b'>' {
                break;
            }
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'=' | b'>' | b'/') {
                i += 1;
            }
            let name = &lower[start..i];
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if bytes.get(i) != Some(&b'=') {
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = match bytes.get(i) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let end = html[i + 1..].find(quote as char).map_or(bytes.len(), |e| i + 1 + e);
                    let value = i + 1..end;
                    i = (end + 1).min(bytes.len());
                    value
                }
                _ => {
                    let start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                        i += 1;
                    }
                    start..i
                }
            };
            if name == "src" {
                out.push((value.clone(), html[value].to_owned()));
            }
        }
        from = i.max(from);
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match text.get(i + 1..i + 3).filter(|h| bytes[i] == b'%' && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            Some(hex) => {
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The file a relative `src` of the note at `note` points at (`None`: not a
/// relative path: a web address, a `data:` URL, an absolute path, …).
fn resolve(note: &str, src: &str) -> Option<String> {
    let src = canvas_model::reports::decode_entities(src.trim());
    if !report_html::notes::local_url(&src) || src.starts_with('#') || src.to_ascii_lowercase().starts_with(ORIGIN) {
        return None;
    }
    let target = percent_decode(src.split(['?', '#']).next().unwrap_or_default());
    if target.is_empty() || target.starts_with('/') || target.contains(['\\', ':']) {
        return None;
    }
    let sep = if note.contains('\\') { "\\" } else { "/" };
    let mut parts: Vec<&str> = note.trim().split(['/', '\\']).collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.len() > 1 => {
                parts.pop();
            }
            ".." => return None,
            part => parts.push(part),
        }
    }
    Some(parts.join(sep))
}

/// The pictures a page body shows from the note's root (its folder when the
/// root is not known): (`src` as written, the file's path), at most
/// [`MAX_PICTURES`]. Pictures from the web never load.
pub fn local_pictures(body: &str, note: &str, root: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let within = root.unwrap_or(parent(note));
    for (_, src) in img_sources(body) {
        if out.len() >= MAX_PICTURES || out.iter().any(|(s, _)| *s == src) {
            continue;
        }
        if let Some(path) = resolve(note, &src).filter(|p| is_inside(p, within) && Kind::of(file_name(p)) == Kind::Image) {
            out.push((src, path));
        }
    }
    out
}

/// `body` with the pictures of `pictures` (by `src` as written) as `data:` URLs.
pub fn inline_pictures(body: &str, pictures: &HashMap<String, String>) -> String {
    let mut out = body.to_owned();
    for (range, src) in img_sources(body).into_iter().rev() {
        if let Some(url) = pictures.get(&src) {
            out.replace_range(range, url);
        }
    }
    out
}

/// A picture file as a `data:` URL.
pub fn data_url(name: &str, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", report_html::notes::content_type(name), base64(bytes))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::doc::{Group, Node};
    use canvas_model::notebook::Meta;

    fn doc() -> Doc {
        let mut doc = Doc::default();
        let group = |id: &str, name: &str, folder: Option<&str>| Group { id: id.into(), name: name.into(), x: 0, y: 0, w: 40, h: 20, color: 0, folder: folder.map(str::to_owned), source: None, defaults: None };
        doc.groups.push(group("g-pay", "Payments", Some(r"C:\nb\Groups\Payments")));
        doc.groups.push(group("g-web", "Website", Some(r"D:\src\web")));
        doc.groups.push(group("g-none", "Loose", None));
        doc.notebook_mounts = vec![r"E:\Vault\".into()];
        doc.nodes.push(serde_json::from_value::<Node>(serde_json::json!({ "id": "s-1", "kind": "session", "name": "Fix flaky test", "x": 0, "y": 0 })).unwrap());
        doc
    }

    fn info() -> Info {
        Info { root: r"C:\nb".into(), canvas_dir: Some(r"C:\h\canvas-agents\c1".into()), home: r"C:\h".into() }
    }

    #[test]
    fn roots_are_the_notebook_outside_group_folders_the_canvas_folder_and_mounts() {
        let roots = roots(&info(), &doc());
        let seen: Vec<(NotebookRootKind, &str, &str, &str)> = roots.iter().map(|r| (r.kind, r.label.as_str(), r.path.as_str(), r.group.as_str())).collect();
        assert_eq!(
            seen,
            [
                (NotebookRootKind::Notebook, "Notebook", r"C:\nb", ""),
                (NotebookRootKind::Group, "◫ Website", r"D:\src\web", "g-web"),
                (NotebookRootKind::CanvasWide, "Canvas (canvas-wide)", r"C:\h\canvas-agents\c1", ""),
                (NotebookRootKind::Mount, "Vault", r"E:\Vault\", ""),
            ],
            "a group folder inside the root is not a root of its own"
        );
        assert_eq!(root_of(&roots, r"C:\nb\Groups\Payments\ai-docs\plan.md").map(|r| r.kind), Some(NotebookRootKind::Notebook));
        assert_eq!(root_of(&roots, "c:/H/canvas-agents/c1/GOALS.md").map(|r| r.kind), Some(NotebookRootKind::CanvasWide), "compared as the service's paths");
        assert!(root_of(&roots, r"C:\nbx\a.md").is_none() && root_of(&roots, r"F:\a.md").is_none());
        assert_eq!(rel(r"C:\nb\", r"C:\nb\Groups\a.md"), "Groups/a.md");
    }

    fn entry(rel: &str, kind: Kind, meta: &[(&str, &str)]) -> Entry {
        let meta = Meta(meta.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect());
        let classified = if kind == Kind::Folder { format!("{rel}/") } else { rel.to_owned() };
        Entry { name: file_name(rel).into(), path: format!(r"C:\nb\{}", rel.replace('/', "\\")), rel: rel.into(), kind, title: notebook::stem(file_name(rel)).into(), provenance: notebook::classify(&classified, &meta, Place::Notes), ..Default::default() }
    }

    #[test]
    fn listings_carry_provenance_and_filter_by_audience() {
        let entries = [
            entry("Groups/Payments/ai-docs", Kind::Folder, &[]),
            entry("Groups/Payments/Brief.html", Kind::Html, &[]),
            entry("Groups/Payments/events.yaml", Kind::Text, &[]),
            entry("Groups/Payments/Notes.md", Kind::Markdown, &[]),
            entry("Groups/Payments/Plan.md", Kind::Markdown, &[("audience", "ai"), ("author", "ai"), ("agent", "opencode"), ("source_session", "s-1")]),
        ];
        let doc = doc();
        let names = |listing: &NoteListing| listing.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let all = listing(r"C:\nb\Groups\Payments", &entries, NoteFilter::All, false, &doc);
        assert_eq!((names(&all), all.hidden, all.internal_files), (vec!["ai-docs".to_owned(), "Brief.html".into(), "Notes.md".into(), "Plan.md".into()], 0, 1), "the canvas's own files left out");
        let plan = &all.entries[3].provenance;
        assert_eq!((plan.audience, plan.author, plan.inferred, plan.agent.as_str(), plan.session.as_str(), plan.session_name.as_str()), (NoteAudience::Ai, NoteAuthor::Ai, false, "opencode", "s-1", "Fix flaky test"));
        let brief = &all.entries[1];
        assert_eq!((brief.kind, brief.provenance.audience, brief.provenance.inferred, brief.provenance.session_name.as_str()), (NoteKind::Html, NoteAudience::Human, true, ""));
        let human = listing("x", &entries, NoteFilter::Human, false, &doc);
        assert_eq!((names(&human), human.hidden), (vec!["ai-docs".to_owned(), "Brief.html".into(), "Notes.md".into()], 1), "folders always stay");
        let ai = listing("x", &entries, NoteFilter::Ai, true, &doc);
        assert_eq!((names(&ai), ai.hidden, ai.internal_files), (vec!["ai-docs".to_owned(), "Plan.md".into()], 3, 0));
        assert!(listing("x", &entries, NoteFilter::All, true, &doc).entries.iter().any(|e| e.name == "events.yaml" && e.provenance.internal_file));
    }

    #[test]
    fn notes_become_one_sanitized_page_with_their_own_pictures_inline() {
        let (roots, doc) = (roots(&info(), &doc()), doc());
        let path = r"C:\nb\Groups\Payments\human-outputs\Release plan.md";
        let text = "---\ntitle: Release plan\naudience: human\nauthor: ai\nsource_session: s-1\n---\n# Plan\n\nSee [[Welcome]] and WI-002.\n\n\
                    ![Chart](../../../pictures/chart.svg) ![Remote](https://example.com/x.png) ![Outside](../../../../secret.png)\n\n<script>alert(1)</script>\n";
        let body = body_html(Kind::Markdown, "Release plan.md", text);
        let wanted = local_pictures(&body, path, root_of(&roots, path).map(|r| r.path.as_str()));
        assert_eq!(wanted, [("../../../pictures/chart.svg".to_owned(), r"C:\nb\pictures\chart.svg".to_owned())], "only pictures of its root, never the web");
        let pictures: HashMap<String, String> = [(wanted[0].0.clone(), data_url("chart.svg", b"<svg/>"))].into();
        let note = NoteText { text: text.into(), rev: "0123456789abcdef".into() };
        let page = page(path, &note, &roots, &doc, &pictures);
        assert_eq!((page.title.as_str(), page.kind, page.rev.as_str(), page.text.as_str()), ("Release plan", NoteKind::Markdown, "0123456789abcdef", text));
        let p = &page.provenance;
        assert_eq!((p.audience, p.author, p.inferred, p.session_name.as_str()), (NoteAudience::Human, NoteAuthor::Ai, false, "Fix flaky test"));
        let html = &page.html;
        assert!(html.starts_with("<!DOCTYPE html>") && html.contains("script-src 'none'") && html.contains("font:-apple-system-body"), "{html}");
        assert!(html.contains("<h1>Plan</h1>") && html.contains("<a href=\"#note:Welcome\">Welcome</a>") && html.contains("<a href=\"#todo:WI-002\">WI-002</a>"), "{html}");
        assert!(html.contains("src=\"data:image/svg+xml;base64,PHN2Zy8+\""), "{html}");
        for gone in ["<script", "alert", "example.com", "secret.png", "source_session"] {
            assert!(!html.contains(gone), "{gone} survived:\n{html}");
        }

        // An HTML note keeps its own styles; scripts, handlers and the web go.
        let roots = [NotebookRoot { kind: NotebookRootKind::Notebook, label: "Notebook".into(), path: "/Users/me/nb".into(), group: String::new() }];
        let path = "/Users/me/nb/Reports/dash.html";
        let text = "<html><head><meta name=\"canvas-audience\" content=\"ai\"><title>Dash</title><style>h1{color:red}</style></head>\
                    <body><h1 onclick=\"x()\">Dash</h1><IMG SRC='shots/a%20b.png'><img src=\"https://evil/x.png\"><script>alert(1)</script></body></html>";
        let wanted = local_pictures(&body_html(Kind::Html, "dash.html", text), path, Some("/Users/me/nb"));
        assert_eq!(wanted, [("shots/a%20b.png".to_owned(), "/Users/me/nb/Reports/shots/a b.png".to_owned())]);
        let pictures: HashMap<String, String> = [(wanted[0].0.clone(), data_url("a b.png", &[0x89, b'P', b'N', b'G']))].into();
        let page = super::page(path, &NoteText { text: text.into(), rev: "r".into() }, &roots, &doc, &pictures);
        assert_eq!((page.title.as_str(), page.kind, page.provenance.audience, page.provenance.inferred), ("Dash", NoteKind::Html, NoteAudience::Ai, false));
        assert!(page.html.contains("<style>h1{color:red}</style>") && page.html.contains("<h1>Dash</h1>") && page.html.contains("src=\"data:image/png;base64,iVBORw==\""), "{}", page.html);
        assert!(!page.html.contains("onclick") && !page.html.contains("evil") && !page.html.contains("alert"), "{}", page.html);

        // Text as a block, a picture on its own, and what the phone cannot show.
        let text = super::page("/Users/me/nb/a.txt", &NoteText { text: "a < b & c".into(), rev: "r".into() }, &roots, &doc, &HashMap::new());
        assert!(text.html.contains("<pre>a &lt; b &amp; c</pre>") && text.kind == NoteKind::Text, "{}", text.html);
        let url = data_url("pic.png", &[1, 2, 3]);
        let picture = super::page("/Users/me/nb/pic.png", &NoteText { text: String::new(), rev: "r".into() }, &roots, &doc, &[("/Users/me/nb/pic.png".to_owned(), url)].into());
        assert!(picture.html.contains("<img src=\"data:image/png;base64,AQID\" alt=\"pic.png\">") && picture.text.is_empty() && picture.title == "pic", "{}", picture.html);
        let other = super::page("/Users/me/nb/data.bin", &NoteText::default(), &roots, &doc, &HashMap::new());
        assert!(other.html.contains("cannot be shown on the phone") && other.kind == NoteKind::Other);
    }

    #[test]
    fn only_relative_pictures_inside_the_root_are_read() {
        assert_eq!(resolve(r"C:\nb\a.md", "img/x.png").as_deref(), Some(r"C:\nb\img\x.png"));
        assert_eq!(resolve("/nb/sub/a.md", "./../img/a%20b.png?v=1#top").as_deref(), Some("/nb/img/a b.png"));
        assert_eq!(resolve("/nb/a.md", "a&amp;b.png").as_deref(), Some("/nb/a&b.png"), "attribute values are decoded");
        for src in ["data:image/png;base64,AA", "https://x/y.png", "//host/y.png", "/etc/y.png", r"..\y.png", "#top", "C:/y.png", "../../../../y.png", "", "%2Fetc%2Fy.png"] {
            assert_eq!(resolve("/nb/a.md", src), None, "{src}");
        }
        let body = "<img alt=\"src=no.png\" src=\"b.png\"><imgx src=\"c.png\"><img src=b.png><img\nsrc = 'doc.pdf'><img src=\"../out.png\"><img src=\"d.png\" />";
        let wanted = local_pictures(body, "/nb/sub/a.md", Some("/nb/sub"));
        assert_eq!(wanted, [("b.png".to_owned(), "/nb/sub/b.png".to_owned()), ("d.png".to_owned(), "/nb/sub/d.png".to_owned())], "once each, pictures only, inside the root");
        assert_eq!(local_pictures("<img src=\"../x.png\">", "/nb/sub/a.md", None), [], "without a root: the note's folder");
        let pictures: HashMap<String, String> = [("b.png".to_owned(), "data:image/png;base64,AA".to_owned())].into();
        assert_eq!(inline_pictures(body, &pictures), body.replace("\"b.png\"", "\"data:image/png;base64,AA\"").replace("src=b.png", "src=data:image/png;base64,AA"));
        assert_eq!(img_sources("<img").len() + img_sources("<img src=").len() + img_sources("<img src=\"x").len(), 2, "cut tags end safely");
    }

    #[test]
    fn links_open_notes_and_todos_and_names_find_their_note() {
        assert_eq!(link("#note:Release%20plan"), Some(NoteLink::Note { name: "Release plan".into() }));
        assert_eq!(link("#todo:DEC-031"), Some(NoteLink::Todo { id: "DEC-031".into() }));
        assert_eq!(link("https://example.com"), None);
        let hit = |path: &str, line: u32| Hit { path: path.into(), line, text: String::new() };
        let hits = [hit(r"C:\nb\Groups\Payments\human-outputs\Release plan.md", 0), hit(r"C:\nb\Release plan.md", 0), hit(r"C:\nb\Release plan old.md", 0), hit(r"C:\nb\x.md", 3)];
        assert_eq!(find_note("Release plan", &hits).as_deref(), Some(r"C:\nb\Release plan.md"), "the shortest path");
        assert_eq!(find_note("human-outputs/release plan#Steps", &hits).as_deref(), Some(r"C:\nb\Groups\Payments\human-outputs\Release plan.md"));
        assert_eq!(find_note("Release plan.md|the plan", &hits).as_deref(), Some(r"C:\nb\Release plan.md"));
        assert_eq!(find_note("outputs/Release plan", &hits), None, "a folder name matches whole");
        assert_eq!(find_note("x", &hits), None, "text hits do not count");
        assert_eq!([base64(b""), base64(b"f"), base64(b"fo"), base64(b"foo"), base64(b"foob")], ["", "Zg==", "Zm8=", "Zm9v", "Zm9vYg=="]);
    }
}
