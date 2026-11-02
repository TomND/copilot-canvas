//! HTML notes served to a web view: the GUI serves an HTML note at
//! `https://notes.canvas.invalid/<path relative to its explorer root>` (bytes
//! read through the backend, so it works locally and remotely; relative
//! links resolve as on disk) and its relative images, style sheets and fonts
//! load from its own folder and the folders below it (nothing else is
//! served). Here: the virtual origin, the Content-Security-Policy every
//! response carries (one without scripts, one for notes allowed to run them:
//! sandboxed, never `allow-same-origin`), content types by extension, and the
//! note's page made safe ([`page`]).
//!
//! The page is sanitized as reports are ([`super::sanitize`]), except that
//! links, images, media and `<link rel="stylesheet">` may also point to the
//! notes origin ([`local_url`]: a relative reference without a scheme, host
//! or backslash, or an absolute URL of the origin). A whole document (it has
//! `<html>`, `<head>` or `<body>`) keeps the look its author gave it (browser
//! defaults plus its own styles; `<html>`/`<body>` attributes such as `class`
//! and `style` are kept); a fragment gets the notebook's reading look
//! ([`super::NOTEBOOK`]) with its own styles after it.

use super::{DROP, KEEP, Token, VOID, attribute_kept, escape_attr, escape_text, style_text, tokens, url_safe};

/// The virtual host notes are served from (`.invalid`: never a real one).
pub const HOST: &str = "notes.canvas.invalid";
/// `https://` + [`HOST`] (no trailing slash).
pub const ORIGIN: &str = "https://notes.canvas.invalid";

/// macOS serves notes from this scheme instead (a WKWebView answers only
/// schemes of its own, never `https:`): the same host and paths; responses
/// name it where [`ORIGIN`] stands (MAC-LINUX-PLAN D9).
pub const SCHEME: &str = "canvas-notes";
/// [`SCHEME`] + `://` + [`HOST`].
pub const SCHEME_ORIGIN: &str = "canvas-notes://notes.canvas.invalid";

/// The policy of a note without scripts (every response carries it).
pub const CSP: &str = "default-src 'none'; img-src https://notes.canvas.invalid data:; style-src 'unsafe-inline' https://notes.canvas.invalid; font-src data: https://notes.canvas.invalid; media-src https://notes.canvas.invalid data:; script-src 'none'; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'";

/// The policy of a note allowed to run its scripts: a sandbox without
/// `allow-same-origin` (an opaque origin: no storage, no cookies; nor
/// `allow-popups`, `allow-forms`, `allow-top-navigation`…), scripts only
/// inline or from the notes origin, nothing from the network, no frames,
/// plugins or workers (a worker's requests could pass by the view's request
/// filter on older runtimes).
pub const SCRIPTS_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' https://notes.canvas.invalid; style-src 'unsafe-inline' https://notes.canvas.invalid; img-src https://notes.canvas.invalid data: blob:; font-src data: https://notes.canvas.invalid; media-src https://notes.canvas.invalid data: blob:; connect-src 'none'; object-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; form-action 'none'; base-uri 'none'";

/// The policy for a note (`scripts`: it may run its own scripts).
pub fn csp(scripts: bool) -> &'static str {
    if scripts { SCRIPTS_CSP } else { CSP }
}

/// The `Content-Type` of a served file by its name's extension (unknown:
/// `application/octet-stream`, which nothing renders with `nosniff`).
pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "md" | "markdown" | "log" | "csv" | "tsv" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// `value` (a URL in an attribute) stays on the notes origin: a relative
/// reference (no scheme, not `//host`, no backslash, which browsers read as
/// `/`) or an absolute URL of [`ORIGIN`]. Fragments (`#x`) count too.
pub fn local_url(value: &str) -> bool {
    // Browsers drop tabs and newlines inside URLs: judge what they would read.
    let compact: String = value.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_control()).collect();
    if compact.contains('\\') {
        return false;
    }
    if compact.to_ascii_lowercase().starts_with(&format!("{ORIGIN}/")) {
        return true;
    }
    if compact.is_empty() || compact.starts_with("//") {
        return false;
    }
    let head = compact.split(['/', '?', '#']).next().unwrap_or_default();
    !head.contains(':')
}

/// Whether a note's attribute stays: reports' rules, plus links, images and
/// media into the note's folder.
fn kept(element: &str, name: &str, value: &str) -> bool {
    match name {
        "href" if element == "a" => url_safe(value, false) || local_url(value),
        "src" if matches!(element, "img" | "video" | "audio" | "source") => url_safe(value, true) || local_url(value),
        "src" => false,
        _ => attribute_kept(element, name, value),
    }
}

