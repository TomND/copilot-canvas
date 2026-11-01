//! The notebook (docs/NOTEBOOK-TASKS.md, "Notebook"): note provenance
//! (who a note is for, who wrote it), frontmatter, and the data types of the
//! backend's notebook requests.
//!
//! A Markdown note may start with a block of flat `key: value` lines between
//! `---` lines (its frontmatter); an HTML note carries the same keys as
//! `<meta name="canvas-KEY" content="VALUE">`. Keys: `audience` (`human` |
//! `ai`), `author` (`human` | `ai` | `mixed`), `agent`, `source_session`,
//! `created`, `reviewed`, `title`. [`classify`] reads them, else infers the
//! audience from the note's folder and file type. Rewrites keep unknown keys
//! and their order.
//!
//! Rendering: [`markdown_html`] (then `report_html::document(.., &report_html::NOTEBOOK)`)
//! turns `[[Note]]` into `#note:` links and todo ids (`DEC-031`) into `#todo:`
//! links; [`link_target`] reads them back.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::report_html::{self, Token};

/// Largest note `NoteRead` and `NoteWrite` handle.
pub const MAX_NOTE: u64 = 4 << 20;
/// Largest file `ReadBytes` answers.
pub const MAX_BYTES: u64 = 16 << 20;
/// Entries one `NotebookList` answers at most.
pub const MAX_ENTRIES: usize = 5_000;
/// Bytes read from the start of a note for its metadata and title.
pub const META_BYTES: usize = 8 << 10;
/// Hits one `NoteSearch` answers at most.
pub const MAX_HITS: u32 = 500;

/// Folders the notebook root always has.
pub const GROUPS: &str = "Groups";
pub const INBOX: &str = "Inbox";
pub const TODOS: &str = "Todos";
/// Deleted notes go here (`<root>/.trash/<ms>-<name>`).
pub const TRASH: &str = ".trash";

/// The notebook root: the canvas setting (`Doc::notebook_root`), else `<home>/notebook`.
pub fn root_of(setting: Option<&str>, home: &Path) -> PathBuf {
    match setting.map(str::trim).filter(|s| !s.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => home.join("notebook"),
    }
}

/// The folder suggested for a new group: `<root>/Groups/<group name>`.
pub fn group_folder(root: &Path, group_name: &str) -> PathBuf {
    root.join(GROUPS).join(safe_name(group_name, "Group"))
}

/// Whom a note is for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// For you (`●`).
    #[default]
    Human,
    /// For agents (`◇`).
    Ai,
}

impl Audience {
    /// `human` | `ai`; also `agent` (ai) and `user` / `you` (human).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "human" | "user" | "you" => Some(Self::Human),
            "ai" | "agent" => Some(Self::Ai),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Human => "●",
            Self::Ai => "◇",
        }
    }
}

/// Who wrote a note.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    #[default]
    Human,
    Ai,
    /// An agent's note you edited.
    Mixed,
}

impl Author {
    /// `human` | `ai` | `mixed`; also `agent` (ai) and `user` / `you` (human).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "human" | "user" | "you" => Some(Self::Human),
            "ai" | "agent" => Some(Self::Ai),
            "mixed" => Some(Self::Mixed),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
            Self::Mixed => "mixed",
        }
    }
}

/// Where a note's audience came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The note's own metadata (frontmatter or meta tags).
    Frontmatter,
    /// Its folder (`ai-docs`, `.canvas`, `human-outputs`, the canvas-wide folder, Reports).
    Folder,
    /// HTML files are for you.
    FileType,
    /// Nothing said otherwise: for you.
    #[default]
    Default,
}

/// What kind of explorer root a path is under (classification).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// The notebook root, a group folder or a mount.
    #[default]
    Notes,
    /// The canvas-wide folder (`<home>/canvas-agents/<canvas id>`): written by and for agents.
    CanvasWide,
    /// The virtual Reports section (the inbox).
    Reports,
}

/// What a notebook entry is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Folder,
    /// `.md`, `.markdown`.
    Markdown,
    /// `.html`, `.htm`.
    Html,
    /// Other known text files.
    Text,
    /// `png jpg jpeg gif svg webp`.
    Image,
    #[default]
    Other,
}

/// Extensions of text files the notebook opens and searches.
const TEXT: &[&str] = &[
    "txt", "text", "log", "json", "jsonl", "yaml", "yml", "toml", "csv", "tsv", "xml", "ini", "cfg", "conf", "env", "gitignore", "gitattributes", "editorconfig", "rs", "py", "js", "mjs", "cjs",
    "ts", "tsx", "jsx", "ps1", "psm1", "psd1", "sh", "bash", "zsh", "bat", "cmd", "c", "h", "cc", "cpp", "hpp", "cs", "go", "java", "kt", "swift", "rb", "php", "sql", "css", "scss", "less", "lua",
    "gd", "hlsl", "glsl", "shader", "cmake", "diff", "patch", "rst", "adoc", "tex", "org", "vue", "svelte", "proto", "graphql", "tf", "bicep", "csproj", "fsproj", "vcxproj", "sln", "props",
    "targets", "xaml", "plist", "properties", "gradle", "r", "jl", "dart", "zig", "nim", "ex", "exs", "erl", "hs", "ml", "fs", "clj", "scala", "pl", "awk", "vim", "nix", "dockerfile", "mdx",
];

impl Kind {
    /// The kind of a file by its name's extension.
    pub fn of(name: &str) -> Self {
        let ext = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => Kind::Markdown,
            "html" | "htm" => Kind::Html,
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => Kind::Image,
            ext if TEXT.contains(&ext) => Kind::Text,
            _ => Kind::Other,
        }
    }

    /// Markdown, HTML and other text: opened as text, searched.
    pub fn is_text(self) -> bool {
        matches!(self, Kind::Markdown | Kind::Html | Kind::Text)
    }
}

/// A note's metadata keys and values in their order (frontmatter or `canvas-` meta tags).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta(pub Vec<(String, String)>);

impl Meta {
    /// A key's value (keys compare case-insensitively; the first wins).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whom a note is for and who wrote it ([`classify`]).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance {
    pub audience: Audience,
    pub author: Author,
    /// Where the audience came from.
    pub source: Source,
    /// The audience was inferred (not from the note's own metadata).
    pub inferred: bool,
    /// A file of the canvas's own (hidden unless *Show internal files*).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
    /// A passive session's memory (`.canvas/passive/<slug>/{rules,state,log}.md`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub passive: bool,
    /// The agent that wrote it (`copilot`, `opencode`, `claude`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The canvas session that wrote it (`source_session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Marked reviewed (`reviewed: true`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reviewed: bool,
}

/// The parts of a relative path (`/` or `\` separators).
fn parts(rel_path: &str) -> Vec<&str> {
    rel_path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect()
}

fn folder_audience(part: &str) -> Option<Audience> {
    if part.eq_ignore_ascii_case(crate::doc::AI_DOCS) || part.eq_ignore_ascii_case(".canvas") {
        Some(Audience::Ai)
    } else if part.eq_ignore_ascii_case(crate::doc::HUMAN_OUTPUTS) {
        Some(Audience::Human)
    } else {
        None
    }
}

/// Passive memory files (`.canvas/passive/<slug>/<file>`).
const MEMORY: &[&str] = &["rules.md", "state.md", "log.md"];

