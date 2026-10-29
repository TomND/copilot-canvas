//! Reports and short messages agents publish to the canvas inbox
//! (`copilot-canvas report publish`): Markdown or HTML documents and plain
//! messages, with a small structured part (kind, related pull request and
//! session, links, thread). The CLI spools a [`Submission`]; the service
//! checks it again, stores it under `<home>/reports/items/<id>/`, keeps the
//! read state and tells every client ([`Snapshot`]). Front ends fetch a
//! report's body on demand ([`Content`]).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Largest body (Markdown or HTML, images inline as `data:` URIs included).
pub const MAX_BODY: usize = 2 << 20;
/// Longest short message (characters).
pub const MAX_MESSAGE: usize = 4_000;
/// Longest title (characters; longer ones are cut).
pub const MAX_TITLE: usize = 160;
pub const MAX_LINKS: usize = 20;
pub const MAX_URL: usize = 2_048;
/// The inbox refuses new reports while this many are unread.
pub const MAX_UNREAD: usize = 1_000;
/// Read reports kept at most, and for at most this many days.
pub const KEEP_READ: usize = 500;
pub const KEEP_READ_DAYS: u64 = 30;
/// Reports a snapshot carries at most (the newest).
pub const SNAPSHOT_ITEMS: usize = 600;
/// Characters of the preview line.
pub const PREVIEW: usize = 200;
/// The spool file format.
pub const SUBMISSION_VERSION: u32 = 1;

/// How a report's body is written.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// A short plain-text message (no document).
    #[default]
    Message,
    Markdown,
    Html,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Message => "message",
            Format::Markdown => "Markdown",
            Format::Html => "HTML",
        }
    }

    /// The body's file name in the store.
    pub fn file(self) -> &'static str {
        match self {
            Format::Message => "body.txt",
            Format::Markdown => "body.md",
            Format::Html => "body.html",
        }
    }
}

/// A report's kind: (name, glyph). `queued` and `delivered` tell intended
/// from confirmed dispatches (a lightweight update of a monitoring agent).
pub const KINDS: [(&str, &str); 7] = [("info", "◇"), ("ok", "✓"), ("warn", "⚠"), ("alert", "▲"), ("queued", "◌"), ("delivered", "✓"), ("failed", "✗")];

/// The glyph of a kind (unknown kinds from a newer producer: `◇`).
pub fn kind_glyph(kind: &str) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, g)| *g).unwrap_or("◇")
}

pub fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.iter().any(|(k, _)| *k == kind) {
        bail!("--kind is one of {} (not {kind:?})", KINDS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub title: String,
    pub url: String,
}

/// What `copilot-canvas report publish` spools for the service
/// (`<home>/reports/spool/<id>.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Submission {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub format: Format,
    #[serde(default)]
    pub kind: String,
    pub body: String,
    pub created_ms: u64,
    /// The publishing canvas session (`COPILOT_CANVAS_SESSION`; empty outside the canvas).
    #[serde(default)]
    pub source: String,
    /// `--source`: who published it when no canvas session did (or a fallback name).
    #[serde(default)]
    pub source_name: String,
    /// The passive session's key (`COPILOT_CANVAS_PASSIVE`) when a passive run published it.
    #[serde(default)]
    pub passive: String,
    /// A related session on the canvas (`--session`: a name or box id).
    #[serde(default)]
    pub session: String,
    /// A related pull request (`--pr`).
    #[serde(default)]
    pub pr: String,
    #[serde(default)]
    pub links: Vec<Link>,
    /// Reports with the same thread key show as one inbox row (`--thread`).
    #[serde(default)]
    pub thread: String,
}

/// A stored report (`meta.json`) with its read state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub id: String,
    pub title: String,
    pub format: Format,
    pub kind: String,
    /// When it was published (the producer's clock) and stored (the service's).
    pub created_ms: u64,
    pub stored_ms: u64,
    /// The publishing box (empty: outside the canvas).
    #[serde(default)]
    pub source: String,
    /// Its name when it was stored (the box may be gone since).
    #[serde(default)]
    pub source_name: String,
    /// `session`, `passive` or `external`.
    #[serde(default)]
    pub source_kind: String,
    /// The publisher's group (id and name) when it is in one.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub group_name: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub pr: String,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub thread: String,
    /// The start of its text, one line.
    #[serde(default)]
    pub preview: String,
    /// Body bytes.
    #[serde(default)]
    pub size: u64,
    /// When it was read (0: unread).
    #[serde(default)]
    pub read_ms: u64,
}

