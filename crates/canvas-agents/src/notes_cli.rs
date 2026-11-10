//! `atelier note …`: agents write notes into the notebook with the
//! right provenance (docs/NOTEBOOK-TASKS.md §2.6).
//!
//! `note path` says where to write, `note new` writes a note (never over
//! another) and `note list` lists the caller's group's notes. The caller is
//! the canvas session in `ATELIER_SESSION` (its agent in
//! `ATELIER_SESSION_AGENT`, else as the routing names it); its group,
//! the group's folder and the notebook root come from the routing the
//! windows send (`<home>/bus/routing.json`). Notes for agents go to the
//! group's `ai-docs`, notes for you to its `human-outputs`; outside a group
//! (or with a folder that is no absolute path), to the notebook's `Inbox`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use canvas_model::notebook::{self, Audience, Author, Kind, NewNote, Place};

/// The `canvas-notes` skill shipped in the canvas plugin.
pub const SKILL: &str = "---
name: canvas-notes
description: Write notes into the Atelier notebook with the right provenance - plans, research, investigation logs and context for other agents (the group's ai-docs), or deliverables for the user such as reports, summaries and final documents (human-outputs) - and find the notes other sessions of your group left. Use when you write a plan, research, an investigation log, a decision or a hand-off worth keeping, when the user asks for a document, write-up or summary to read later, when you need to know where notes belong, and before substantial work in a canvas group (read its ai-docs, GOALS.md first).
---

# The Atelier notebook

The canvas keeps a notebook of Markdown and HTML notes that the user reads and
edits inside the canvas. Every note says whom it is for (`audience`: `ai` or
`human`) and who wrote it (`author`); the user sees notes for them first and
notes for agents dimmed.

## Where notes go

- `atelier note path` prints the folder for notes for other agents: your
  group's `ai-docs`. `atelier note path --audience human` prints the
  folder for the user: your group's `human-outputs`. A session in no group gets
  the notebook's `Inbox`.
- For agents (`audience: ai`): plans, research, investigation logs, decisions and
  the context the next session needs.
- For the user (`audience: human`): reports, summaries, final documents, anything
  the user asked to read. Prefer one self-contained HTML file when layout matters
  (tables, colour, side-by-side comparisons); Markdown otherwise.

## Write a note

`atelier note new --title \"Investigation: flaky upload test\" --md notes.md`

- It adds the provenance (`audience`, `author: ai`, `agent`, `source_session`,
  `created`), writes the note into the right folder and prints its path. It never
  overwrites a note: a taken name gets `-2`, `-3`.
- `--md FILE|-` or `--html FILE|-` (`-` reads standard input); `--audience ai|human`
  (default ai); `--folder REL` (a subfolder); `--name FILE` (default: from the title).
- To change a note later, edit the file at that path and keep its front matter.
- A Markdown note you write yourself starts with front matter:

  ```
  ---
  title: \"Investigation: flaky upload test\"
  audience: ai
  author: ai
  source_session: <the value of ATELIER_SESSION>
  ---
  ```

  HTML notes carry the same keys as meta tags:
  `<meta name=\"canvas-audience\" content=\"human\">`.
- `atelier note list [--audience ai|human] [--format json]` lists your group's notes.

## Rules

- Before substantial work, read your group's `ai-docs` (`GOALS.md` first): other
  sessions leave their context there (`atelier note list --audience ai`).
- Never put secrets, tokens, passwords or credentials in a note.
- HTML notes are self-contained and work offline: inline CSS, pictures as `data:`
  URIs or files next to the note; no scripts, CDNs, web fonts or remote images.
- Link notes with `[[Note name]]` and todos by their id (`DEC-031`); the canvas
  turns them into links.
- Something the user should notice now goes to the Reports inbox (the
  canvas-reports skill); reports from a session in a group with a folder are also
  filed in that group's `human-outputs/reports`.
- In a VM session `atelier note path` names the group's notes as kept on the VM:
  what you write there goes back to the canvas's machine. A note that answers
  *Queued* is filed when the canvas is reachable again.
";

const USAGE: &str = "usage: atelier note path [--audience ai|human]
       atelier note new --title TITLE (--md FILE|- | --html FILE|-) [--audience ai|human] [--folder REL] [--name FILE]
       atelier note list [--audience ai|human] [--internal] [--format json]";

/// Notes listed at most.
const MAX_LIST: usize = 1_000;

/// Who runs the command: the canvas session and its agent (from the environment).
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub session: String,
    /// The session's agent from its marker (`ATELIER_SESSION_AGENT`;
    /// empty: as the routing names it). `ATELIER_AGENT` alone may be
    /// your choice of agent for new sessions, not the writer.
    pub agent: String,
}