/// Whom a note is for and who wrote it. `rel_path`: relative to its explorer
/// root, `/` separators, ending in `/` for a folder; `meta`: the note's own
/// metadata; `place`: the kind of root.
///
/// Audience: the metadata (source frontmatter), else by folder (any
/// component; the deepest decides): `ai-docs` and `.canvas` ai,
/// `human-outputs` human, else the canvas-wide root ai and Reports human
/// (source folder), else `.html`/`.htm` human (file type), else human
/// (default). Author: the metadata, else ai inside `ai-docs`, `.canvas`,
/// `human-outputs`, the canvas-wide root and Reports, else human.
pub fn classify(rel_path: &str, meta: &Meta, place: Place) -> Provenance {
    let dir = rel_path.ends_with(['/', '\\']);
    let parts = parts(rel_path);
    let last = parts.last().copied().unwrap_or_default();
    let by_folder = parts.iter().rev().find_map(|p| folder_audience(p)).or(match place {
        Place::CanvasWide => Some(Audience::Ai),
        Place::Reports => Some(Audience::Human),
        Place::Notes => None,
    });
    let (audience, source) = if let Some(audience) = meta.get("audience").and_then(Audience::parse) {
        (audience, Source::Frontmatter)
    } else if let Some(audience) = by_folder {
        (audience, Source::Folder)
    } else if !dir && Kind::of(last) == Kind::Html {
        (Audience::Human, Source::FileType)
    } else {
        (Audience::Human, Source::Default)
    };
    let agents_folder = parts.iter().any(|p| folder_audience(p).is_some()) || place != Place::Notes;
    let author = meta.get("author").and_then(Author::parse).unwrap_or(if agents_folder { Author::Ai } else { Author::Human });
    let text = |key: &str| meta.get(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    Provenance {
        audience,
        author,
        source,
        inferred: source != Source::Frontmatter,
        internal: is_internal(&parts, dir),
        passive: !dir && passive_memory(&parts),
        agent: text("agent"),
        session: text("source_session"),
        created: text("created"),
        reviewed: meta.get("reviewed").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1")),
    }
}

/// `.canvas/passive/<slug>/{rules,state,log}.md`.
fn passive_memory(parts: &[&str]) -> bool {
    let Some(at) = parts.iter().position(|p| p.eq_ignore_ascii_case(".canvas")) else { return false };
    matches!(&parts[at + 1..], [passive, _, file] if passive.eq_ignore_ascii_case("passive") && MEMORY.contains(&file.to_ascii_lowercase().as_str()))
}

/// The canvas's own files: inside `.canvas` (but the folders leading to
/// passive memory and the memory itself), inside `.trash` or `.git`, names
/// starting with `.`, `events.yaml`, `canvas-group.instructions.md`.
fn is_internal(parts: &[&str], dir: bool) -> bool {
    let Some(last) = parts.last() else { return false };
    if parts.iter().any(|p| p.eq_ignore_ascii_case(TRASH) || p.eq_ignore_ascii_case(".git")) {
        return true;
    }
    if let Some(at) = parts.iter().position(|p| p.eq_ignore_ascii_case(".canvas")) {
        let rest = &parts[at + 1..];
        let shown = match rest {
            [] => dir,
            [passive] | [passive, _] => dir && passive.eq_ignore_ascii_case("passive"),
            _ => passive_memory(parts) && !dir,
        };
        return !shown || parts[..at].iter().any(|p| p.starts_with('.'));
    }
    parts.iter().any(|p| p.starts_with('.')) || last.eq_ignore_ascii_case("events.yaml") || last.eq_ignore_ascii_case("canvas-group.instructions.md")
}

// ---------- frontmatter ----------

/// A Markdown note's frontmatter block.
#[derive(Clone, Debug, PartialEq)]
pub struct Frontmatter<'a> {
    /// The lines between the delimiters (without line ends).
    pub lines: Vec<&'a str>,
    /// Byte offset of the opening `---` (after a byte order mark).
    pub start: usize,
    /// Byte offset of the body (after the closing line).
    pub end: usize,
    /// The block's lines end in `\r\n`.
    pub crlf: bool,
}

/// `key: value` (a key at the start of the line, then `:` and a space or the
/// end; keys may have inner spaces, as Obsidian properties do).
fn key_value(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim_end();
    let first = key.chars().next()?;
    if !(first.is_alphanumeric() || first == '_') || !key.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ')) {
        return None;
    }
    let rest = &line[colon + 1..];
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    Some((key, rest.trim()))
}

/// A line that belongs to the key above it (a list item, an indented value).
fn continuation(line: &str) -> bool {
    line.starts_with([' ', '\t']) || line.starts_with("- ") || line == "-"
}

/// The frontmatter at the very start of a Markdown text (after a byte order
/// mark): `---`, flat `key: value` lines (comments, blank lines and indented
/// or list continuation lines allowed), then `---` or `...`. Anything else
/// in between means the text has none (a `---` rule is not eaten).
pub fn frontmatter(text: &str) -> Option<Frontmatter<'_>> {
    let start = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let first_end = start + text[start..].find('\n')?;
    let first = &text[start..first_end];
    if first.trim_end() != "---" {
        return None;
    }
    let crlf = first.ends_with('\r');
    let mut lines = Vec::new();
    let mut at = first_end + 1;
    while at < text.len() {
        let line_end = text[at..].find('\n').map(|e| at + e);
        let raw = &text[at..line_end.unwrap_or(text.len())];
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if matches!(line.trim_end(), "---" | "...") {
            return Some(Frontmatter { lines, start, end: line_end.map_or(text.len(), |e| e + 1), crlf });
        }
        let fits = line.trim().is_empty() || line.trim_start().starts_with('#') || key_value(line).is_some() || continuation(line);
        if !fits {
            return None;
        }
        lines.push(line);
        at = line_end? + 1;
    }
    None
}

/// The body of a Markdown text: without its frontmatter.
pub fn strip_frontmatter(text: &str) -> &str {
    match frontmatter(text) {
        Some(block) => &text[block.end..],
        None => text.strip_prefix('\u{feff}').unwrap_or(text),
    }
}

/// A value as written: double quotes (`\"`, `\\`, `\n`, `\t`), single quotes
/// (`''`), or plain (a ` #` starts a comment).
fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        return out;
    }
    if let Some(inner) = raw.strip_prefix('\'') {
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\'' {
                out.push(c);
            } else if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
            } else {
                break;
            }
        }
        return out;
    }
    if raw.starts_with('#') {
        return String::new();
    }
    let comment = [" #", "\t#"].iter().filter_map(|c| raw.find(c)).min();
    comment.map_or(raw, |at| &raw[..at]).trim().to_owned()
}

/// A value for a frontmatter line: plain when YAML reads it back unchanged,
/// else double-quoted.
fn scalar(value: &str) -> String {
    let special = value.is_empty()
        || value != value.trim()
        || value.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
        || value.contains(": ")
        || value.contains(" #")
        || value.ends_with(':')
        || value.chars().any(char::is_control);
    if !special {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The frontmatter's keys and values (unquoted), in their order.
pub fn markdown_meta(text: &str) -> Meta {
    let Some(block) = frontmatter(text) else { return Meta::default() };
    Meta(block.lines.iter().filter_map(|line| key_value(line)).map(|(key, value)| (key.to_owned(), unquote(value))).collect())
}

/// The last change per key, in first-seen order.
fn last_per_key<'a>(changes: &[(&'a str, Option<&'a str>)]) -> Vec<(&'a str, Option<&'a str>)> {
    let mut out: Vec<(&str, Option<&str>)> = Vec::new();
    for (key, value) in changes {
        match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            Some(slot) => slot.1 = *value,
            None => out.push((key, *value)),
        }
    }
    out
}

/// A key a frontmatter line (or a `canvas-` meta tag) can carry and
/// [`key_value`] reads back as itself (no `:`, line ends or outer spaces).
fn valid_key(key: &str) -> bool {
    key_value(&format!("{key}: x")).is_some_and(|(k, _)| k == key)
}

fn apply_changes(lines: &mut Vec<String>, changes: &[(&str, Option<&str>)]) {
    for (key, value) in last_per_key(changes).into_iter().filter(|(key, _)| valid_key(key)) {
        let found = lines.iter().position(|l| key_value(l).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)));
        match (found, value) {
            (Some(at), value) => {
                let mut end = at + 1;
                while end < lines.len() && continuation(&lines[end]) {
                    end += 1;
                }
                let written = key_value(&lines[at]).map(|(k, _)| k.to_owned()).unwrap_or_else(|| key.to_owned());
                let tail = lines.split_off(end);
                lines.truncate(at);
                if let Some(value) = value {
                    lines.push(format!("{written}: {}", scalar(value)));
                }
                lines.extend(tail);
            }
            (None, Some(value)) => lines.push(format!("{key}: {}", scalar(value))),
            (None, None) => {}
        }
    }
}