impl Report {
    pub fn unread(&self) -> bool {
        self.read_ms == 0
    }
}

/// A report the service refused after it was queued (the producer had
/// already been told "queued"): shown in the inbox and by `report list`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Rejected {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub ms: u64,
}

/// The inbox as clients see it (`ServiceMsg::Reports`, `Request::Reports`):
/// the newest reports first, with their read state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Bumped on every change.
    pub version: u64,
    pub reports: Vec<Report>,
    /// Stored reports in all (`reports` holds at most [`SNAPSHOT_ITEMS`]).
    pub total: u32,
    pub unread: u32,
    #[serde(default)]
    pub rejected: Vec<Rejected>,
}

/// One report's body (`Request::ReportContent`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Content {
    pub id: String,
    pub format: Format,
    pub body: String,
}

/// One line of text: control characters removed, white space collapsed,
/// at most `max` characters (cut with `…`).
pub fn clean_line(text: &str, max: usize) -> String {
    let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
    let line = words.join(" ");
    // Bidirectional overrides would let a title show other text than it holds.
    let line: String = line.chars().filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')).collect();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max.saturating_sub(1)).collect::<String>().trim_end())
    } else {
        line
    }
}

/// A title as shown and stored.
pub fn clean_title(title: &str) -> String {
    clean_line(title, MAX_TITLE)
}

/// A link a report may carry: `http(s)://host…` or `mailto:`, no spaces or
/// control characters.
pub fn check_url(url: &str) -> Result<()> {
    if url.len() > MAX_URL {
        bail!("a link is longer than {MAX_URL} characters");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("a link has spaces or control characters: {url:?}");
    }
    let lower = url.to_ascii_lowercase();
    let host = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
    match host {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => Ok(()),
        _ if lower.starts_with("mailto:") && lower.len() > 7 => Ok(()),
        _ => bail!("links are http(s):// or mailto: addresses, not {url:?}"),
    }
}

/// `--link TITLE=URL`.
pub fn parse_link(text: &str) -> Result<Link> {
    let Some((title, url)) = text.split_once('=') else { bail!("--link takes TITLE=URL, not {text:?}") };
    let url = url.trim();
    check_url(url).map_err(|e| anyhow::anyhow!("--link {text:?}: {e}"))?;
    let title = clean_line(title, 80);
    Ok(Link { title: if title.is_empty() { url.to_owned() } else { title }, url: url.to_owned() })
}

/// A thread key: 1–120 of letters, digits and `._:/#-`.
pub fn check_thread(thread: &str) -> Result<()> {
    if thread.is_empty() || thread.len() > 120 || !thread.chars().all(|c| c.is_ascii_alphanumeric() || "._:/#-".contains(c)) {
        bail!("--thread takes 1-120 letters, digits or . _ : / # - (for example pr-1234-comments), not {thread:?}");
    }
    Ok(())
}