impl Caller {
    pub fn from_env() -> Self {
        let session = canvas_common::env::var("ATELIER_SESSION").map(|v| v.trim().to_owned()).unwrap_or_default();
        Caller { session, agent: crate::agents::session_agent().map(|kind| kind.id().to_owned()).unwrap_or_default() }
    }
}

/// Where the caller's notes live.
#[derive(Clone, Debug, PartialEq)]
pub struct NoteDirs {
    /// The caller's group (id, name) when it has a folder.
    pub group: Option<(String, String)>,
    /// That group's folder.
    pub folder: Option<PathBuf>,
    /// The notebook root.
    pub root: PathBuf,
    /// The caller's agent as the routing names it.
    pub agent: Option<String>,
}

impl NoteDirs {
    /// The caller's group and folder from the routed canvas (a folder or a
    /// notebook root that is no absolute path counts for nothing).
    pub fn of(home: &Path, caller: &Caller) -> Self {
        let routing = crate::cli::routing(home);
        let absolute = |path: &str| notebook::checked_path(path.trim()).ok();
        let root = routing.notebook_root.as_deref().and_then(absolute).unwrap_or_else(|| notebook::root_of(None, home));
        let agent = (!caller.session.is_empty()).then(|| routing.agent_of(&caller.session)).flatten();
        let group = (!caller.session.is_empty()).then(|| routing.group_of(&caller.session)).flatten().and_then(|id| routing.group(&id).cloned());
        match group.and_then(|g| g.folder.as_deref().and_then(absolute).map(|f| (g, f))) {
            Some((group, folder)) => NoteDirs { group: Some((group.id, group.name)), folder: Some(folder), root, agent },
            None => NoteDirs { group: None, folder: None, root, agent },
        }
    }

    /// The folder for notes of this audience.
    pub fn dir(&self, audience: Audience) -> PathBuf {
        match (&self.folder, audience) {
            (Some(folder), Audience::Ai) => folder.join(canvas_model::doc::AI_DOCS),
            (Some(folder), Audience::Human) => folder.join(canvas_model::doc::HUMAN_OUTPUTS),
            (None, _) => self.root.join(notebook::INBOX),
        }
    }
}

/// Runs `atelier note WORDS…`: `options` are the `--name value` pairs,
/// `flags` the boolean flags (without `--`).
pub fn run(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String]) -> Result<()> {
    let out = execute(home, words, options, flags, &Caller::from_env(), &mut std::io::stdin().lock())?;
    println!("{out}");
    Ok(())
}

fn option<'a>(options: &'a [(String, String)], name: &str) -> Option<&'a str> {
    options.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Refuses options and flags the command does not take.
fn check_args(command: &str, options: &[(String, String)], flags: &[String], allowed: &[&str], allowed_flags: &[&str]) -> Result<()> {
    if let Some((name, _)) = options.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        bail!("note {command} does not take --{name}\n{USAGE}");
    }
    if let Some(flag) = flags.iter().find(|f| !allowed_flags.contains(&f.as_str())) {
        bail!("note {command} does not take --{flag}\n{USAGE}");
    }
    Ok(())
}

fn audience_option(options: &[(String, String)]) -> Result<Option<Audience>> {
    option(options, "audience").map(|a| Audience::parse(a).with_context(|| format!("--audience is ai or human, not {a:?}"))).transpose()
}

/// The command's output (printed by [`run`]).
pub fn execute(home: &Path, words: &[&str], options: &[(String, String)], flags: &[String], caller: &Caller, stdin: &mut dyn Read) -> Result<String> {
    match words {
        ["path"] => {
            check_args("path", options, flags, &["audience"], &[])?;
            let dir = NoteDirs::of(home, caller).dir(audience_option(options)?.unwrap_or(Audience::Ai));
            std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
            Ok(dir.display().to_string())
        }
        ["new"] => {
            check_args("new", options, flags, &["title", "md", "html", "audience", "folder", "name"], &[])?;
            Ok(new_note(home, options, caller, stdin)?.display().to_string())
        }
        ["list"] => {
            check_args("list", options, flags, &["audience", "format"], &["internal"])?;
            let json = match option(options, "format") {
                None | Some("text") => false,
                Some("json") => true,
                Some(other) => bail!("--format is json or text, not {other:?}"),
            };
            list(home, caller, audience_option(options)?, flags.iter().any(|f| f == "internal"), json)
        }
        _ => bail!("{USAGE}"),
    }
}