/// Sets (`Some`) or removes (`None`) frontmatter keys: a key already there
/// keeps its place (its continuation lines go with it), new ones are added at
/// the end, everything else stays as written. A text without frontmatter
/// gets one (unless nothing is set). Line ends follow the text's. Keys no
/// frontmatter line can carry (with `:` or a line end) are left out.
pub fn set_frontmatter(text: &str, changes: &[(&str, Option<&str>)]) -> String {
    let bom = if text.starts_with('\u{feff}') { "\u{feff}" } else { "" };
    let (mut lines, nl, body) = match frontmatter(text) {
        Some(block) => (block.lines.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>(), if block.crlf { "\r\n" } else { "\n" }, &text[block.end..]),
        None if changes.iter().all(|(k, v)| v.is_none() || !valid_key(k)) => return text.to_owned(),
        None => (Vec::new(), if text.contains("\r\n") { "\r\n" } else { "\n" }, &text[bom.len()..]),
    };
    apply_changes(&mut lines, changes);
    let mut out = String::with_capacity(text.len() + 64);
    out.push_str(bom);
    out.push_str("---");
    out.push_str(nl);
    for line in &lines {
        out.push_str(line);
        out.push_str(nl);
    }
    out.push_str("---");
    out.push_str(nl);
    out.push_str(body);
    out
}

// ---------- HTML meta tags ----------

/// Elements whose content is raw text (no tags inside).
const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript"];

/// Start tags of an HTML text: (start, end, lower-case name); comments,
/// end tags and raw text (scripts, styles) are skipped.
fn start_tags(html: &str) -> Vec<(usize, usize, String)> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if lower[i..].starts_with("<!--") {
            i = lower[i + 4..].find("-->").map_or(bytes.len(), |e| i + 4 + e + 3);
            continue;
        }
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        if matches!(next, b'!' | b'?' | b'/') {
            i = lower[i..].find('>').map_or(bytes.len(), |e| i + e + 1);
            continue;
        }
        if !next.is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let name_end = (i + 1..bytes.len()).find(|&j| bytes[j].is_ascii_whitespace() || matches!(bytes[j], b'>' | b'/')).unwrap_or(bytes.len());
        let name = lower[i + 1..name_end].to_owned();
        let mut j = name_end;
        let mut quote = 0u8;
        while j < bytes.len() {
            let b = bytes[j];
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
            } else if b == b'"' || b == b'\'' {
                quote = b;
            } else if b == b'>' {
                break;
            }
            j += 1;
        }
        let end = (j + 1).min(bytes.len());
        out.push((i, end, name.clone()));
        i = end;
        if RAW.contains(&name.as_str()) {
            i = lower[i..].find(&format!("</{name}")).map_or(bytes.len(), |e| i + e);
        }
    }
    out
}

/// The `canvas-` meta tags of an HTML text, in their order (keys lower case).
pub fn html_meta(html: &str) -> Meta {
    let mut meta = Meta::default();
    for token in report_html::tokens(html) {
        if let Token::Start { name, attrs, .. } = token
            && name == "meta"
            && let Some(key) = canvas_key(&attrs)
        {
            let content = attrs.iter().find(|(k, _)| k == "content").map(|(_, v)| v.clone()).unwrap_or_default();
            meta.0.push((key, content));
        }
    }
    meta
}

/// The key of a `<meta name="canvas-KEY">` tag's attributes.
fn canvas_key(attrs: &[(String, String)]) -> Option<String> {
    let name = attrs.iter().find(|(k, _)| k == "name")?.1.trim().to_ascii_lowercase();
    name.strip_prefix("canvas-").filter(|k| !k.is_empty()).map(str::to_owned)
}

fn escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

fn meta_tag(key: &str, value: &str) -> String {
    format!("<meta name=\"canvas-{}\" content=\"{}\">", escape_attr(key), escape_attr(value))
}

/// Sets (`Some`) or removes (`None`) `canvas-` meta tags of an HTML text: a
/// tag already there is replaced in place, new ones go right after `<head>`
/// (else `<html>`, else the doctype, else the start). Nothing else changes.
/// Keys and values are escaped; keys a frontmatter could not carry are left out.
pub fn set_html_meta(html: &str, changes: &[(&str, Option<&str>)]) -> String {
    let tags = start_tags(html);
    let metas: Vec<(usize, usize, String)> = tags
        .iter()
        .filter(|(_, _, name)| name == "meta")
        .filter_map(|(start, end, _)| match report_html::tokens(&html[*start..*end]).into_iter().next() {
            Some(Token::Start { attrs, .. }) => canvas_key(&attrs).map(|key| (*start, *end, key)),
            _ => None,
        })
        .collect();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut added = String::new();
    for (key, value) in last_per_key(changes).into_iter().filter(|(key, _)| valid_key(key.trim())) {
        let key = key.trim().to_ascii_lowercase();
        let mut found = metas.iter().filter(|m| m.2 == key);
        match found.next() {
            Some(first) => {
                edits.push((first.0, first.1, value.map(|v| meta_tag(&key, v)).unwrap_or_default()));
                edits.extend(found.map(|dup| (dup.0, dup.1, String::new())));
            }
            None => {
                if let Some(value) = value {
                    added.push('\n');
                    added.push_str(&meta_tag(&key, value));
                }
            }
        }
    }
    if !added.is_empty() {
        let lower = html.to_ascii_lowercase();
        let doctype = lower.find("<!doctype").and_then(|at| lower[at..].find('>').map(|e| at + e + 1));
        let bom = if html.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let at = ["head", "html"].iter().find_map(|name| tags.iter().find(|t| t.2 == *name).map(|t| t.1)).or(doctype).unwrap_or(bom);
        if at == bom {
            // Before everything: the tags, then the text on its own line.
            added = format!("{}\n", added.trim_start_matches('\n'));
        }
        edits.push((at, at, added));
    }
    edits.sort_by_key(|e| (e.0, e.1));
    let mut out = String::with_capacity(html.len() + 128);
    let mut cursor = 0;
    for (start, end, text) in edits {
        if start < cursor {
            continue;
        }
        out.push_str(&html[cursor..start]);
        out.push_str(&text);
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// A note's metadata: Markdown frontmatter, HTML meta tags, else none.
pub fn note_meta(kind: Kind, text: &str) -> Meta {
    match kind {
        Kind::Markdown => markdown_meta(text),
        Kind::Html => html_meta(text),
        _ => Meta::default(),
    }
}

/// Sets or removes metadata keys of a note ([`set_frontmatter`] for
/// Markdown, [`set_html_meta`] for HTML; other kinds stay as they are).
/// E.g. `author: mixed` when you save an agent's note, `reviewed: true` on
/// *Mark reviewed*.
pub fn set_meta(kind: Kind, text: &str, changes: &[(&str, Option<&str>)]) -> String {
    match kind {
        Kind::Markdown => set_frontmatter(text, changes),
        Kind::Html => set_html_meta(text, changes),
        _ => text.to_owned(),
    }
}

// ---------- titles ----------

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first level-1 heading (`# Title`) of a Markdown text, outside code
/// (fences, which a run of at least as many of their marks closes, and
/// lines indented by four or more spaces) and after the frontmatter.
pub fn markdown_title(text: &str) -> Option<String> {
    let mut fence: Option<(char, usize)> = None;
    for raw in strip_frontmatter(text).lines() {
        let line = raw.trim_start_matches(' ');
        let indent = raw.len() - line.len();
        let run = |mark: char| line.chars().take_while(|c| *c == mark).count();
        if let Some((mark, len)) = fence {
            if indent <= 3 && run(mark) >= len && line[run(mark)..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if indent > 3 {
            continue;
        }
        if let Some(mark) = ['`', '~'].into_iter().find(|m| run(*m) >= 3) {
            fence = Some((mark, run(mark)));
            continue;
        }
        let rest = match line.strip_prefix('#') {
            Some(rest) if rest.is_empty() || rest.starts_with([' ', '\t']) => rest,
            _ => continue,
        };
        let title = collapse(rest.trim().trim_end_matches('#'));
        if !title.is_empty() {
            return Some(title);
        }
    }
    None
}

/// An HTML text's `<title>`, else its first `<h1>` (text only).
pub fn html_title(html: &str) -> Option<String> {
    let tokens = report_html::tokens(html);
    let text_of = |element: &str| -> Option<String> {
        let start = tokens.iter().position(|t| matches!(t, Token::Start { name, .. } if name == element))?;
        let mut text = String::new();
        for token in &tokens[start + 1..] {
            match token {
                Token::End(name) if name == element => break,
                Token::Text(t) => text.push_str(t),
                _ => {}
            }
        }
        let text = collapse(&crate::reports::decode_entities(&text));
        (!text.is_empty()).then_some(text)
    };
    text_of("title").or_else(|| text_of("h1"))
}

/// A file name without its extension.
pub fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// A note's title: its metadata `title`, else the first `# ` heading (HTML:
/// `<title>`, else `<h1>`), else the file name without its extension.
pub fn title(kind: Kind, text: &str, meta: &Meta, name: &str) -> String {
    if let Some(title) = meta.get("title").map(collapse).filter(|t| !t.is_empty()) {
        return title;
    }
    let found = match kind {
        Kind::Markdown => markdown_title(text),
        Kind::Html => html_title(text),
        _ => None,
    };
    found.unwrap_or_else(|| stem(name).to_owned())
}

// ---------- new notes ----------

/// The provenance of a note being created ([`new_note_text`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewNote {
    pub title: String,
    pub audience: Audience,
    pub author: Author,
    /// `agent` (`copilot`, `opencode`, `claude`).
    pub agent: Option<String>,
    /// `source_session` (the canvas session's id).
    pub session: Option<String>,
    /// `created` (RFC 3339).
    pub created: String,
    /// More keys after these (e.g. `report`).
    pub extra: Vec<(String, String)>,
}

impl NewNote {
    fn fields(&self) -> Vec<(String, String)> {
        let mut fields = Vec::new();
        let title = collapse(&self.title);
        if !title.is_empty() {
            fields.push(("title".to_owned(), title));
        }
        fields.push(("audience".to_owned(), self.audience.id().to_owned()));
        fields.push(("author".to_owned(), self.author.id().to_owned()));
        for (key, value) in [("agent", &self.agent), ("source_session", &self.session)] {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                fields.push((key.to_owned(), value.to_owned()));
            }
        }
        if !self.created.trim().is_empty() {
            fields.push(("created".to_owned(), self.created.trim().to_owned()));
        }
        fields.extend(self.extra.iter().filter(|(k, _)| !k.trim().is_empty()).cloned());
        fields
    }
}

/// A new note's text: `body` with the provenance as frontmatter (Markdown:
/// keys its own frontmatter has are replaced, others kept; a body without a
/// level-1 heading gets `# <title>`) or as `canvas-` meta tags (HTML).
pub fn new_note_text(kind: Kind, body: &str, note: &NewNote) -> String {
    let fields = note.fields();
    let changes: Vec<(&str, Option<&str>)> = fields.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
    if kind == Kind::Html {
        return set_html_meta(body, &changes);
    }
    let title = collapse(&note.title);
    let text = if markdown_title(body).is_none() && !title.is_empty() {
        let (head, rest) = match frontmatter(body) {
            Some(block) => body.split_at(block.end),
            None => ("", body.strip_prefix('\u{feff}').unwrap_or(body)),
        };
        let nl = if body.contains("\r\n") { "\r\n" } else { "\n" };
        let rest = rest.trim_start_matches(['\r', '\n']);
        if rest.is_empty() { format!("{head}# {title}{nl}") } else { format!("{head}# {title}{nl}{nl}{rest}") }
    } else {
        body.to_owned()
    };
    set_frontmatter(&text, &changes)
}

// ---------- rendering ----------

fn gfm() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES
}

/// Markdown as HTML with the extensions the canvas shows everywhere (tables,
/// task lists, strikethrough, footnotes); no notebook links. Raw HTML in it
/// stays: pages go through `report_html::sanitize` afterwards.
pub fn commonmark_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3 / 2);
    pulldown_cmark::html::push_html(&mut out, pulldown_cmark::Parser::new_ext(text, gfm()));
    out
}

