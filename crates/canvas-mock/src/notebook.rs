//! The mock's notebook (docs/NOTEBOOK-TASKS.md §2): a demo folder
//! `<state>/notebook` seeded with notes for you and for agents (an HTML
//! note with a script and a picture from the network that must never run or
//! load, a picture the notes show from the folder, a file of the canvas's
//! own), and the backend's notebook requests answered from the files as the
//! service answers them. Missing demo files are written at every start;
//! changed ones stay.
//!
//! Reads go anywhere (like `ReadText`: this is a development tool). Writes
//! (`NoteWrite`, `NoteMkdir`, `NoteRename`, `NoteTrash`) work only inside
//! the demo folder, whatever roots the client names. The canvas's notebook
//! setting is not followed: the notebook is always the demo folder.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use canvas_model::notebook::{self, Entry, Hit, Info, Kind, Meta, NoteText, Place};
use canvas_model::rpc::{Request, Response};

/// Search: folders deep, files looked at, and the largest file read.
const SEARCH_DEPTH: usize = 20;
const SEARCH_FILES: usize = 5_000;
const SEARCH_FILE: u64 = 1 << 20;

const WELCOME: &str = "---\ntitle: Welcome to the notebook\naudience: human\nauthor: human\ncreated: 2026-10-31T09:00:00Z\n---\n# Welcome to the notebook\n\n\
This is the mock service's demo notebook: notes **for you** and notes **for agents**.\n\n\
- The release plan an agent wrote for you: [[Release plan]]\n- The momentum work is tracked as WI-002.\n\n\
- [x] Notes render on the phone\n- [ ] Mark an agent's note reviewed\n\n\
| Folder | For |\n|---|---|\n| `Groups/Payments/ai-docs` | agents |\n| `Groups/Payments/human-outputs` | you |\n\n\
![A chart from this folder](pictures/chart.svg)\n";

const IDEA: &str = "# Idea: offline mode\n\nKeep the last canvas on the phone so it opens without a connection.\n";

const GOALS: &str = "---\naudience: ai\nauthor: ai\nagent: opencode\nsource_session: mock-review\ncreated: 2026-10-30T16:20:00Z\n---\n# Goals\n\n\
- Refunds never exceed the original charge.\n- Every refund is logged with its id.\n";

const LOG: &str = "# Investigation log\n\n- 09:10 The retry test waits on a real `setTimeout`.\n- 09:40 Fake timers fix it (200 runs, 0 failures).\n";

const PLAN: &str = "---\ntitle: Release plan\naudience: human\nauthor: ai\nagent: copilot\nsource_session: mock-fix\ncreated: 2026-10-31T08:15:00Z\n---\n# Release plan\n\n\
1. Merge the flaky-test fix (see [[investigation-log]]).\n2. Ship the refund checks with PR #42 (WI-003).\n3. Watch the error rate for a day.\n\n\
![Error rate](../../../pictures/chart.svg)\n";

/// The demo HTML note. Its script and its network picture are what the
/// phone must never run or fetch.
const DASHBOARD: &str = r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="canvas-audience" content="human">
<meta name="canvas-author" content="ai">
<meta name="canvas-source_session" content="mock-fix">
<title>Payments dashboard</title>
<style>h1 { color: #56b4e9 } .ok { color: #3fb950; font-weight: 600 }</style>
</head>
<body>
<h1 id="title">Payments dashboard</h1>
<p class="ok">Error rate 0.02 %, no drift.</p>
<img src="../../../pictures/chart.svg" alt="Error rate">
<img src="http://127.0.0.1:9/beacon.png" alt="beacon">
<script>document.getElementById('title').textContent = 'SCRIPT RAN';</script>
</body>
</html>
"##;

const INSTRUCTIONS: &str = "---\napplyTo: '**'\n---\n# Canvas group: Payments\n\nYou are working in a session of the \"Payments\" group in Copilot Canvas.\n";

const CHART: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="40" viewBox="0 0 160 40"><polyline points="0,30 30,24 60,28 90,12 120,16 160,6" fill="none" stroke="#56b4e9" stroke-width="3"/></svg>
"##;

/// The demo files (relative to the notebook root).
const DEMO: &[(&str, &str)] = &[
    ("Welcome.md", WELCOME),
    ("Inbox/Idea - offline mode.md", IDEA),
    ("Groups/Payments/ai-docs/GOALS.md", GOALS),
    ("Groups/Payments/ai-docs/investigation-log.md", LOG),
    ("Groups/Payments/human-outputs/Release plan.md", PLAN),
    ("Groups/Payments/human-outputs/Payments dashboard.html", DASHBOARD),
    ("Groups/Payments/.canvas/canvas-group.instructions.md", INSTRUCTIONS),
    ("pictures/chart.svg", CHART),
];

/// Creates the notebook's folders and the demo files that are missing.
pub fn seed(root: &Path) -> Result<()> {
    for folder in [notebook::GROUPS, notebook::INBOX, notebook::TODOS] {
        std::fs::create_dir_all(root.join(folder))?;
    }
    for (rel, text) in DEMO {
        let path = root.join(rel);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().context("a folder")?)?;
            std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
        }
    }
    Ok(())
}