/// A note's body: a file, or `-` for standard input (UTF-8, at most 4 MiB).
fn read_body(source: &str, stdin: &mut dyn Read) -> Result<String> {
    let max = notebook::MAX_NOTE;
    let mut bytes = Vec::new();
    if source == "-" {
        (&mut *stdin).take(max + 1).read_to_end(&mut bytes).context("cannot read the note from standard input")?;
    } else {
        std::fs::File::open(source).and_then(|f| f.take(max + 1).read_to_end(&mut bytes)).with_context(|| format!("cannot read {source}"))?;
    }
    anyhow::ensure!(bytes.len() as u64 <= max, "a note is at most {} MiB", max >> 20);
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", if source == "-" { "standard input" } else { source }))?;
    Ok(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

/// A name `--folder` or `--name` may give on any machine: no `.` first, no
/// character or form Windows refuses or reads as another name (`<>:"/\|?*`,
/// control characters, a trailing dot or space, `CON`, `nul.md`).
fn plain_name(part: &str) -> bool {
    !part.is_empty() && !part.starts_with('.') && part == part.trim_start() && !part.ends_with(['.', ' ']) && !notebook::is_device_name(part) && !part.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
}

/// A subfolder given as `--folder`: relative, plain names, no `..`, nothing hidden.
fn subfolder(rel: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = rel.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if rel.starts_with(['/', '\\']) || !parts.iter().all(|p| plain_name(p)) {
        bail!("--folder takes a relative folder of plain names (for example research/2026), not {rel:?}");
    }
    Ok(parts.iter().collect())
}

/// `--name`: a plain file name; the extension must fit the format (added when missing).
fn file_name(name: &str, kind: Kind) -> Result<(String, String)> {
    let name = name.trim();
    let ext = if kind == Kind::Html { "html" } else { "md" };
    if !plain_name(name) {
        bail!("--name takes a plain file name (for example plan.md), not {name:?}");
    }
    match name.rsplit_once('.') {
        Some((stem, given)) if Kind::of(name) == kind && !stem.is_empty() => Ok((stem.to_owned(), given.to_owned())),
        Some((_, given)) if matches!(Kind::of(name), Kind::Markdown | Kind::Html) => bail!("--name {name:?} ends in .{given}, but the note is {}", if kind == Kind::Html { "HTML (.html)" } else { "Markdown (.md)" }),
        _ => Ok((name.to_owned(), ext.to_owned())),
    }
}

fn new_note(home: &Path, options: &[(String, String)], caller: &Caller, stdin: &mut dyn Read) -> Result<PathBuf> {
    let title = option(options, "title").map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
    if title.is_empty() {
        bail!("give the note a --title\n{USAGE}");
    }
    let (kind, body) = match (option(options, "md"), option(options, "html")) {
        (Some(md), None) => (Kind::Markdown, read_body(md, stdin)?),
        (None, Some(html)) => (Kind::Html, read_body(html, stdin)?),
        _ => bail!("give exactly one of --md FILE or --html FILE (FILE may be - for standard input)\n{USAGE}"),
    };
    // The audience: --audience, else the body's own, else for agents.
    let audience = match audience_option(options)? {
        Some(audience) => audience,
        None => notebook::note_meta(kind, &body).get("audience").and_then(Audience::parse).unwrap_or(Audience::Ai),
    };
    let folder = option(options, "folder").map(subfolder).transpose()?.unwrap_or_default();
    let (stem, ext) = match option(options, "name") {
        Some(name) => file_name(name, kind)?,
        None => (notebook::safe_name(&title, "note"), if kind == Kind::Html { "html".to_owned() } else { "md".to_owned() }),
    };
    let dirs = NoteDirs::of(home, caller);
    let note = NewNote {
        title,
        audience,
        author: Author::Ai,
        agent: Some(caller.agent.clone()).filter(|a| !a.is_empty()).or_else(|| dirs.agent.clone()),
        session: Some(caller.session.clone()).filter(|s| !s.is_empty()),
        created: crate::paths::rfc3339_now(),
        extra: Vec::new(),
    };
    let text = notebook::new_note_text(kind, &body, &note);
    crate::notebook::create_new(&dirs.dir(audience).join(folder), &stem, &ext, text.as_bytes())
}

/// Notes (Markdown, HTML, text, pictures) under `dir`, classified relative
/// to `root` (links to folders are not followed; at most 50,000 names a
/// folder); internal files (dot files, `.canvas`, the trash) only with
/// `internal`, and never what is in `.git`.
fn collect(dir: &Path, root: &Path, depth: usize, internal: bool, out: &mut Vec<notebook::Entry>) {
    if depth > 8 || out.len() >= MAX_LIST {
        return;
    }
    let Ok(items) = std::fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = items.flatten().take(50_000).map(|i| i.path()).collect();
    items.sort();
    for path in items {
        if out.len() >= MAX_LIST {
            return;
        }
        let (Ok(meta), Some(rel)) = (std::fs::metadata(&path), notebook::rel_path(&path, root)) else { continue };
        let entry = crate::notebook::entry(&path, &rel, Place::Notes, &meta);
        if entry.provenance.internal && (!internal || path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(".git"))) {
            continue;
        }
        if meta.is_dir() {
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                collect(&path, root, depth + 1, internal, out);
            }
        } else if entry.kind != Kind::Other {
            out.push(entry);
        }
    }
}