/// A note as HTML: [`commonmark_html`] without the frontmatter, with
/// `[[Note]]` / `[[Note|text]]` as `<a href="#note:Note">` and todo ids
/// (`DEC-031`, outside code and links) as `<a href="#todo:DEC-031">`. Show it
/// with `report_html::document(.., &report_html::NOTEBOOK)`.
pub fn markdown_html(text: &str) -> String {
    let body = strip_frontmatter(text);
    let mut out = String::with_capacity(body.len() * 3 / 2);
    pulldown_cmark::html::push_html(&mut out, linked_events(body).into_iter());
    out
}

/// The parser's events with wiki links pointing at `#note:` and todo ids as links.
fn linked_events(body: &str) -> Vec<pulldown_cmark::Event<'_>> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
    let mut out = Vec::new();
    let (mut code, mut link) = (0usize, 0usize);
    let mut text = String::new();
    for event in Parser::new_ext(body, gfm() | Options::ENABLE_WIKILINKS) {
        if let Event::Text(t) = &event
            && code == 0
            && link == 0
        {
            text.push_str(t);
            continue;
        }
        flush_text(&mut text, &mut out);
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                code += 1;
                out.push(Event::Start(Tag::CodeBlock(kind)));
            }
            Event::End(TagEnd::CodeBlock) => {
                code = code.saturating_sub(1);
                out.push(Event::End(TagEnd::CodeBlock));
            }
            Event::Start(Tag::Link { link_type: link_type @ LinkType::WikiLink { .. }, dest_url, title, id }) => {
                link += 1;
                out.push(Event::Start(Tag::Link { link_type, dest_url: format!("#note:{}", encode(&dest_url)).into(), title, id }));
            }
            Event::Start(tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                link += 1;
                out.push(Event::Start(tag));
            }
            Event::End(end @ (TagEnd::Link | TagEnd::Image)) => {
                link = link.saturating_sub(1);
                out.push(Event::End(end));
            }
            other => out.push(other),
        }
    }
    flush_text(&mut text, &mut out);
    out
}

/// Pending text as events, its todo ids as links.
fn flush_text(text: &mut String, out: &mut Vec<pulldown_cmark::Event<'_>>) {
    use pulldown_cmark::Event;
    if text.is_empty() {
        return;
    }
    let mut last = 0;
    for (start, end) in todo_spans(text) {
        if start > last {
            out.push(Event::Text(text[last..start].to_owned().into()));
        }
        out.push(Event::InlineHtml(format!("<a href=\"#todo:{0}\">{0}</a>", &text[start..end]).into()));
        last = end;
    }
    if last < text.len() {
        out.push(Event::Text(text[last..].to_owned().into()));
    }
    text.clear();
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Todo ids in plain text: 2 to 5 upper-case letters, `-`, 3 or more digits,
/// on word boundaries (byte ranges).
fn todo_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_uppercase() || text[..i].chars().next_back().is_some_and(word_char) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_uppercase() {
            j += 1;
        }
        if (2..=5).contains(&(j - i)) && bytes.get(j) == Some(&b'-') {
            let mut k = j + 1;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                k += 1;
            }
            if k - (j + 1) >= 3 && !text[k..].chars().next().is_some_and(word_char) {
                spans.push((i, k));
                i = k;
                continue;
            }
        }
        i = j;
    }
    spans
}

/// Whether `id` is a todo id (`DEC-031`).
pub fn is_todo_id(id: &str) -> bool {
    todo_spans(id) == [(0, id.len())]
}