/// A report id: `<ms>-<8 hex>` (sortable by time, safe as a folder name).
pub fn new_id(now_ms: u64) -> String {
    format!("{now_ms:013}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !id.starts_with('-')
}

impl Submission {
    /// Every rule a report must follow (the CLI checks before it spools, the
    /// service again before it stores).
    pub fn validate(&self) -> Result<()> {
        if self.version != SUBMISSION_VERSION {
            bail!("unknown report format version {} (this canvas reads {SUBMISSION_VERSION}; update it)", self.version);
        }
        if !valid_id(&self.id) {
            bail!("invalid report id {:?}", self.id);
        }
        if clean_title(&self.title).is_empty() {
            bail!("a report needs a --title (or a --message to take it from)");
        }
        check_kind(&self.kind)?;
        if self.body.contains('\0') {
            bail!("the report holds NUL characters (is it a binary file?)");
        }
        match self.format {
            Format::Message => {
                if self.body.trim().is_empty() {
                    bail!("the message is empty");
                }
                let chars = self.body.chars().count();
                if chars > MAX_MESSAGE {
                    bail!("the message has {chars} characters; at most {MAX_MESSAGE} (publish a --markdown report for more)");
                }
            }
            Format::Markdown | Format::Html => {
                if self.body.trim().is_empty() {
                    bail!("the report is empty");
                }
                if self.body.len() > MAX_BODY {
                    bail!(
                        "the report is {:.1} MiB; at most {} MiB (images count: inline them small, or link to large files)",
                        self.body.len() as f64 / (1 << 20) as f64,
                        MAX_BODY >> 20
                    );
                }
            }
        }
        if self.links.len() > MAX_LINKS {
            bail!("a report carries at most {MAX_LINKS} links ({} given)", self.links.len());
        }
        for link in &self.links {
            check_url(&link.url)?;
        }
        if !self.pr.is_empty() {
            check_url(&self.pr).map_err(|e| anyhow::anyhow!("--pr: {e}"))?;
        }
        if !self.thread.is_empty() {
            check_thread(&self.thread)?;
        }
        if self.session.chars().count() > 200 || self.source_name.chars().count() > 200 || self.source.len() > 200 || self.passive.len() > 200 {
            bail!("--session and --source take at most 200 characters");
        }
        Ok(())
    }
}

/// The start of a body's text as one line (Markdown and HTML markup left out).
pub fn preview(format: Format, body: &str) -> String {
    let text = match format {
        Format::Message => body.to_owned(),
        Format::Html => strip_html(&body[..floor(body, 64 * 1024)]),
        Format::Markdown => strip_markdown(&body[..floor(body, 16 * 1024)]),
    };
    clean_line(&text, PREVIEW)
}

/// The largest char boundary at or below `at`.
fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn strip_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let line = trimmed.trim_start_matches(['#', '>', ' ']).trim_start_matches(['-', '*', '+']).trim_start();
        if line.chars().all(|c| matches!(c, '-' | '=' | '|' | ':' | ' ' | '*' | '_')) {
            continue;
        }
        // [text](url) → text; emphasis and code marks go.
        let mut chars = line.chars().peekable();
        let mut plain = String::new();
        while let Some(c) = chars.next() {
            match c {
                '*' | '_' | '`' | '~' | '[' => {}
                '!' if chars.peek() == Some(&'[') => {}
                ']' if chars.peek() == Some(&'(') => {
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
                '|' => plain.push(' '),
                c => plain.push(c),
            }
        }
        out.push_str(&plain);
        out.push(' ');
    }
    out
}

fn strip_html(text: &str) -> String {
    let mut out = String::new();
    let lower = text.to_ascii_lowercase();
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Raw-text elements are left out with their content.
            let named = |tag: &str| lower[i + 1..].starts_with(tag) && lower[i + 1 + tag.len()..].starts_with(|c: char| c == '>' || c == '/' || c.is_ascii_whitespace());
            let skip = ["script", "style", "head", "title", "template", "noscript"].into_iter().find(|tag| named(tag));
            let end = match skip {
                Some(tag) => lower[i..].find(&format!("</{tag}")).and_then(|e| lower[i + e..].find('>').map(|g| i + e + g + 1)),
                None => lower[i..].find('>').map(|g| i + g + 1),
            };
            i = end.unwrap_or(bytes.len());
            out.push(' ');
            continue;
        }
        let next = text[i..].find('<').map(|n| i + n).unwrap_or(text.len());
        out.push_str(&decode_entities(&text[i..next]));
        i = next;
    }
    out
}

