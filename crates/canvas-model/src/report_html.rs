//! Agent-authored HTML, handled as data: a tolerant tokenizer (the desktop
//! report viewer's formatted-text rendering reads its tokens) and the
//! sanitizer the web views get their document from (the GUI's WebView2, the
//! iPhone app's WKWebView). The sanitizer is defence in depth on top of each
//! view's own lock-down (scripts off, no network, navigation blocked): it
//! keeps formatting elements and their styling, drops scripts, frames,
//! plugins, forms' targets, `<base>` and `<meta>`, every event-handler
//! attribute and every URL that is not an `http(s)`/`mailto` link or an
//! inline `data:image` picture, and puts a strict Content-Security-Policy
//! first in the document.

/// One piece of an HTML document.
#[derive(Clone, Debug, PartialEq)]
pub enum Token<'a> {
    /// Text as written (entities not decoded).
    Text(&'a str),
    /// A start tag: lower-case name, attributes (lower-case names, decoded values).
    Start { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    End(String),
}

/// Elements whose content is raw text up to their end tag.
const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript"];

/// Splits HTML into tokens (comments, doctypes and processing instructions
/// are left out; a `<` that starts no tag is text).
pub fn tokens(html: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        let markup_end = if html[i..].starts_with("<!--") {
            Some(lower[i + 4..].find("-->").map(|e| i + 4 + e + 3).unwrap_or(bytes.len()))
        } else if next == b'!' || next == b'?' {
            Some(lower[i..].find('>').map(|e| i + e + 1).unwrap_or(bytes.len()))
        } else {
            None
        };
        if let Some(end) = markup_end {
            if text_start < i {
                out.push(Token::Text(&html[text_start..i]));
            }
            i = end;
            text_start = i;
            continue;
        }
        let closing = next == b'/';
        let name_at = if closing { i + 2 } else { i + 1 };
        if !bytes.get(name_at).is_some_and(|b| b.is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        if text_start < i {
            out.push(Token::Text(&html[text_start..i]));
        }
        let (token, end) = tag(html, name_at, closing);
        i = end;
        text_start = i;
        // Raw text up to the matching end tag.
        if let Token::Start { name, self_closing: false, .. } = &token
            && RAW.contains(&name.as_str())
        {
            let close = format!("</{name}");
            let stop = lower[i..].find(&close).map(|e| i + e).unwrap_or(bytes.len());
            let name = name.clone();
            out.push(token);
            if stop > i {
                out.push(Token::Text(&html[i..stop]));
            }
            out.push(Token::End(name));
            i = lower[stop..].find('>').map(|e| stop + e + 1).unwrap_or(bytes.len());
            text_start = i;
            continue;
        }
        out.push(token);
    }
    if text_start < bytes.len() {
        out.push(Token::Text(&html[text_start..]));
    }
    out
}

/// A tag whose name starts at `at`; answers it and where it ends.
fn tag(html: &str, at: usize, closing: bool) -> (Token<'_>, usize) {
    let bytes = html.as_bytes();
    let mut i = at;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/' {
        i += 1;
    }
    let name = html[at..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || (bytes[i] == b'/' && bytes.get(i + 1) != Some(&b'>'))) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'>' {
            i += 1;
            break;
        }
        if bytes[i] == b'/' {
            self_closing = true;
            i += 2;
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = html[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if bytes.get(i) == Some(&b'=') {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            match bytes.get(i) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let end = html[i + 1..].find(quote as char).map(|e| i + 1 + e).unwrap_or(bytes.len());
                    value = crate::reports::decode_entities(&html[i + 1..end]);
                    i = (end + 1).min(bytes.len());
                }
                _ => {
                    let start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                        i += 1;
                    }
                    value = crate::reports::decode_entities(&html[start..i]);
                }
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    let token = if closing { Token::End(name) } else { Token::Start { name, attrs, self_closing } };
    (token, i.min(bytes.len()))
}

/// Elements dropped together with their content.
const DROP: &[&str] = &[
    "script", "iframe", "frame", "frameset", "object", "embed", "applet", "noscript", "template", "title", "textarea", "select", "button", "noembed", "noframes", "xmp",
    "foreignobject", "portal", "dialog", "math",
];

/// Elements kept (with filtered attributes); anything else is left out
/// while its content stays.
const KEEP: &[&str] = &[
    "a", "abbr", "address", "article", "aside", "b", "bdi", "bdo", "blockquote", "br", "caption", "center", "cite", "code", "col", "colgroup", "dd", "del", "details", "dfn", "div", "dl",
    "dt", "em", "figcaption", "figure", "font", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "i", "img", "ins", "kbd", "label", "li", "main", "mark", "meter",
    "nav", "ol", "p", "picture", "pre", "progress", "q", "rp", "rt", "ruby", "s", "samp", "section", "small", "span", "strike", "strong", "style", "sub", "summary", "sup", "table", "tbody",
    "td", "tfoot", "th", "thead", "time", "tr", "tt", "u", "ul", "var", "wbr", "input", "svg", "g", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "text", "tspan",
    "defs", "lineargradient", "radialgradient", "stop", "clippath", "mask", "pattern", "symbol", "use", "marker", "desc",
];

/// Void elements (no end tag).
const VOID: &[&str] = &["br", "hr", "img", "input", "wbr", "col"];

/// Attributes that are never kept (they fetch, navigate or run something).
const DENY: &[&str] = &[
    "action", "formaction", "srcdoc", "srcset", "ping", "background", "poster", "http-equiv", "codebase", "manifest", "lowsrc", "dynsrc", "data", "autofocus", "contenteditable", "form",
];

fn url_safe(value: &str, image: bool) -> bool {
    let compact: String = value.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_control()).collect::<String>().to_ascii_lowercase();
    if image {
        compact.starts_with("data:image/")
    } else {
        compact.starts_with("http://") || compact.starts_with("https://") || compact.starts_with("mailto:") || compact.starts_with('#')
    }
}