/// The notes a Markdown text links to (`[[Note]]`, in order, once each).
pub fn note_links(text: &str) -> Vec<String> {
    use pulldown_cmark::{Event, LinkType, Tag};
    let mut out: Vec<String> = Vec::new();
    for event in linked_events(strip_frontmatter(text)) {
        if let Event::Start(Tag::Link { link_type: LinkType::WikiLink { .. }, dest_url, .. }) = event
            && let Some(LinkTarget::Note(name)) = link_target(&dest_url)
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

/// The todo ids a Markdown text mentions outside code and links (in order, once each).
pub fn todo_ids(text: &str) -> Vec<String> {
    use pulldown_cmark::Event;
    let mut out: Vec<String> = Vec::new();
    for event in linked_events(strip_frontmatter(text)) {
        if let Event::InlineHtml(html) = event
            && let Some(id) = html.strip_prefix("<a href=\"#todo:").and_then(|rest| rest.split_once('"')).map(|(id, _)| id.to_owned())
            && is_todo_id(&id)
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

/// Percent-encodes everything but unreserved and a few safe characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~!$()*+,;=:@/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b)))
        {
            out.push((high * 16 + low) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a link of a rendered note opens in the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// `#note:Name` (a `[[Name]]` link; the name as written, maybe with `#heading`).
    Note(String),
    /// `#todo:DEC-031`.
    Todo(String),
}

/// The app link an `href` of [`markdown_html`] stands for (`None`: another link).
pub fn link_target(href: &str) -> Option<LinkTarget> {
    let href = href.trim();
    if let Some(rest) = href.strip_prefix("#note:") {
        let name = decode(rest);
        return (!name.trim().is_empty()).then_some(LinkTarget::Note(name));
    }
    let id = decode(href.strip_prefix("#todo:")?);
    is_todo_id(&id).then_some(LinkTarget::Todo(id))
}

// ---------- backend data ----------

/// `Request::NotebookInfo`'s answer (paths on the service's machine).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// The notebook root (created, with `Groups/`, `Inbox/`, `Todos/`).
    pub root: String,
    /// The canvas-wide folder (`<home>/canvas-agents/<canvas id>`; `None`: no valid canvas id).
    pub canvas_dir: Option<String>,
    /// The service's data folder.
    pub home: String,
}

/// One entry of a notebook folder (`Request::NotebookList`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub name: String,
    /// Absolute path.
    pub path: String,
    /// Relative to the listing's root, `/` separators.
    pub rel: String,
    pub kind: Kind,
    /// Bytes (0 for folders).
    pub size: u64,
    pub modified_ms: u64,
    pub provenance: Provenance,
    /// Metadata `title`, the first heading, else the name without its extension (folders: the name).
    pub title: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Folder
    }
}

/// A note's text and revision (`Request::NoteRead`, a conflict's current text).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteText {
    /// UTF-8 (invalid bytes replaced: such a note is shown, never saved, as
    /// `NoteWrite` refuses to replace a file that is not UTF-8).
    pub text: String,
    /// [`rev_of`] the file's bytes.
    pub rev: String,
}

/// What `NoteWrite` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Written; the new revision.
    Written(String),
    /// The file changed since `base` (or exists, for a new note): nothing was written.
    Conflict(NoteText),
}

/// One search hit (`Request::NoteSearch`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    /// 1-based line of a content hit; 0 for a file name hit.
    pub line: u32,
    /// The line (shortened around the match) or the file name.
    pub text: String,
}

/// A file's revision: the first 8 bytes of its SHA-256, lowercase hex.
pub fn rev_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes).to_vec();
    canvas_common::paths::hex(&digest[..8])
}

// ---------- paths ----------

/// A name for a file or folder made from free text (a title, a group name):
/// characters Windows refuses become `-`, whitespace is collapsed, no leading
/// or trailing dots, dashes or spaces, not a device name (`CON` gets `_`), at
/// most 80 characters; `fallback` when nothing is left.
pub fn safe_name(text: &str, fallback: &str) -> String {
    let clean: String = text.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '-' } else { c }).collect();
    let clean = collapse(&clean).chars().take(80).collect::<String>();
    let clean = clean.trim_matches(|c: char| c == '.' || c == '-' || c.is_whitespace()).to_owned();
    if clean.is_empty() {
        return fallback.to_owned();
    }
    if is_device_name(&clean) { format!("_{clean}") } else { clean }
}

/// A name Windows takes for a device wherever it appears (`CON`, `nul.md`,
/// `COM1`, `LPT¹`, `CONIN$`): the part before the first `.`, trailing
/// spaces ignored, any case.
pub fn is_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let Some(number) = base.strip_prefix("COM").or_else(|| base.strip_prefix("LPT")) else { return false };
    let mut chars = number.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
}

/// A lower-case slug of free text for file names (`Q3: plan / review!` →
/// `q3-plan-review`): letters and digits, single `-` between words, at most
/// 60 characters (empty when nothing is left).
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.chars().take(60).collect::<String>().trim_end_matches('-').to_owned()
}

fn component_key(component: Component<'_>) -> String {
    let text = component.as_os_str().to_string_lossy();
    if cfg!(windows) { text.to_lowercase() } else { text.into_owned() }
}

/// `path` is `root` or inside it (compared by components; case-insensitive on Windows).
pub fn inside(path: &Path, root: &Path) -> bool {
    let mut parts = path.components().map(component_key);
    root.components().map(component_key).all(|r| parts.next().is_some_and(|p| p == r))
}

/// `path` relative to `root` with `/` separators (`None`: not inside it; `""`: the root).
pub fn rel_path(path: &Path, root: &Path) -> Option<String> {
    if !inside(path, root) {
        return None;
    }
    let skip = root.components().count();
    Some(path.components().skip(skip).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"))
}

/// An absolute path without `.` or `..` components. The notebook's reads
/// take any such path (as `Request::ReadText` does: the client can do what
/// you can on that machine); writes also need [`writable`].
pub fn checked_path(path: &str) -> Result<PathBuf, String> {
    let buf = PathBuf::from(path);
    if path.trim().is_empty() || !buf.is_absolute() {
        return Err(format!("{path:?} is not an absolute path"));
    }
    if buf.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(format!("{path} may not contain \".\" or \"..\""));
    }
    Ok(buf)
}

/// `path` is inside one of `roots` (not a root itself).
pub fn inside_roots(path: &Path, roots: &[String]) -> bool {
    roots.iter().filter_map(|root| checked_path(root).ok()).any(|root| inside(path, &root) && path.components().count() > root.components().count())
}