/// `&amp;`, `&#39;`, `&#x27;` and the common named entities.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|e| e + 1);
        let decoded = end.filter(|e| rest[*e..].starts_with(';')).and_then(|e| entity(&rest[1..e]).map(|c| (c, e + 1)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "divide" => '÷',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "deg" => '°',
        "plusmn" => '±',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "check" => '✓',
        "laquo" => '«',
        "raquo" => '»',
        "sect" => '§',
        "para" => '¶',
        "zwj" => '\u{200d}',
        "zwnj" => '\u{200c}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submission(format: Format, body: &str) -> Submission {
        Submission { version: SUBMISSION_VERSION, id: new_id(1_700_000_000_000), title: "Morning brief".into(), format, kind: "info".into(), body: body.into(), ..Default::default() }
    }

    #[test]
    fn submissions_are_checked_before_they_are_spooled_and_again_when_stored() {
        assert!(submission(Format::Markdown, "# Brief\nAll green.").validate().is_ok());
        assert!(submission(Format::Message, "PR 12 got 3 comments").validate().is_ok());
        let error = |s: Submission| s.validate().unwrap_err().to_string();
        assert!(error(submission(Format::Markdown, "  ")).contains("empty"));
        assert!(error(submission(Format::Message, &"x".repeat(MAX_MESSAGE + 1))).contains("at most 4000"));
        assert!(error(submission(Format::Html, &"x".repeat(MAX_BODY + 1))).contains("at most 2 MiB"));
        assert!(error(submission(Format::Markdown, "a\0b")).contains("NUL"));
        assert!(error(Submission { title: " \n\t".into(), ..submission(Format::Markdown, "x") }).contains("--title"));
        assert!(error(Submission { kind: "urgent".into(), ..submission(Format::Markdown, "x") }).contains("info, ok, warn, alert, queued, delivered, failed"));
        assert!(error(Submission { pr: "javascript:alert(1)".into(), ..submission(Format::Markdown, "x") }).contains("--pr"));
        assert!(error(Submission { id: "../evil".into(), ..submission(Format::Markdown, "x") }).contains("invalid report id"));
        assert!(error(Submission { version: 2, ..submission(Format::Markdown, "x") }).contains("version 2"));
        assert!(error(Submission { thread: "a b".into(), ..submission(Format::Markdown, "x") }).contains("--thread"));
        let links = vec![Link { title: "x".into(), url: "https://example.com".into() }; MAX_LINKS + 1];
        assert!(error(Submission { links, ..submission(Format::Markdown, "x") }).contains("at most 20 links"));
    }

    #[test]
    fn titles_links_and_threads_are_sanitized() {
        assert_eq!(clean_title("  Morning\tbrief\n\x1b[31mred\u{202e}txt "), "Morning brief [31mredtxt");
        let long = clean_title(&"word ".repeat(100));
        assert_eq!(long.chars().count(), MAX_TITLE);
        assert!(long.ends_with('…'));
        assert_eq!(parse_link("PR 12=https://github.com/o/r/pull/12").unwrap(), Link { title: "PR 12".into(), url: "https://github.com/o/r/pull/12".into() });
        assert_eq!(parse_link("=https://example.com").unwrap().title, "https://example.com");
        for bad in ["Title", "x=file:///C:/secret.txt", "x=javascript:alert(1)", "x=https://", "x=https:///path", "x=https://a b"] {
            assert!(parse_link(bad).is_err(), "{bad}");
        }
        assert!(check_url("mailto:me@example.com").is_ok() && check_url("HTTPS://Example.com/a?b=c#d").is_ok());
        assert!(check_thread("pr-1234/comments#2").is_ok() && check_thread("").is_err());
        assert!(valid_id(&new_id(5)) && !valid_id("a/b") && !valid_id(""));
        assert_eq!(kind_glyph("failed"), "✗");
        assert_eq!(kind_glyph("future"), "◇");
    }

    #[test]
    fn previews_leave_markup_out() {
        assert_eq!(preview(Format::Markdown, "# Morning brief\n\n**3** PRs need you: see [the list](https://x).\n\n```\ncode\n```\n| a | b |\n|---|---|"), "Morning brief 3 PRs need you: see the list. a b");
        assert_eq!(preview(Format::Html, "<html><head><title>T</title><style>p{}</style></head><body><h1>Triage</h1><script>alert(1)</script><p>5 &amp; 6&nbsp;&#x2713;</p></body></html>"), "Triage 5 & 6 ✓");
        assert_eq!(preview(Format::Html, "<header>Top</header><p>a<br>b</p>"), "Top a b");
        assert_eq!(preview(Format::Message, "line one\nline two"), "line one line two");
        assert_eq!(decode_entities("&lt;b&gt; &unknown; &#0; &"), "<b> &unknown; &#0; &");
    }

    #[test]
    fn snapshots_and_contents_travel_as_json() {
        let report = Report { id: new_id(9), title: "T".into(), format: Format::Html, kind: "queued".into(), links: vec![Link { title: "a".into(), url: "https://a".into() }], ..Default::default() };
        let snapshot = Snapshot { version: 3, reports: vec![report], total: 1, unread: 1, rejected: vec![] };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"format\":\"html\""));
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), snapshot);
        // A producer of a newer version may add fields; older fields may be missing.
        let old: Report = serde_json::from_str(r#"{"id":"1-a","title":"t","format":"markdown","kind":"info","created_ms":1,"stored_ms":2,"extra":true}"#).unwrap();
        assert!(old.unread() && old.links.is_empty());
    }
}