fn attribute_kept(element: &str, name: &str, value: &str) -> bool {
    let well_formed = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    if !well_formed || name.starts_with("on") || DENY.contains(&name) {
        return false;
    }
    match name {
        "href" | "xlink:href" => url_safe(value, false) && (element == "a" || value.trim_start().starts_with('#')),
        "src" => element == "img" && url_safe(value, true),
        "style" => {
            let lower = value.to_ascii_lowercase();
            !lower.contains("javascript:") && !lower.contains("expression(") && !lower.contains("behavior:")
        }
        "type" if element == "input" => value.eq_ignore_ascii_case("checkbox"),
        "target" | "rel" => false,
        _ => true,
    }
}

fn escape_text(text: &str) -> String {
    text.replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The report's HTML with only formatting left (see the module docs).
pub fn sanitize(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    // Elements being dropped with their content (nesting counted per name).
    let mut dropping: Vec<String> = Vec::new();
    let mut in_style = false;
    for token in tokens(html) {
        match token {
            Token::Start { name, attrs, self_closing } => {
                if !dropping.is_empty() {
                    if dropping.last() == Some(&name) && !self_closing {
                        dropping.push(name);
                    }
                    continue;
                }
                if DROP.contains(&name.as_str()) {
                    if !self_closing && !VOID.contains(&name.as_str()) {
                        dropping.push(name);
                    }
                    continue;
                }
                if !KEEP.contains(&name.as_str()) {
                    continue;
                }
                out.push('<');
                out.push_str(&name);
                for (key, value) in attrs.iter().filter(|(k, v)| attribute_kept(&name, k, v)) {
                    out.push_str(&format!(" {key}=\"{}\"", escape_attr(value)));
                }
                if name == "input" {
                    // A task list's checkbox: shown, never editable.
                    out.push_str(" disabled");
                }
                if name == "a" && attrs.iter().any(|(k, v)| k == "href" && url_safe(v, false) && !v.trim_start().starts_with('#')) {
                    out.push_str(" rel=\"noopener noreferrer\"");
                }
                out.push_str(if self_closing && !VOID.contains(&name.as_str()) { "/>" } else { ">" });
                in_style = name == "style" && !self_closing;
            }
            Token::End(name) => {
                if !dropping.is_empty() {
                    if dropping.last() == Some(&name) {
                        dropping.pop();
                    }
                    continue;
                }
                if KEEP.contains(&name.as_str()) && !VOID.contains(&name.as_str()) {
                    out.push_str(&format!("</{name}>"));
                }
                if name == "style" {
                    in_style = false;
                }
            }
            Token::Text(text) if dropping.is_empty() => {
                if in_style {
                    // CSS as written (the tokenizer ended it at its end tag).
                    out.push_str(text);
                } else {
                    out.push_str(&escape_text(text));
                }
            }
            Token::Text(_) => {}
        }
    }
    out
}

/// The Content-Security-Policy every report document carries: nothing may
/// load or run; inline styles and `data:` images and fonts are allowed.
pub const CSP: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:; media-src data:; script-src 'none'; object-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'";

/// The look a report page starts from: extra `<head>` markup (a viewport)
/// and default CSS. The report's own styles come after it and win.
pub struct Look {
    pub head: &'static str,
    pub css: &'static str,
}

/// The desktop GUI's look (the canvas's colours, Windows fonts).
pub const DESKTOP: Look = Look {
    head: "",
    css: ":root{color-scheme:dark}body{margin:14px 18px;font:14px/1.5 'Segoe UI',sans-serif;color:#e6edf3;background:#0d1117}\
          a{color:#58a6ff}pre,code{font-family:'Cascadia Mono',Consolas,monospace;background:#161b22}pre{padding:8px;overflow:auto}\
          table{border-collapse:collapse}td,th{border:1px solid #30363d;padding:4px 8px}img{max-width:100%}",
};

/// A page for a web view: the policy and `look` first, then the sanitized
/// report (its own styles win over the defaults).
pub fn document(html: &str, look: &Look) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">{}<style>{}</style></head><body>{}</body></html>",
        look.head,
        look.css,
        sanitize(html)
    )
}