/// A path the notebook may write (create, change, rename, trash): absolute,
/// without `..`, not inside a `.git` folder, inside one of `roots` (the
/// explorer's roots the client names: they keep mistakes inside the
/// notebook). The backend adds the checks of its platform (Windows names,
/// links leading out of the roots).
pub fn writable(path: &str, roots: &[String]) -> Result<PathBuf, String> {
    let buf = checked_path(path)?;
    if buf.components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")) {
        return Err(format!("{path} is inside a .git folder"));
    }
    if !inside_roots(&buf, roots) {
        return Err(format!("{path} is outside the notebook's folders"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> Meta {
        Meta(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect())
    }

    #[test]
    fn frontmatter_is_read_with_quotes_comments_lists_and_crlf() {
        let text = "---\ntitle: \"Plan: phase \\\"2\\\"\"\naudience: agent # for the next session\nauthor: 'it''s ai'\ntags:\n  - a\n  - b\nempty:\n---\n# Body\n";
        let m = markdown_meta(text);
        assert_eq!(m.get("title"), Some("Plan: phase \"2\""));
        assert_eq!(m.get("AUDIENCE"), Some("agent"), "keys compare case-insensitively; comments go");
        assert_eq!(m.get("author"), Some("it's ai"));
        assert_eq!(m.get("tags"), Some(""), "lists stay raw text");
        assert_eq!(m.0.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["title", "audience", "author", "tags", "empty"]);
        assert_eq!(strip_frontmatter(text), "# Body\n");
        let crlf = "\u{feff}---\r\naudience: ai\r\n---\r\nBody\r\n";
        let block = frontmatter(crlf).unwrap();
        assert!(block.crlf && block.start == 3);
        assert_eq!(markdown_meta(crlf).get("audience"), Some("ai"));
        assert_eq!(strip_frontmatter(crlf), "Body\r\n");
        // Not frontmatter: no closing line, a rule followed by prose, not at the start.
        for text in ["---\naudience: ai\n", "---\nSome prose here.\n---\n", "x\n---\naudience: ai\n---\n", "--- \n"] {
            assert!(frontmatter(text).is_none() && markdown_meta(text).is_empty(), "{text:?}");
        }
        assert_eq!(strip_frontmatter("\u{feff}plain"), "plain");
        assert!(frontmatter("---\n---\nx").is_some() && markdown_meta("---\n---\nx").is_empty());
        assert_eq!(markdown_meta("---\na: 1\n...\nx").get("a"), Some("1"), "`...` closes too");
        assert_eq!(markdown_meta("---\nurl: http://x\n---\n").get("url"), Some("http://x"));
        assert!(frontmatter("---\nurl: http://x\nb:c\n---\n").is_none(), "a line that is no `key: value` means no frontmatter");
        // Obsidian properties: keys with spaces and other scripts, nested values stay continuation lines.
        let obsidian = "---\nmy key: 1\nÉtat : brouillon\nnested:\n  inner: x\n---\nBody";
        let m = markdown_meta(obsidian);
        assert_eq!((m.get("my key"), m.get("État"), m.get("inner")), (Some("1"), Some("brouillon"), None));
        assert_eq!(set_frontmatter(obsidian, &[("nested", Some("flat"))]), "---\nmy key: 1\nÉtat : brouillon\nnested: flat\n---\nBody");
    }

    #[test]
    fn frontmatter_rewrites_keep_unknown_keys_order_and_line_ends() {
        let text = "---\ntitle: Plan\ntags:\n  - x\naudience: ai # old\nzeta: 1\n---\nBody\n";
        let out = set_frontmatter(text, &[("audience", Some("human")), ("author", Some("mixed")), ("tags", None), ("missing", None)]);
        assert_eq!(out, "---\ntitle: Plan\naudience: human\nzeta: 1\nauthor: mixed\n---\nBody\n");
        // Quoting: values YAML would read differently are double-quoted and read back the same.
        for value in ["Plan: phase 2", "#1 thing", " padded ", "", "say \"hi\"", "a\nb", "- item", "back\\slash", "trailing:"] {
            let out = set_frontmatter("Body", &[("title", Some(value))]);
            assert_eq!(markdown_meta(&out).get("title"), Some(value), "{value:?} → {out}");
        }
        assert_eq!(set_frontmatter("Body", &[("reviewed", Some("true"))]), "---\nreviewed: true\n---\nBody");
        assert_eq!(set_frontmatter("x", &[("a", None)]), "x", "nothing to set: unchanged");
        let crlf = set_frontmatter("Line\r\nTwo\r\n", &[("author", Some("ai"))]);
        assert_eq!(crlf, "---\r\nauthor: ai\r\n---\r\nLine\r\nTwo\r\n");
        let crlf = set_frontmatter("---\r\nauthor: ai\r\nx: y\r\n---\r\nBody\r\n", &[("author", Some("mixed"))]);
        assert_eq!(crlf, "---\r\nauthor: mixed\r\nx: y\r\n---\r\nBody\r\n");
        let bom = set_frontmatter("\u{feff}# T\n", &[("audience", Some("ai")), ("audience", Some("human"))]);
        assert_eq!(bom, "\u{feff}---\naudience: human\n---\n# T\n", "the last change per key wins; the BOM stays first");
        // The key keeps its spelling; a closing line at the end of the file.
        assert_eq!(set_frontmatter("---\nAuthor: ai\n---", &[("author", Some("mixed"))]), "---\nAuthor: mixed\n---\n");
    }

    #[test]
    fn html_meta_tags_are_read_and_rewritten_in_place() {
        let html = "<!DOCTYPE html><html><head><title>T &amp; U</title><meta charset=utf-8><META NAME=\"canvas-audience\" CONTENT='ai'><meta name=\"canvas-author\" content=\"ai\"><meta name=\"canvas-author\" content=\"dup\"></head><body><h1>Head</h1><!-- <meta name=\"canvas-x\" content=\"no\"> --></body></html>";
        let m = html_meta(html);
        assert_eq!(m.get("audience"), Some("ai"));
        assert_eq!(m.get("author"), Some("ai"));
        assert_eq!(m.get("x"), None, "comments are skipped");
        let out = set_html_meta(html, &[("author", Some("mixed")), ("reviewed", Some("true")), ("audience", None)]);
        assert!(out.contains("<head>\n<meta name=\"canvas-reviewed\" content=\"true\"><title>"), "{out}");
        assert!(out.contains("<meta charset=utf-8><meta name=\"canvas-author\" content=\"mixed\"></head>"), "replaced in place, the duplicate removed: {out}");
        assert!(!out.to_lowercase().contains("canvas-audience"));
        let again = html_meta(&out);
        assert_eq!((again.get("author"), again.get("reviewed"), again.get("audience")), (Some("mixed"), Some("true"), None));
        // Values are escaped; tags go after <html> or the doctype when there is no <head>, else first.
        let out = set_html_meta("<html lang=en><p>x</p></html>", &[("title", Some("A \"quoted\" <b>"))]);
        assert_eq!(out, "<html lang=en>\n<meta name=\"canvas-title\" content=\"A &quot;quoted&quot; &lt;b&gt;\"><p>x</p></html>");
        assert_eq!(html_meta(&out).get("title"), Some("A \"quoted\" <b>"));
        assert_eq!(set_html_meta("<p>x</p>", &[("audience", Some("human"))]), "<meta name=\"canvas-audience\" content=\"human\">\n<p>x</p>");
        assert_eq!(set_html_meta("<!doctype html>\n<p>x</p>", &[("a", Some("1"))]), "<!doctype html>\n<meta name=\"canvas-a\" content=\"1\">\n<p>x</p>");
        // `<header>` is not `<head>`; a script mentioning <meta> is left alone.
        let tricky = "<header>x</header><script>var s = '<meta name=\"canvas-audience\" content=\"ai\">';</script>";
        assert_eq!(html_meta(tricky).get("audience"), None);
        assert_eq!(set_html_meta(tricky, &[("audience", Some("human"))]), format!("<meta name=\"canvas-audience\" content=\"human\">\n{tricky}"));
        assert_eq!(note_meta(Kind::Text, "---\na: b\n---\n"), Meta::default());
        assert_eq!(set_meta(Kind::Other, "x", &[("a", Some("b"))]), "x");
    }

    #[test]
    fn classification_follows_metadata_then_folder_then_file_type() {
        let none = Meta::default();
        let c = |rel: &str, m: &Meta, place| {
            let p = classify(rel, m, place);
            (p.audience, p.source, p.author)
        };
        use Audience::{Ai, Human};
        // 1. Metadata wins everywhere (with the aliases).
        assert_eq!(c("ai-docs/x.md", &meta(&[("audience", "human")]), Place::Notes), (Human, Source::Frontmatter, Author::Ai));
        assert_eq!(c("x.md", &meta(&[("audience", "Agent")]), Place::Notes), (Ai, Source::Frontmatter, Author::Human));
        assert_eq!(c("x.md", &meta(&[("audience", "you")]), Place::CanvasWide), (Human, Source::Frontmatter, Author::Ai));
        assert_eq!(c("x.md", &meta(&[("audience", "user"), ("author", "mixed")]), Place::Notes), (Human, Source::Frontmatter, Author::Mixed));
        assert_eq!(c("x.md", &meta(&[("audience", "robots")]), Place::Notes), (Human, Source::Default, Author::Human), "unknown values are ignored");
        // 2. Folders (any component, the deepest decides), the canvas-wide root, Reports.
        assert_eq!(c("Groups/X/ai-docs/plan.md", &none, Place::Notes), (Ai, Source::Folder, Author::Ai));
        assert_eq!(c("Groups/X/human-outputs/r.md", &none, Place::Notes), (Human, Source::Folder, Author::Ai));
        assert_eq!(c("g/.canvas/passive/guard/state.md", &none, Place::Notes), (Ai, Source::Folder, Author::Ai));
        assert_eq!(c("human-outputs/ai-docs/x.md", &none, Place::Notes), (Ai, Source::Folder, Author::Ai));
        assert_eq!(c("AI-Docs/x.html", &none, Place::Notes), (Ai, Source::Folder, Author::Ai), "folders before file type, any case");
        assert_eq!(c("agents/a/state.md", &none, Place::CanvasWide), (Ai, Source::Folder, Author::Ai));
        assert_eq!(c("GOALS.md", &none, Place::CanvasWide), (Ai, Source::Folder, Author::Ai));
        assert_eq!(c("brief", &none, Place::Reports), (Human, Source::Folder, Author::Ai));
        assert_eq!(c("ai-docs/", &none, Place::Notes), (Ai, Source::Folder, Author::Ai), "a folder itself");
        // 3. File type, 4. default.
        assert_eq!(c("Inbox/page.HTML", &none, Place::Notes), (Human, Source::FileType, Author::Human));
        assert_eq!(c("Inbox/page.htm", &meta(&[("author", "ai")]), Place::Notes), (Human, Source::FileType, Author::Ai));
        assert_eq!(c("Inbox/x.md", &none, Place::Notes), (Human, Source::Default, Author::Human));
        assert_eq!(c("html/", &none, Place::Notes), (Human, Source::Default, Author::Human));
        let p = classify("x.md", &meta(&[("audience", "ai"), ("agent", "opencode"), ("source_session", " s1 "), ("created", "2026-10-31T10:00:00Z"), ("reviewed", "True")]), Place::Notes);
        assert!(!p.inferred && p.reviewed && !p.internal && !p.passive);
        assert_eq!((p.agent.as_deref(), p.session.as_deref(), p.created.as_deref()), (Some("opencode"), Some("s1"), Some("2026-10-31T10:00:00Z")));
        assert!(classify("x.md", &none, Place::Notes).inferred);
        assert!(!classify("x.md", &meta(&[("reviewed", "no")]), Place::Notes).reviewed);
    }

    #[test]
    fn internal_files_and_passive_memory() {
        let internal = |rel: &str| classify(rel, &Meta::default(), Place::Notes).internal;
        for rel in [
            "g/.canvas/canvas-group.instructions.md",
            "g/.canvas/events.yaml",
            "events.yaml",
            "canvas-group.instructions.md",
            ".obsidian/",
            ".obsidian/app.json",
            ".hidden.md",
            ".trash/",
            ".trash/123-x.md",
            "a/.git/config",
            "g/.canvas/passive/guard/group-context.md",
            "g/.canvas/passive/guard/sub/state.md",
            "g/.canvas/other/",
            "g/.canvas/passive/guard/state.md/",
            ".x/g/.canvas/passive/guard/state.md",
        ] {
            assert!(internal(rel), "{rel} is internal");
        }
        for rel in ["g/.canvas/", "g/.canvas/passive/", "g/.canvas/passive/guard/", "g/.canvas/passive/guard/state.md", "g/.canvas/passive/guard/RULES.md", "g/ai-docs/GOALS.md", "notes/x.md", ""] {
            assert!(!internal(rel), "{rel} is shown");
        }
        assert!(classify("g/.canvas/passive/guard/log.md", &Meta::default(), Place::Notes).passive);
        assert!(!classify("g/.canvas/passive/guard/other.md", &Meta::default(), Place::Notes).passive);
        assert!(!classify("g/.canvas/passive/guard/", &Meta::default(), Place::Notes).passive);
    }

    #[test]
    fn titles_kinds_and_names() {
        assert_eq!(title(Kind::Markdown, "---\ntitle: From meta\n---\n# Heading\n", &markdown_meta("---\ntitle: From meta\n---\n"), "x.md"), "From meta");
        assert_eq!(title(Kind::Markdown, "intro\n```\n# not this\n```\n## two\n#nope\n  #  The  title ##\n", &Meta::default(), "x.md"), "The title");
        assert_eq!(title(Kind::Markdown, "no heading", &Meta::default(), "My note.md"), "My note");
        assert_eq!(title(Kind::Html, "<head><title> T &amp; U </title></head><h1>H</h1>", &Meta::default(), "p.html"), "T & U");
        assert_eq!(title(Kind::Html, "<h1 class=x>Status <b>green</b></h1>", &Meta::default(), "p.html"), "Status green");
        assert_eq!(title(Kind::Html, "<p>x</p>", &meta(&[("title", "  ")]), "page.html"), "page");
        assert_eq!(title(Kind::Text, "# not markdown", &Meta::default(), ".gitignore"), ".gitignore");
        for (name, kind) in [
            ("a.md", Kind::Markdown),
            ("a.MARKDOWN", Kind::Markdown),
            ("a.htm", Kind::Html),
            ("a.Html", Kind::Html),
            ("a.txt", Kind::Text),
            ("a.rs", Kind::Text),
            ("a.svg", Kind::Image),
            ("a.JPEG", Kind::Image),
            ("a.webp", Kind::Image),
            ("a.pdf", Kind::Other),
            ("Makefile", Kind::Other),
        ] {
            assert_eq!(Kind::of(name), kind, "{name}");
        }
        assert!(Kind::Html.is_text() && !Kind::Image.is_text() && !Kind::Folder.is_text());
        assert_eq!(safe_name("Release: v2 / \"final\"?", "x"), "Release- v2 - -final");
        assert_eq!(safe_name("  ..  ", "Group"), "Group");
        assert_eq!(safe_name("con", "x"), "_con");
        assert_eq!(safe_name(".hidden", "x"), "hidden");
        assert_eq!(safe_name(&"a".repeat(200), "x").len(), 80);
        assert_eq!(slug("Q3: plan / review!"), "q3-plan-review");
        assert_eq!(slug("  Été — Bilan  "), "été-bilan");
        assert_eq!(slug("???"), "");
        assert_eq!(slug(&"ab ".repeat(40)).len(), 59, "at most 60, no trailing dash");
        let root = std::env::temp_dir().join("nb");
        assert_eq!(group_folder(&root, "Team A"), root.join("Groups").join("Team A"));
        assert_eq!(root_of(Some("  "), &root), root.join("notebook"));
        assert_eq!(root_of(None, &root), root.join("notebook"));
        assert_eq!(root_of(Some(root.display().to_string().as_str()), Path::new("elsewhere")), root);
    }

    #[test]
    fn revisions_are_the_start_of_the_sha256() {
        assert_eq!(rev_of(b""), "e3b0c44298fc1c14");
        assert_eq!(rev_of(b"abc"), "ba7816bf8f01cfea");
        assert_ne!(rev_of(b"a"), rev_of(b"b"));
    }

    #[test]
    fn keys_no_line_can_carry_are_left_out_and_never_break_the_metadata() {
        // A key with `:` or a line end would make the whole block unreadable (or add lines).
        let text = "---\naudience: ai\n---\nBody\n";
        let out = set_frontmatter(text, &[("bad: key", Some("x")), ("a\nreviewed", Some("true")), ("author", Some("mixed")), (" padded", Some("y"))]);
        assert_eq!(out, "---\naudience: ai\nauthor: mixed\n---\nBody\n");
        assert_eq!(markdown_meta(&out).get("audience"), Some("ai"));
        assert_eq!(set_frontmatter("Body", &[("x:y", Some("1"))]), "Body", "nothing valid to set: unchanged");
        // Meta tags: a key cannot leave its attribute.
        let html = "<head></head>";
        assert_eq!(set_html_meta(html, &[("x\" onload=\"alert(1)", Some("v")), ("a<b", Some("v"))]), html);
        let out = set_html_meta(html, &[("my key", Some("v"))]);
        assert_eq!(out, "<head>\n<meta name=\"canvas-my key\" content=\"v\"></head>");
        assert_eq!(html_meta(&out).get("my key"), Some("v"));
        let extra = NewNote { title: "T".into(), extra: vec![("report:id".into(), "x".into()), ("report".into(), "0001".into())], ..Default::default() };
        let md = new_note_text(Kind::Markdown, "x", &extra);
        assert!(frontmatter(&md).is_some() && markdown_meta(&md).get("report") == Some("0001") && !md.contains("report:id"), "{md}");
    }

    #[test]
    fn titles_skip_indented_code_and_fences_until_a_long_enough_close() {
        assert_eq!(markdown_title("````\n```\n# not this\n````\n# Yes\n").as_deref(), Some("Yes"));
        assert_eq!(markdown_title("~~~\n```\n# not\n~~~\n# Tilde\n").as_deref(), Some("Tilde"));
        assert_eq!(markdown_title("```rust\n# not\n``` trailing\n# still code\n```\n# Done\n").as_deref(), Some("Done"));
        assert_eq!(markdown_title("    # indented code\n   # Three spaces\n").as_deref(), Some("Three spaces"));
        assert_eq!(markdown_title("    ```\n# Not a fence above\n").as_deref(), Some("Not a fence above"));
    }

    #[test]
    fn device_names_are_known_in_every_form() {
        for name in ["CON", "con", "nul.md", "Aux.tar.gz", "con .txt", "COM1", "com0", "LPT9.log", "COM¹", "lpt³.md", "CONIN$", "conout$.x"] {
            assert!(is_device_name(name), "{name}");
            assert!(safe_name(name, "x").starts_with('_'), "{name}");
        }
        for name in ["console", "COM10", "LPT", "COMA", "nul-1.md", "xcon", "CON-x", "", ".nul"] {
            assert!(!is_device_name(name), "{name}");
        }
    }

    #[test]
    fn paths_must_be_absolute_inside_a_root_and_outside_git() {
        let root = std::env::temp_dir().join("cc-notebook-root");
        let roots = vec![root.display().to_string()];
        let path = |rel: &str| root.join(rel).display().to_string();
        assert_eq!(writable(&path("a/b.md"), &roots).unwrap(), root.join("a").join("b.md"));
        assert!(writable("relative/x.md", &roots).unwrap_err().contains("not an absolute path"));
        assert!(writable("", &roots).is_err());
        let up = format!("{}{}..{}x.md", root.display(), std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR);
        assert!(writable(&up, &roots).unwrap_err().contains(".."));
        assert!(writable(&path(".git/config"), &roots).unwrap_err().contains(".git"));
        assert!(writable(&path("sub/.GIT/x"), &roots).is_err());
        assert!(writable(&std::env::temp_dir().join("elsewhere.md").display().to_string(), &roots).unwrap_err().contains("outside"));
        assert!(writable(&root.display().to_string(), &roots).is_err(), "never the root itself");
        assert!(writable(&path("x.md"), &[]).is_err(), "no roots: nothing");
        assert!(writable(&path("x.md"), &["relative".into()]).is_err(), "relative roots count for nothing");
        let sibling = format!("{}-2{}x.md", root.display(), std::path::MAIN_SEPARATOR);
        assert!(writable(&sibling, &roots).is_err(), "a sibling with the root's name as prefix is outside");
        assert_eq!(rel_path(&root.join("a").join("b.md"), &root).as_deref(), Some("a/b.md"));
        assert_eq!(rel_path(&root, &root).as_deref(), Some(""));
        assert_eq!(rel_path(&std::env::temp_dir(), &root), None);
        if cfg!(windows) {
            let upper = PathBuf::from(root.display().to_string().to_uppercase()).join("x.md");
            assert!(inside(&upper, &root), "case does not matter on Windows");
        }
    }

    #[test]
    fn markdown_becomes_html_with_note_and_todo_links() {
        let html = markdown_html("---\ntitle: T\naudience: ai\n---\n# Plan\n\nSee [[Other note]] and [[Design doc|the design]], then DEC-031 (and X-AB-1234). Not ABCDEF-123, DEC-03, xDEC-031 or DEC-0312x.\n\n`DEC-111` in code, [DEC-222](https://e.x) in a link.\n\n```\nDEC-333 [[In code]]\n```\n\n| a | b |\n|---|---|\n| DEC-444 | ~~old~~ |\n\n- [x] done\n- [ ] open\n\nNote[^1].\n\n[^1]: A footnote.\n");
        assert!(!html.contains("audience"), "the frontmatter is stripped: {html}");
        assert!(html.contains("<h1>Plan</h1>"));
        assert!(html.contains("<a href=\"#note:Other%20note\">Other note</a>"), "{html}");
        assert!(html.contains("<a href=\"#note:Design%20doc\">the design</a>"), "{html}");
        assert!(html.contains("<a href=\"#todo:DEC-031\">DEC-031</a>") && html.contains("<a href=\"#todo:AB-1234\">AB-1234</a>"), "{html}");
        for not in ["#todo:BCDEF", "#todo:DEC-03\"", "#todo:DEC-0312", "#todo:DEC-111", "#todo:DEC-222", "#todo:DEC-333", "#note:In"] {
            assert!(!html.contains(not), "{not} must not be linked: {html}");
        }
        assert!(html.contains("<code>DEC-111</code>") && html.contains("DEC-333 [[In code]]"));
        assert!(html.contains("<td><a href=\"#todo:DEC-444\">DEC-444</a></td>") && html.contains("<del>old</del>"), "{html}");
        assert!(html.contains("type=\"checkbox\" checked=\"\"") && html.contains("footnote"), "{html}");
        assert_eq!(note_links("[[A]] [[B|b]] [[A]] `[[C]]`"), ["A", "B"]);
        assert_eq!(todo_ids("DEC-001 and `DEC-002`, DEC-001, WI-019 <a href=\"#todo:X-1\">raw</a>"), ["DEC-001", "WI-019"]);
        assert_eq!(link_target("#note:Other%20note%23Part"), Some(LinkTarget::Note("Other note#Part".into())));
        assert_eq!(link_target("#note:%C3%A9t%C3%A9"), Some(LinkTarget::Note("été".into())));
        assert_eq!(link_target("#todo:DEC-031"), Some(LinkTarget::Todo("DEC-031".into())));
        assert_eq!(link_target("#todo:nope"), None);
        assert_eq!(link_target("https://example.com"), None);
        assert_eq!(link_target("#note:"), None);
        let accents = markdown_html("[[Été & co]]");
        assert!(accents.contains("href=\"#note:%C3%89t%C3%A9%20%26%20co\""), "{accents}");
        assert_eq!(link_target("#note:%C3%89t%C3%A9%20%26%20co"), Some(LinkTarget::Note("Été & co".into())));
        assert!(is_todo_id("WI-019") && !is_todo_id("WI-01") && !is_todo_id("wi-019") && !is_todo_id("DEC-031 "));
        assert_eq!(commonmark_html("[[x]] DEC-031"), "<p>[[x]] DEC-031</p>\n", "the plain conversion links nothing");
    }

    #[test]
    fn new_notes_carry_their_provenance() {
        let note = NewNote {
            title: "Investigation: flaky test".into(),
            audience: Audience::Ai,
            author: Author::Ai,
            agent: Some("opencode".into()),
            session: Some("s-1".into()),
            created: "2026-10-31T10:00:00.000Z".into(),
            extra: vec![("report".into(), "0001-x".into())],
        };
        let md = new_note_text(Kind::Markdown, "Findings.\n", &note);
        assert_eq!(
            md,
            "---\ntitle: \"Investigation: flaky test\"\naudience: ai\nauthor: ai\nagent: opencode\nsource_session: s-1\ncreated: 2026-10-31T10:00:00.000Z\nreport: 0001-x\n---\n# Investigation: flaky test\n\nFindings.\n"
        );
        let p = classify("Inbox/x.md", &markdown_meta(&md), Place::Notes);
        assert_eq!((p.audience, p.author, p.source, p.session.as_deref()), (Audience::Ai, Author::Ai, Source::Frontmatter, Some("s-1")));
        assert_eq!(title(Kind::Markdown, &md, &markdown_meta(&md), "x.md"), "Investigation: flaky test");
        // The body's own frontmatter: its other keys stay, ours replace the same keys; its heading stays.
        let own = new_note_text(Kind::Markdown, "---\ntags: [x]\naudience: human\n---\n# Own heading\nText\n", &NewNote { title: "T".into(), ..Default::default() });
        assert_eq!(own, "---\ntags: [x]\naudience: human\ntitle: T\nauthor: human\n---\n# Own heading\nText\n");
        assert_eq!(new_note_text(Kind::Markdown, "", &NewNote { title: "Empty".into(), ..Default::default() }), "---\ntitle: Empty\naudience: human\nauthor: human\n---\n# Empty\n");
        let html = new_note_text(Kind::Html, "<html><head><title>Report</title></head><body><p>x</p></body></html>", &note);
        let m = html_meta(&html);
        assert_eq!((m.get("audience"), m.get("author"), m.get("agent"), m.get("source_session"), m.get("report")), (Some("ai"), Some("ai"), Some("opencode"), Some("s-1"), Some("0001-x")));
        assert!(html.starts_with("<html><head>\n<meta name=\"canvas-title\" content=\"Investigation: flaky test\">"), "{html}");
        assert!(html.ends_with("<title>Report</title></head><body><p>x</p></body></html>"));
    }
}