/// `note list`: the group's `ai-docs` and `human-outputs` (no group: the
/// notebook's Inbox), newest first; `internal`: internal files too.
fn list(home: &Path, caller: &Caller, audience: Option<Audience>, internal: bool, json: bool) -> Result<String> {
    let place = NoteDirs::of(home, caller);
    let (root, dirs) = match &place.folder {
        Some(folder) => (folder.clone(), vec![place.dir(Audience::Ai), place.dir(Audience::Human)]),
        None => (place.root.clone(), vec![place.dir(Audience::Ai)]),
    };
    let mut notes = Vec::new();
    for dir in &dirs {
        collect(dir, &root, 0, internal, &mut notes);
    }
    notes.retain(|n| audience.is_none_or(|a| n.provenance.audience == a));
    notes.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then_with(|| a.rel.cmp(&b.rel)));
    if json {
        let notes: Vec<serde_json::Value> = notes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "path": n.path, "rel": n.rel, "title": n.title, "kind": n.kind, "audience": n.provenance.audience, "author": n.provenance.author,
                    "inferred": n.provenance.inferred, "agent": n.provenance.agent, "source_session": n.provenance.session, "created": n.provenance.created,
                    "modified": canvas_common::paths::format_rfc3339_ms(n.modified_ms), "size": n.size, "internal": n.provenance.internal,
                })
            })
            .collect();
        let group = place.group.as_ref().map(|(id, name)| serde_json::json!({ "id": id, "name": name }));
        let value = serde_json::json!({ "schemaVersion": 1, "kind": "note-list", "group": group, "folders": dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(), "notes": notes });
        return Ok(serde_json::to_string_pretty(&value)?);
    }
    let mut out = vec![match &place.group {
        Some((_, name)) => format!("Notes of the group \"{name}\" ({}): {}", root.display(), notes.len()),
        None => format!("Notes in the notebook's Inbox ({}; this session is in no group with a folder): {}", dirs[0].display(), notes.len()),
    }];
    let now = crate::paths::now_ms();
    for n in &notes {
        let by = n.provenance.agent.as_deref().map(|a| format!(", by {a}")).unwrap_or_default();
        let internal = if n.provenance.internal { ", internal" } else { "" };
        out.push(format!("  {} {:<5} {:>5} ago  {}  ({}{by}{internal})", n.provenance.audience.glyph(), n.provenance.audience.id(), crate::paths::format_age(n.modified_ms, now), n.title, n.path));
    }
    if notes.is_empty() {
        out.push("  (none yet: atelier note new --title T --md FILE)".into());
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_model::bus::{RouteGroup, Routing};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-notes-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("bus")).unwrap();
        dir
    }

    /// A routed canvas: session `s1` in group "Team" (with a folder), `s2` in a group without one.
    fn route(home: &Path, notebook_root: Option<&Path>) -> PathBuf {
        let folder = home.join("work").join("team");
        let routing = Routing {
            groups: vec![RouteGroup { id: "g1".into(), name: "Team".into(), folder: Some(folder.display().to_string()) }, RouteGroup { id: "g2".into(), name: "Loose".into(), folder: None }],
            members: [("s1".to_owned(), "g1".to_owned()), ("s2".to_owned(), "g2".to_owned())].into(),
            notebook_root: notebook_root.map(|r| r.display().to_string()),
            ..Default::default()
        };
        std::fs::write(home.join("bus").join("routing.json"), serde_json::to_string(&routing).unwrap()).unwrap();
        folder
    }

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn run_as(home: &Path, caller: &Caller, words: &[&str], pairs: &[(&str, &str)], stdin: &str) -> Result<String> {
        execute(home, words, &opts(pairs), &[], caller, &mut stdin.as_bytes())
    }

    fn session(id: &str) -> Caller {
        Caller { session: id.into(), agent: "opencode".into() }
    }

    #[test]
    fn paths_follow_the_callers_group_else_the_inbox() {
        let home = temp("path");
        let folder = route(&home, None);
        assert_eq!(run_as(&home, &session("s1"), &["path"], &[], "").unwrap(), folder.join("ai-docs").display().to_string());
        assert_eq!(run_as(&home, &session("s1"), &["path"], &[("audience", "human")], "").unwrap(), folder.join("human-outputs").display().to_string());
        assert!(folder.join("human-outputs").is_dir(), "created");
        let inbox = home.join("notebook").join("Inbox");
        for caller in [session("s2"), session("stranger"), Caller::default()] {
            assert_eq!(run_as(&home, &caller, &["path"], &[("audience", "human")], "").unwrap(), inbox.display().to_string(), "{caller:?}");
        }
        let custom = home.join("My Notes");
        route(&home, Some(&custom));
        assert_eq!(run_as(&home, &Caller::default(), &["path"], &[], "").unwrap(), custom.join("Inbox").display().to_string(), "the canvas's notebook folder");
        assert!(run_as(&home, &session("s1"), &["path"], &[("audience", "robots")], "").unwrap_err().to_string().contains("--audience is ai or human"));
        assert!(run_as(&home, &session("s1"), &["path"], &[("format", "json")], "").unwrap_err().to_string().contains("does not take --format"));
        assert!(execute(&home, &["path"], &[], &["mine".into()], &session("s1"), &mut "".as_bytes()).unwrap_err().to_string().contains("does not take --mine"));
        assert!(run_as(&home, &session("s1"), &["nope"], &[], "").unwrap_err().to_string().starts_with("usage: atelier note path"));
        assert!(run_as(&home, &session("s1"), &[], &[], "").is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn new_notes_get_provenance_a_free_name_and_the_right_folder() {
        let home = temp("new");
        let folder = route(&home, None);
        let me = session("s1");
        let path = PathBuf::from(run_as(&home, &me, &["new"], &[("title", "Plan: phase 2"), ("md", "-")], "Steps.\n").unwrap());
        assert_eq!(path, folder.join("ai-docs").join("Plan- phase 2.md"));
        let text = std::fs::read_to_string(&path).unwrap();
        let meta = notebook::markdown_meta(&text);
        assert_eq!((meta.get("title"), meta.get("audience"), meta.get("author"), meta.get("agent"), meta.get("source_session")), (Some("Plan: phase 2"), Some("ai"), Some("ai"), Some("opencode"), Some("s1")));
        assert!(meta.get("created").is_some_and(|c| c.ends_with('Z')) && text.ends_with("# Plan: phase 2\n\nSteps.\n"), "{text}");
        // Never overwritten: the next one gets -2.
        let again = run_as(&home, &me, &["new"], &[("title", "Plan: phase 2"), ("md", "-")], "Other.\n").unwrap();
        assert!(again.ends_with("Plan- phase 2-2.md"), "{again}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        // For the user, from a file, in a subfolder with a given name; HTML gets meta tags.
        let page = home.join("page.html");
        std::fs::write(&page, "\u{feff}<html><head><title>Board</title></head><body>x</body></html>").unwrap();
        let html = PathBuf::from(run_as(&home, &me, &["new"], &[("title", "Board"), ("html", &page.display().to_string()), ("audience", "human"), ("folder", "boards/2026"), ("name", "board")], "").unwrap());
        assert_eq!(html, folder.join("human-outputs").join("boards").join("2026").join("board.html"));
        let m = notebook::html_meta(&std::fs::read_to_string(&html).unwrap());
        assert_eq!((m.get("audience"), m.get("author"), m.get("source_session")), (Some("human"), Some("ai"), Some("s1")));
        // The body's own audience counts when --audience is not given.
        let own = run_as(&home, &me, &["new"], &[("title", "For you"), ("md", "-")], "---\naudience: human\n---\nHi\n").unwrap();
        assert!(own.starts_with(&folder.join("human-outputs").display().to_string()), "{own}");
        // No group: the Inbox of the notebook; no session: no source_session.
        let loose = PathBuf::from(run_as(&home, &Caller::default(), &["new"], &[("title", "Loose idea"), ("md", "-"), ("name", "idea.md")], "x").unwrap());
        assert_eq!(loose, home.join("notebook").join("Inbox").join("idea.md"));
        let loose_meta = notebook::markdown_meta(&std::fs::read_to_string(&loose).unwrap());
        assert_eq!((loose_meta.get("source_session"), loose_meta.get("agent")), (None, None));
        // Refused.
        let fail = |pairs: &[(&str, &str)]| run_as(&home, &me, &["new"], pairs, "x").unwrap_err().to_string();
        assert!(fail(&[("md", "-")]).contains("--title"));
        assert!(fail(&[("title", "T")]).contains("exactly one of --md"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("html", "-")]).contains("exactly one"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("folder", "../up")]).contains("--folder"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("folder", "C:\\abs")]).contains("--folder"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("folder", ".git")]).contains("--folder"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("name", "a/b.md")]).contains("--name"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("name", "x.html")]).contains("but the note is Markdown"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("audience", "bots")]).contains("--audience"));
        assert!(fail(&[("title", "T"), ("md", "-"), ("color", "red")]).contains("does not take --color"));
        assert!(fail(&[("title", "T"), ("md", "missing-file.md")]).contains("cannot read"));
        assert!(run_as(&home, &me, &["new"], &[("title", "T"), ("md", "-")], "\u{0}").is_ok(), "any UTF-8 text");
        let mut invalid: &[u8] = &[0xff, 0xfe];
        assert!(execute(&home, &["new"], &opts(&[("title", "T"), ("md", "-")]), &[], &me, &mut invalid).unwrap_err().to_string().contains("not UTF-8"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn names_from_the_option_keep_or_add_their_extension() {
        assert_eq!(file_name("plan", Kind::Markdown).unwrap(), ("plan".into(), "md".into()));
        assert_eq!(file_name("plan.markdown", Kind::Markdown).unwrap(), ("plan".into(), "markdown".into()));
        assert_eq!(file_name("v1.2 notes", Kind::Markdown).unwrap(), ("v1.2 notes".into(), "md".into()));
        assert_eq!(file_name("page.HTM", Kind::Html).unwrap(), ("page".into(), "HTM".into()));
        for bad in ["", ".hidden", "a\\b", "x.", "c:x", "x.md", "con", "nul.md", "LPT1.html", "a\tb"] {
            assert!(file_name(bad, Kind::Html).is_err(), "{bad:?}");
        }
        assert_eq!(subfolder("a/b\\c").unwrap(), PathBuf::from("a").join("b").join("c"));
        assert_eq!(subfolder("").unwrap(), PathBuf::new());
        for bad in ["/abs", "a/../b", "a/./b", ".hidden/x", "a/b:c", " x", "x /y", "dots./x", "con/x", "a/AUX.d"] {
            assert!(subfolder(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_agent_comes_from_the_sessions_marker_else_the_routing_and_odd_folders_count_for_nothing() {
        let home = temp("agent");
        let folder = route(&home, None);
        let mut routing = crate::cli::routing(&home);
        routing.agents.insert("s1".into(), "claude".into());
        // A relative group folder or notebook root (a broken routing) never writes relative to the working folder.
        routing.groups[1].folder = Some("relative\\loose".into());
        std::fs::write(home.join("bus").join("routing.json"), serde_json::to_string(&routing).unwrap()).unwrap();
        let unmarked = Caller { session: "s1".into(), agent: String::new() };
        let path = PathBuf::from(run_as(&home, &unmarked, &["new"], &[("title", "From routing"), ("md", "-")], "x").unwrap());
        assert_eq!(notebook::markdown_meta(&std::fs::read_to_string(&path).unwrap()).get("agent"), Some("claude"));
        let marked = PathBuf::from(run_as(&home, &session("s1"), &["new"], &[("title", "Marked"), ("md", "-")], "x").unwrap());
        assert_eq!(notebook::markdown_meta(&std::fs::read_to_string(&marked).unwrap()).get("agent"), Some("opencode"), "the session's own marker first");
        assert!(path.starts_with(&folder));
        let inbox = home.join("notebook").join("Inbox");
        assert_eq!(run_as(&home, &session("s2"), &["path"], &[], "").unwrap(), inbox.display().to_string());
        routing.notebook_root = Some("relative\\notes".into());
        std::fs::write(home.join("bus").join("routing.json"), serde_json::to_string(&routing).unwrap()).unwrap();
        assert_eq!(run_as(&home, &Caller::default(), &["path"], &[], "").unwrap(), inbox.display().to_string());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn lists_show_the_groups_notes_with_their_audience() {
        let home = temp("list");
        let folder = route(&home, None);
        let me = session("s1");
        run_as(&home, &me, &["new"], &[("title", "Plan"), ("md", "-")], "x").unwrap();
        run_as(&home, &me, &["new"], &[("title", "Report"), ("md", "-"), ("audience", "human")], "x").unwrap();
        std::fs::create_dir_all(folder.join("human-outputs").join(".hidden")).unwrap();
        std::fs::write(folder.join("human-outputs").join(".hidden").join("x.md"), "x").unwrap();
        std::fs::write(folder.join("ai-docs").join("data.bin"), "x").unwrap();
        std::fs::write(folder.join("ai-docs").join("GOALS.md"), "# Goals\n").unwrap();
        let text = run_as(&home, &me, &["list"], &[], "").unwrap();
        assert!(text.starts_with(&format!("Notes of the group \"Team\" ({}): 3", folder.display())), "{text}");
        assert!(text.contains("◇ ai") && text.contains("● human") && text.contains("Goals") && text.contains(", by opencode") && !text.contains("data.bin") && !text.contains(".hidden"), "{text}");
        let json: serde_json::Value = serde_json::from_str(&run_as(&home, &me, &["list"], &[("audience", "human"), ("format", "json")], "").unwrap()).unwrap();
        assert_eq!((json["schemaVersion"].as_u64(), json["kind"].as_str(), json["group"]["name"].as_str()), (Some(1), Some("note-list"), Some("Team")));
        let notes = json["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!((notes[0]["title"].as_str(), notes[0]["audience"].as_str(), notes[0]["rel"].as_str(), notes[0]["source_session"].as_str()), (Some("Report"), Some("human"), Some("human-outputs/Report.md"), Some("s1")));
        let empty = run_as(&home, &Caller::default(), &["list"], &[], "").unwrap();
        assert!(empty.contains("Inbox") && empty.contains("(none yet"), "{empty}");
        assert!(run_as(&home, &me, &["list"], &[("format", "yaml")], "").unwrap_err().to_string().contains("--format is json"));
        // --internal: internal files too (marked), never what is in .git.
        std::fs::create_dir_all(folder.join("ai-docs").join(".git")).unwrap();
        std::fs::write(folder.join("ai-docs").join(".git").join("HEAD.md"), "x").unwrap();
        let internal = |words: &[&str], pairs: &[(&str, &str)]| execute(&home, words, &opts(pairs), &["internal".to_owned()], &me, &mut "".as_bytes());
        let all = internal(&["list"], &[]).unwrap();
        assert!(all.contains(".hidden") && all.contains(", internal)") && !all.contains("HEAD"), "{all}");
        let json: serde_json::Value = serde_json::from_str(&internal(&["list"], &[("format", "json")]).unwrap()).unwrap();
        let hidden: Vec<&serde_json::Value> = json["notes"].as_array().unwrap().iter().filter(|n| n["internal"] == true).collect();
        assert_eq!(hidden.len(), 1, "{json}");
        assert!(hidden[0]["rel"].as_str().unwrap().contains(".hidden"));
        assert!(internal(&["path"], &[]).unwrap_err().to_string().contains("note path does not take --internal"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