/// Media elements a note may show (their files come from its folder).
const MEDIA: &[&str] = &["video", "audio", "source"];

/// A note's HTML made safe: the body, the attributes of its `<html>` and
/// `<body>` (as markup), its `<title>` (escaped) and whether it was a whole
/// document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Clean {
    pub body: String,
    pub html_attrs: String,
    pub body_attrs: String,
    pub title: Option<String>,
    pub document: bool,
}

fn attrs_markup(element: &str, attrs: &[(String, String)]) -> String {
    attrs.iter().filter(|(k, v)| kept(element, k, v)).map(|(k, v)| format!(" {k}=\"{}\"", escape_attr(v))).collect()
}

/// Sanitizes a note (see the module docs).
pub fn sanitize(html: &str) -> Clean {
    let mut clean = Clean::default();
    let out = &mut clean.body;
    let mut dropping: Vec<String> = Vec::new();
    let mut in_style = false;
    let mut in_title = false;
    for token in tokens(html) {
        match token {
            Token::Start { name, attrs, self_closing } => {
                if !dropping.is_empty() {
                    if dropping.last() == Some(&name) && !self_closing {
                        dropping.push(name);
                    }
                    continue;
                }
                match name.as_str() {
                    "html" | "head" | "body" => {
                        clean.document = true;
                        let markup = attrs_markup(&name, &attrs);
                        match name.as_str() {
                            "html" if clean.html_attrs.is_empty() => clean.html_attrs = markup,
                            "body" if clean.body_attrs.is_empty() => clean.body_attrs = markup,
                            _ => {}
                        }
                        continue;
                    }
                    "title" if clean.title.is_none() && !self_closing => {
                        in_title = true;
                        clean.title = Some(String::new());
                        continue;
                    }
                    "link" => {
                        let rel = attrs.iter().any(|(k, v)| k == "rel" && v.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet")));
                        if let Some((_, href)) = attrs.iter().find(|(k, _)| k == "href")
                            && rel
                            && local_url(href)
                        {
                            out.push_str(&format!("<link rel=\"stylesheet\" href=\"{}\">", escape_attr(href)));
                        }
                        continue;
                    }
                    _ => {}
                }
                if DROP.contains(&name.as_str()) {
                    if !self_closing && !VOID.contains(&name.as_str()) {
                        dropping.push(name);
                    }
                    continue;
                }
                let media = MEDIA.contains(&name.as_str());
                if !KEEP.contains(&name.as_str()) && !media {
                    continue;
                }
                out.push('<');
                out.push_str(&name);
                out.push_str(&attrs_markup(&name, &attrs));
                if name == "input" {
                    out.push_str(" disabled");
                }
                if name == "a" && attrs.iter().any(|(k, v)| k == "href" && url_safe(v, false) && !v.trim_start().starts_with('#')) {
                    out.push_str(" rel=\"noopener noreferrer\"");
                }
                let void = VOID.contains(&name.as_str()) || name == "source";
                out.push_str(if self_closing && !void { "/>" } else { ">" });
                in_style = name == "style" && !self_closing;
            }
            Token::End(name) => {
                if in_title && name == "title" {
                    in_title = false;
                    continue;
                }
                if !dropping.is_empty() {
                    if dropping.last() == Some(&name) {
                        dropping.pop();
                    }
                    continue;
                }
                if (KEEP.contains(&name.as_str()) && !VOID.contains(&name.as_str())) || matches!(name.as_str(), "video" | "audio") {
                    out.push_str(&format!("</{name}>"));
                }
                if name == "style" {
                    in_style = false;
                }
            }
            Token::Text(text) if in_title => {
                if let Some(title) = clean.title.as_mut() {
                    title.push_str(&escape_text(text));
                }
            }
            Token::Text(text) if dropping.is_empty() => out.push_str(&if in_style { style_text(text) } else { escape_text(text) }),
            Token::Text(_) => {}
        }
    }
    clean
}

/// The page a web view shows for a note without scripts: the policy first
/// (a `<meta>` copy of the header's), its title, the reading look for a
/// fragment (none for a whole document), then the sanitized note.
pub fn page(html: &str) -> String {
    let clean = sanitize(html);
    let look = if clean.document { "" } else { super::NOTEBOOK.css };
    let title = clean.title.as_deref().map(|t| format!("<title>{t}</title>")).unwrap_or_default();
    let style = if look.is_empty() { String::new() } else { format!("<style>{look}</style>") };
    format!(
        "<!DOCTYPE html><html{}><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">{title}{style}</head><body{}>{}</body></html>",
        clean.html_attrs, clean.body_attrs, clean.body
    )
}

/// A short page in the reading look that says `message` (a note that could
/// not be read or shown).
pub fn message_page(message: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\"><style>{}</style></head><body><p>{}</p></body></html>",
        super::NOTEBOOK.css,
        escape_text(&message.replace('&', "&amp;"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_are_the_specs_and_scripts_get_the_sandbox() {
        assert_eq!(csp(false), CSP);
        assert_eq!(csp(true), SCRIPTS_CSP);
        assert!(CSP.contains("script-src 'none'") && CSP.contains("connect-src 'none'") && CSP.contains("frame-src 'none'") && !CSP.contains("sandbox"));
        assert!(SCRIPTS_CSP.starts_with("sandbox allow-scripts;") && !SCRIPTS_CSP.contains("allow-same-origin") && SCRIPTS_CSP.contains("connect-src 'none'"));
        // The scripts policy names frames, plugins and workers itself (a worker's
        // `worker-src` would fall back to `script-src`, which allows the origin).
        for directive in ["object-src 'none'", "frame-src 'none'", "child-src 'none'", "worker-src 'none'", "media-src https://notes.canvas.invalid data: blob:"] {
            assert!(SCRIPTS_CSP.contains(directive), "{directive}");
        }
        assert_eq!(SCRIPTS_CSP.split(';').next(), Some("sandbox allow-scripts"), "no other sandbox flag");
        for policy in [CSP, SCRIPTS_CSP] {
            assert!(policy.contains("default-src 'none'") && policy.contains("form-action 'none'") && policy.contains("base-uri 'none'"));
            // Only the notes origin, data: (and blob: for scripts) appear as sources.
            for source in policy.split(';').flat_map(|d| d.split_whitespace().skip(1)) {
                assert!(
                    matches!(source, "'none'" | "'unsafe-inline'" | "https://notes.canvas.invalid" | "data:" | "blob:" | "allow-scripts"),
                    "{source} in {policy}"
                );
            }
        }
        assert_eq!(ORIGIN, format!("https://{HOST}"));
    }

    #[test]
    fn content_types_follow_the_extension() {
        for (name, ct) in [
            ("a.HTML", "text/html; charset=utf-8"),
            ("x/y.htm", "text/html; charset=utf-8"),
            ("s.css", "text/css; charset=utf-8"),
            ("app.js", "text/javascript; charset=utf-8"),
            ("m.mjs", "text/javascript; charset=utf-8"),
            ("d.json", "application/json"),
            ("r.txt", "text/plain; charset=utf-8"),
            ("p.png", "image/png"),
            ("p.JPG", "image/jpeg"),
            ("p.jpeg", "image/jpeg"),
            ("p.gif", "image/gif"),
            ("p.svg", "image/svg+xml"),
            ("p.webp", "image/webp"),
            ("f.woff", "font/woff"),
            ("f.woff2", "font/woff2"),
            ("v.mp4", "video/mp4"),
            ("noext", "application/octet-stream"),
            ("x.exe", "application/octet-stream"),
        ] {
            assert_eq!(content_type(name), ct, "{name}");
        }
    }

    #[test]
    fn local_urls_stay_on_the_notes_origin() {
        for ok in ["pic.png", "img/pic.png", "./a.css", "../up.html", "/abs.png", "a%20b.png", "#part", "?q=1", "https://notes.canvas.invalid/x.png", "HTTPS://NOTES.CANVAS.INVALID/x", "page.html#note:X"] {
            assert!(local_url(ok), "{ok}");
        }
        for bad in [
            "https://evil.example/x.png",
            "http://notes.canvas.invalid/x",
            "https://notes.canvas.invalid.evil.com/x",
            "https://notes.canvas.invalid@evil.com/x",
            "//evil.example/x.png",
            "\\\\evil\\share\\x.png",
            "/\\evil.example/x",
            "javascript:alert(1)",
            "java\nscript:alert(1)",
            " javascript:x",
            "file:///C:/secret.txt",
            "C:/secret.txt",
            "data:text/html,x",
            "",
        ] {
            assert!(!local_url(bad), "{bad:?}");
        }
    }

    #[test]
    fn notes_keep_their_folder_links_and_lose_everything_active() {
        let html = r##"<!DOCTYPE html><html lang="en" class="dark"><head><title>My &amp; plan</title><meta http-equiv="refresh" content="0;url=https://evil"><base href="https://evil/">
<link rel="stylesheet" href="style.css"><link rel="stylesheet" href="https://evil/x.css"><link rel="icon" href="fav.png"><style>h1{background:url(bg.png)}</style>
<script src="app.js"></script></head><body class="wide" onload="x()" style="margin:0">
<h1>Plan</h1><img src="img/pic.png" alt="local"><img src="https://evil/b.png" alt="remote"><img src="//evil/c.png" alt="proto">
<a href="other.html">other</a> <a href="sub/page.md#x">md</a> <a href="#note:Other%20note">note</a> <a href="https://example.com/">ext</a> <a href="javascript:x()">js</a> <a href="\\evil\x">unc</a>
<video src="clip.mp4" controls autoplay onplay="x()"><source src="clip.webm" type="video/webm"><source src="https://evil/v.mp4"></video>
<iframe src="other.html"></iframe><object data="x.swf"></object><form action="https://evil"><input type="text"></form></body></html>"##;
        let clean = sanitize(html);
        assert!(clean.document);
        assert_eq!(clean.html_attrs, " lang=\"en\" class=\"dark\"");
        assert_eq!(clean.body_attrs, " class=\"wide\" style=\"margin:0\"");
        assert_eq!(clean.title.as_deref(), Some("My &amp; plan"));
        let body = &clean.body;
        for kept in [
            "<link rel=\"stylesheet\" href=\"style.css\">",
            "<style>h1{background:url(bg.png)}</style>",
            "<img src=\"img/pic.png\" alt=\"local\">",
            "<a href=\"other.html\">other</a>",
            "<a href=\"sub/page.md#x\">md</a>",
            "<a href=\"#note:Other%20note\">note</a>",
            "<a href=\"https://example.com/\" rel=\"noopener noreferrer\">ext</a>",
            "<video src=\"clip.mp4\" controls=\"\" autoplay=\"\"><source src=\"clip.webm\" type=\"video/webm\"><source></video>",
        ] {
            assert!(body.contains(kept), "{kept:?} is missing:\n{body}");
        }
        for gone in ["evil", "<script", "app.js", "onload", "onplay", "javascript", "<iframe", "<object", "<form", "<meta", "<base", "fav.png", "<title", "\\"] {
            assert!(!body.contains(gone), "{gone:?} survived:\n{body}");
        }
        assert!(body.contains("<img alt=\"remote\">") && body.contains("<img alt=\"proto\">"), "{body}");
        let page = page(html);
        assert!(page.starts_with("<!DOCTYPE html><html lang=\"en\" class=\"dark\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src https://notes.canvas.invalid"), "{page}");
        assert!(page.contains("<title>My &amp; plan</title></head><body class=\"wide\" style=\"margin:0\">"), "a whole document keeps its own look:\n{page}");
        assert!(!page.contains("max-width:80ch"), "{page}");
    }

    #[test]
    fn fragments_get_the_reading_look_and_messages_are_escaped() {
        let page = page("<h2>Just a fragment</h2><style>h2{color:red}</style><script>x()</script>");
        assert!(page.contains("max-width:80ch") && page.contains("script-src 'none'"), "{page}");
        let look_at = page.find("max-width:80ch").unwrap_or(usize::MAX);
        let own_at = page.find("h2{color:red}").unwrap_or(0);
        assert!(look_at < own_at, "the note's own styles come after the look: {page}");
        assert!(page.ends_with("<body><h2>Just a fragment</h2><style>h2{color:red}</style></body></html>"), "{page}");
        let message = message_page("C:\\notes\\<a>.html could not be read: x & y");
        assert!(message.contains("<p>C:\\notes\\&lt;a&gt;.html could not be read: x &amp; y</p>") && message.contains("script-src 'none'"), "{message}");
    }

    #[test]
    fn a_notes_style_inside_svg_cannot_carry_markup() {
        let clean = sanitize("<svg><style><img src=x onerror=alert(1)><iframe src=\"https://evil/\"></style></svg>");
        assert!(!clean.body.contains("<img") && !clean.body.contains("<iframe"), "{}", clean.body);
        assert_eq!(clean.body, "<svg><style>\\3C img src=x onerror=alert(1)>\\3C iframe src=\"https://evil/\"></style></svg>");
    }
}