/// Answers a notebook request (`None`: not one). `home`: the mock's state
/// folder; `root`: its notebook.
pub fn answer(home: &Path, root: &Path, request: &Request) -> Option<Response> {
    let result = match request {
        Request::NotebookInfo { canvas, .. } => info(home, root, canvas.as_deref()),
        Request::NotebookList { dir, root: base } => list(home, dir, base.as_deref()),
        Request::NoteRead { path } => read(path).map(Response::Note),
        Request::NoteRevs { paths } => Ok(Response::NoteRevs(paths.iter().map(|p| rev(p)).collect())),
        Request::ReadBytes { path, max_bytes } => read_bytes(path, *max_bytes),
        Request::NoteSearch { roots, query, limit } => search(roots, query, *limit),
        Request::NoteWrite { path, text, base, .. } => write(root, path, text, base.as_deref()),
        Request::NoteMkdir { path, .. } => mkdir(root, path),
        Request::NoteRename { from, to, .. } => rename(root, from, to),
        Request::NoteTrash { path, .. } => trash(root, path),
        _ => return None,
    };
    Some(result.unwrap_or_else(|error| Response::Error(format!("{error:#}"))))
}

fn checked(path: &str) -> Result<PathBuf> {
    notebook::checked_path(path).map_err(|e| anyhow!(e))
}

/// A path the mock may change: inside its notebook.
fn writable(root: &Path, path: &str) -> Result<PathBuf> {
    notebook::writable(path, &[root.display().to_string()]).map_err(|e| anyhow!("{e} (the mock writes only inside its demo notebook, {})", root.display()))
}