/// The page the GUI shows ([`DESKTOP`]).
pub fn safe_document(html: &str) -> String {
    document(html, &DESKTOP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tokenizer_reads_tags_attributes_raw_text_and_stray_brackets() {
        let html = "<!DOCTYPE html><p class=\"a b\" data-x='1' hidden>x < y &amp; z</p><!-- c --><script>if (a<b) {}</script><br/>";
        let tokens = tokens(html);
        assert_eq!(tokens[0], Token::Start { name: "p".into(), attrs: vec![("class".into(), "a b".into()), ("data-x".into(), "1".into()), ("hidden".into(), String::new())], self_closing: false });
        assert_eq!(tokens[1], Token::Text("x < y &amp; z"));
        assert_eq!(tokens[2], Token::End("p".into()));
        assert_eq!(tokens[4], Token::Text("if (a<b) {}"), "a script's text is raw");
        assert_eq!(tokens[5], Token::End("script".into()));
        assert_eq!(tokens[6], Token::Start { name: "br".into(), attrs: vec![], self_closing: true });
        assert_eq!(super::tokens("<A HREF=x>t</A>")[0], Token::Start { name: "a".into(), attrs: vec![("href".into(), "x".into())], self_closing: false });
        assert_eq!(super::tokens("unclosed <b"), vec![Token::Text("unclosed "), Token::Start { name: "b".into(), attrs: vec![], self_closing: false }]);
    }

    #[test]
    fn scripts_handlers_frames_and_foreign_urls_are_removed_formatting_stays() {
        let dirty = r##"<html><head><title>T</title><meta http-equiv="refresh" content="0;url=https://evil"><base href="https://evil/"><link rel=stylesheet href="https://evil/x.css">
<style>h1{color:#f0f}</style></head><body onload="steal()">
<h1 style="color:red" onclick="x()">Brief</h1><p>Some <b>bold</b> &amp; <i>italic</i> text.</p>
<script>document.title='pwned'</script><SCRIPT src="https://evil/x.js"></SCRIPT>
<img src="https://evil/beacon.png" onerror="steal()" alt="beacon"><img src="data:image/png;base64,AAAA" alt="ok">
<a href="javascript:steal()">js</a> <a href=" JaVaScRiPt:steal()">js2</a> <a href="https://example.com/pr/1" target="_blank">PR</a> <a href="file:///C:/secrets.txt">file</a>
<iframe src="https://evil"><p>inner</p></iframe><object data="x.swf"></object><form action="https://evil"><input type="text" name="q"><input type="checkbox" checked><button>Go</button></form>
<svg onload="x()"><script>x()</script><foreignObject><p>html in svg</p></foreignObject><rect width="10" height="10" fill="#0f0"/></svg>
<table><tr><th>A</th><td style="background:url(javascript:x)">1</td></tr></table></body></html>"##;
        let clean = sanitize(dirty);
        for gone in ["<script", "steal", "pwned", "evil", "<iframe", "inner", "<object", "<form", "<meta", "<base", "<link", "javascript", "file:", "onclick", "onload", "onerror", "target=", "<button", "Go<", "html in svg", "<title"] {
            assert!(!clean.to_lowercase().contains(&gone.to_lowercase()), "{gone:?} survived:\n{clean}");
        }
        for kept in ["<style>h1{color:#f0f}</style>", "<h1 style=\"color:red\">Brief</h1>", "<b>bold</b> &amp; <i>italic</i>", "src=\"data:image/png;base64,AAAA\"", "<a href=\"https://example.com/pr/1\" rel=\"noopener noreferrer\">PR</a>", "<input type=\"checkbox\" checked=\"\" disabled>", "<rect width=\"10\" height=\"10\" fill=\"#0f0\"/>", "<th>A</th>"] {
            assert!(clean.contains(kept), "{kept:?} is missing:\n{clean}");
        }
        assert!(clean.contains("<img alt=\"beacon\">"), "an image from the network loses its address:\n{clean}");
        assert!(!clean.contains("<td style"), "a style with a script URL goes:\n{clean}");
        let doc = safe_document("<p>x</p>");
        assert!(doc.starts_with("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none';"));
        assert!(doc.contains("script-src 'none'") && doc.contains("connect-src 'none'") && doc.ends_with("<body><p>x</p></body></html>"));
        assert!(doc.contains("<style>:root{color-scheme:dark}body{margin:14px 18px;font:14px/1.5 'Segoe UI'"), "the desktop look:\n{doc}");
        let phone = document("<script>x()</script><p>y</p>", &Look { head: "<meta name=\"viewport\" content=\"width=device-width\">", css: "body{margin:0}" });
        assert!(phone.contains("content=\"default-src 'none';") && phone.contains("<meta name=\"viewport\" content=\"width=device-width\"><style>body{margin:0}</style>"));
        assert!(phone.ends_with("<body><p>y</p></body></html>"), "sanitized for every look:\n{phone}");
    }

    #[test]
    fn escaped_text_cannot_open_tags_again() {
        assert_eq!(sanitize("a < b > c <scr<script>ipt>alert(1)</script>"), "a &lt; b &gt; c ipt&gt;alert(1)");
        assert_eq!(sanitize("<p title='\"><script>x</script>'>t</p>"), "<p title=\"&quot;&gt;&lt;script&gt;x&lt;/script&gt;\">t</p>");
        assert_eq!(sanitize("<div><div>nested</div></div><style>p{}</style>"), "<div><div>nested</div></div><style>p{}</style>");
        // Nested dropped elements of the same name stay dropped to their own end.
        assert_eq!(sanitize("<select><select>x</select>y</select>z"), "z");
    }
}