fn info(home: &Path, root: &Path, canvas: Option<&str>) -> Result<Response> {
    seed(root)?;
    let canvas_dir = canvas.filter(|id| canvas_model::doc::valid_canvas_id(id)).map(|id| canvas_model::canvas_wide::canvas_dir(home, id).display().to_string());
    Ok(Response::NotebookInfo(Info { root: root.display().to_string(), canvas_dir, home: home.display().to_string() }))
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// One entry, classified as the service does (metadata from the first 8 KiB).
fn entry(path: &Path, rel: &str, place: Place, meta: &std::fs::Metadata) -> Entry {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let common = |kind, title: String, provenance| Entry { name: name.clone(), path: path.display().to_string(), rel: rel.to_owned(), kind, size: if meta.is_dir() { 0 } else { meta.len() }, modified_ms: modified_ms(meta), provenance, title };
    if meta.is_dir() {
        return common(Kind::Folder, name.clone(), notebook::classify(&format!("{rel}/"), &Meta::default(), place));
    }
    let kind = Kind::of(&name);
    let mut head = Vec::new();
    if matches!(kind, Kind::Markdown | Kind::Html) {
        let _ = std::fs::File::open(path).and_then(|f| f.take(notebook::META_BYTES as u64).read_to_end(&mut head));
    }
    let text = String::from_utf8_lossy(&head);
    let note_meta = notebook::note_meta(kind, &text);
    common(kind, notebook::title(kind, &text, &note_meta, &name), notebook::classify(rel, &note_meta, place))
}

/// `Request::NotebookList`: folders first, then files, by name.
fn list(home: &Path, dir: &str, root: Option<&str>) -> Result<Response> {
    let dir = checked(dir)?;
    let root = match root {
        Some(root) => checked(root)?,
        None => dir.clone(),
    };
    let base = notebook::rel_path(&dir, &root).with_context(|| format!("{} is not inside {}", dir.display(), root.display()))?;
    let place = if notebook::inside(&root, &home.join("canvas-agents")) { Place::CanvasWide } else { Place::Notes };
    let mut found: Vec<(bool, String, PathBuf)> =
        std::fs::read_dir(&dir).map_err(|e| anyhow!("cannot list {}: {e}", dir.display()))?.flatten().map(|item| (item.path().is_dir(), item.file_name().to_string_lossy().into_owned(), item.path())).collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    found.truncate(notebook::MAX_ENTRIES);
    let entries = found
        .into_iter()
        .filter_map(|(_, name, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            Some(entry(&path, &if base.is_empty() { name } else { format!("{base}/{name}") }, place, &meta))
        })
        .collect();
    Ok(Response::NoteEntries(entries))
}

fn read(path: &str) -> Result<NoteText> {
    let path = checked(path)?;
    let meta = std::fs::metadata(&path).with_context(|| format!("cannot open {}", path.display()))?;
    ensure!(meta.is_file(), "{} is not a file", path.display());
    ensure!(meta.len() <= notebook::MAX_NOTE, "{} is larger than {} MiB", path.display(), notebook::MAX_NOTE >> 20);
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(NoteText { text: String::from_utf8_lossy(&bytes).into_owned(), rev: notebook::rev_of(&bytes) })
}

fn rev(path: &str) -> Option<String> {
    let path = checked(path).ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    (meta.is_file() && meta.len() <= notebook::MAX_BYTES).then(|| std::fs::read(&path).ok().map(|b| notebook::rev_of(&b))).flatten()
}

fn read_bytes(path: &str, max_bytes: u64) -> Result<Response> {
    let path = checked(path)?;
    let max = max_bytes.min(notebook::MAX_BYTES);
    let meta = std::fs::metadata(&path).with_context(|| format!("cannot open {}", path.display()))?;
    ensure!(meta.is_file(), "{} is not a file", path.display());
    ensure!(meta.len() <= max, "{} is larger than {max} bytes", path.display());
    Ok(Response::Bytes(std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?))
}

/// Case-insensitive search of names and text files (`.trash`, `.git` and
/// `node_modules` skipped).
fn search(roots: &[String], query: &str, limit: u32) -> Result<Response> {
    let query = query.trim().to_lowercase();
    let limit = limit.clamp(1, notebook::MAX_HITS) as usize;
    let mut hits = Vec::new();
    let mut files = 0;
    let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| checked(r).map(|p| (p, 0))).collect::<Result<_>>()?;
    stack.reverse();
    while let Some((dir, depth)) = stack.pop() {
        if query.is_empty() || hits.len() >= limit || files >= SEARCH_FILES {
            break;
        }
        let Ok(items) = std::fs::read_dir(&dir) else { continue };
        let mut items: Vec<(String, PathBuf, std::fs::FileType)> = items.flatten().filter_map(|i| Some((i.file_name().to_string_lossy().into_owned(), i.path(), i.file_type().ok()?))).collect();
        items.sort_by_key(|a| a.0.to_lowercase());
        let mut folders = Vec::new();
        for (name, path, kind) in items {
            if kind.is_dir() {
                if depth < SEARCH_DEPTH && ![notebook::TRASH, ".git", "node_modules"].iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    folders.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() || hits.len() >= limit {
                continue;
            }
            files += 1;
            if name.to_lowercase().contains(&query) {
                hits.push(Hit { path: path.display().to_string(), line: 0, text: name.clone() });
            }
            if !Kind::of(&name).is_text() || !std::fs::metadata(&path).is_ok_and(|m| m.len() <= SEARCH_FILE) {
                continue;
            }
            let text = std::fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            for (n, line) in text.lines().enumerate().filter(|(_, l)| l.to_lowercase().contains(&query)).take(limit.saturating_sub(hits.len())) {
                hits.push(Hit { path: path.display().to_string(), line: n as u32 + 1, text: line.trim().chars().take(200).collect() });
            }
        }
        stack.extend(folders.into_iter().rev());
    }
    hits.truncate(limit);
    Ok(Response::NoteHits(hits))
}

/// `Request::NoteWrite`: only when the note is still at `base` (`None`: new).
fn write(root: &Path, path: &str, text: &str, base: Option<&str>) -> Result<Response> {
    let path = writable(root, path)?;
    ensure!(text.len() as u64 <= notebook::MAX_NOTE, "a note is at most {} MiB", notebook::MAX_NOTE >> 20);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match base {
        None if path.exists() => return Ok(Response::NoteConflict(read(&path.display().to_string())?)),
        None => {}
        Some(base) => {
            ensure!(path.is_file(), "{} was moved or deleted; save it as a new note", path.display());
            let current = read(&path.display().to_string())?;
            if current.rev != base {
                return Ok(Response::NoteConflict(current));
            }
        }
    }
    canvas_common::paths::write_atomic(&path, text.as_bytes()).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Response::NoteSaved { rev: notebook::rev_of(text.as_bytes()) })
}

fn mkdir(root: &Path, path: &str) -> Result<Response> {
    let path = writable(root, path)?;
    ensure!(!path.exists(), "{} already exists", path.display());
    std::fs::create_dir_all(&path)?;
    Ok(Response::Done)
}

fn rename(root: &Path, from: &str, to: &str) -> Result<Response> {
    let (from, to) = (writable(root, from)?, writable(root, to)?);
    ensure!(from.exists(), "{} does not exist", from.display());
    ensure!(!to.exists(), "{} already exists", to.display());
    ensure!(!notebook::inside(&to, &from), "a folder cannot move into itself");
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&from, &to).with_context(|| format!("cannot move {} to {}", from.display(), to.display()))?;
    Ok(Response::Done)
}

/// Into `<notebook>/.trash/<ms>-<name>`.
fn trash(root: &Path, path: &str) -> Result<Response> {
    let path = writable(root, path)?;
    let bin = root.join(notebook::TRASH);
    if notebook::inside(&path, &bin) {
        bail!("{} is already in the trash", path.display());
    }
    ensure!(path.exists(), "{} does not exist", path.display());
    std::fs::create_dir_all(&bin)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ms = canvas_common::paths::now_ms();
    let target = (1..1_000).map(|n| bin.join(if n == 1 { format!("{ms}-{name}") } else { format!("{ms}-{n}-{name}") })).find(|t| !t.exists()).context("no free name in the trash")?;
    std::fs::rename(&path, &target).with_context(|| format!("cannot move {} to the trash", path.display()))?;
    Ok(Response::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::notebook::{Audience, Author};

    /// A fresh state folder for a test.
    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("canvas-mock-notebook-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ask(home: &Path, request: Request) -> Response {
        answer(home, &home.join("notebook"), &request).expect("a notebook request")
    }

    fn text(path: &Path) -> String {
        path.display().to_string()
    }

    fn entries(home: &Path, dir: &Path, root: Option<&Path>) -> Vec<Entry> {
        match ask(home, Request::NotebookList { dir: text(dir), root: root.map(text) }) {
            Response::NoteEntries(entries) => entries,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_demo_notebook_is_listed_read_and_searched_like_the_services() {
        let home = home("read");
        let root = home.join("notebook");
        let Response::NotebookInfo(info) = ask(&home, Request::NotebookInfo { root: None, canvas: Some("c-1".into()) }) else { panic!("info") };
        assert_eq!((info.root.as_str(), info.home.as_str()), (text(&root).as_str(), text(&home).as_str()));
        assert_eq!(info.canvas_dir, Some(text(&home.join("canvas-agents").join("c-1"))));
        assert!(["Groups", "Inbox", "Todos"].iter().all(|f| root.join(f).is_dir()));
        // Folders first, then notes, with titles and provenance.
        let top = entries(&home, &root, None);
        assert_eq!(top.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Groups", "Inbox", "pictures", "Todos", "Welcome.md"]);
        let welcome = &top[4];
        assert_eq!((welcome.title.as_str(), welcome.kind, welcome.provenance.audience, welcome.provenance.inferred), ("Welcome to the notebook", Kind::Markdown, Audience::Human, false));
        let outputs = root.join("Groups").join("Payments").join("human-outputs");
        let found = entries(&home, &outputs, Some(&root));
        let plan = found.iter().find(|e| e.name == "Release plan.md").unwrap();
        assert_eq!((plan.rel.as_str(), plan.provenance.author, plan.provenance.session.as_deref()), ("Groups/Payments/human-outputs/Release plan.md", Author::Ai, Some("mock-fix")));
        let dashboard = found.iter().find(|e| e.kind == Kind::Html).unwrap();
        assert_eq!((dashboard.title.as_str(), dashboard.provenance.audience, dashboard.provenance.inferred), ("Payments dashboard", Audience::Human, false));
        let docs = entries(&home, &root.join("Groups").join("Payments").join("ai-docs"), Some(&root));
        let log = docs.iter().find(|e| e.name == "investigation-log.md").unwrap();
        assert_eq!((log.provenance.audience, log.provenance.author, log.provenance.inferred), (Audience::Ai, Author::Ai, true), "for agents by its folder");
        let own = entries(&home, &root.join("Groups").join("Payments").join(".canvas"), Some(&root));
        assert!(own.iter().all(|e| e.provenance.internal) && own.len() == 1, "{own:?}");

        // Reads, revisions and bytes.
        let plan_path = text(&outputs.join("Release plan.md"));
        let Response::Note(note) = ask(&home, Request::NoteRead { path: plan_path.clone() }) else { panic!("read") };
        assert!(note.text.contains("# Release plan") && note.rev == notebook::rev_of(note.text.as_bytes()));
        let missing = text(&root.join("missing.md"));
        assert_eq!(ask(&home, Request::NoteRevs { paths: vec![plan_path, missing] }), Response::NoteRevs(vec![Some(note.rev), None]));
        let chart = text(&root.join("pictures").join("chart.svg"));
        assert!(matches!(ask(&home, Request::ReadBytes { path: chart.clone(), max_bytes: 1 << 20 }), Response::Bytes(bytes) if bytes.starts_with(b"<svg")));
        assert!(matches!(ask(&home, Request::ReadBytes { path: chart, max_bytes: 10 }), Response::Error(why) if why.contains("larger")));
        assert!(matches!(ask(&home, Request::NoteRead { path: "relative.md".into() }), Response::Error(_)));

        // Search: names and lines, case-insensitive.
        let search = |query: &str| match ask(&home, Request::NoteSearch { roots: vec![text(&root)], query: query.into(), limit: 50 }) {
            Response::NoteHits(hits) => hits,
            other => panic!("{other:?}"),
        };
        assert!(search("REFUNDS NEVER").iter().any(|h| h.path.ends_with("GOALS.md") && h.line == 10 && h.text.starts_with("- Refunds")), "{:?}", search("refunds never"));
        assert!(search("release").iter().any(|h| h.line == 0 && h.text == "Release plan.md"));
        assert!(search("  ").is_empty());
        assert!(answer(&home, &root, &Request::Info).is_none(), "not a notebook request");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn writes_stay_inside_the_demo_notebook_and_never_lose_a_change() {
        let home = home("write");
        let root = home.join("notebook");
        seed(&root).unwrap();
        let write = |path: &Path, body: &str, base: Option<&str>| ask(&home, Request::NoteWrite { path: text(path), text: body.into(), base: base.map(str::to_owned), roots: vec![text(&home)] });
        let note = root.join("Inbox").join("new.md");
        assert_eq!(write(&note, "# New\n", None), Response::NoteSaved { rev: notebook::rev_of(b"# New\n") });
        assert!(matches!(write(&note, "other", None), Response::NoteConflict(current) if current.text == "# New\n"), "a new note never replaces one");
        let rev = notebook::rev_of(b"# New\n");
        assert!(matches!(write(&note, "# New\n\nmore\n", Some(&rev)), Response::NoteSaved { .. }));
        assert!(matches!(write(&note, "lost", Some(&rev)), Response::NoteConflict(current) if current.text.contains("more")), "an old base is a conflict");
        let outside = home.join("outside.md");
        assert!(matches!(write(&outside, "x", None), Response::Error(why) if why.contains("only inside its demo notebook")) && !outside.exists());

        // Folders, renames and the trash.
        let ideas = root.join("Inbox").join("Ideas");
        assert_eq!(ask(&home, Request::NoteMkdir { path: text(&ideas), roots: Vec::new() }), Response::Done);
        assert!(ideas.is_dir() && matches!(ask(&home, Request::NoteMkdir { path: text(&ideas), roots: Vec::new() }), Response::Error(_)));
        let moved = ideas.join("new.md");
        assert_eq!(ask(&home, Request::NoteRename { from: text(&note), to: text(&moved), roots: Vec::new() }), Response::Done);
        assert!(matches!(ask(&home, Request::NoteRename { from: text(&ideas), to: text(&ideas.join("inner")), roots: Vec::new() }), Response::Error(_)), "not into itself");
        assert_eq!(ask(&home, Request::NoteTrash { path: text(&moved), root: text(&root) }), Response::Done);
        assert!(!moved.exists() && std::fs::read_dir(root.join(notebook::TRASH)).unwrap().count() == 1);

        // Seeding again keeps your changes and brings back what is missing.
        std::fs::write(root.join("Welcome.md"), "# Mine\n").unwrap();
        std::fs::remove_file(root.join("pictures").join("chart.svg")).unwrap();
        seed(&root).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("Welcome.md")).unwrap(), "# Mine\n");
        assert!(root.join("pictures").join("chart.svg").is_file());
        let _ = std::fs::remove_dir_all(home);
    }
}
