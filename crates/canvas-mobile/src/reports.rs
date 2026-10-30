//! The reports inbox on the phone (the desktop's `canvas-core` reports, for
//! a list and a web view): the service's inbox with this phone's own changes
//! applied until the service confirms them, reports grouped by thread, and
//! the page a report is shown as.
//!
//! Every format becomes one sanitized HTML page ([`page`]): HTML reports as
//! the desktop GUI shows them (`canvas_model::report_html`: an allow-list of
//! formatting, no scripts, no URLs but links and inline pictures, a strict
//! Content-Security-Policy), Markdown rendered first, messages as
//! paragraphs. The app's web view is locked down on top (scripts off, no
//! network, navigation blocked: `ReportWebView.swift`).

use std::collections::HashSet;

use canvas_model::doc::{Doc, Node};
use canvas_model::report_html::{self, Look};
use canvas_model::reports::{Content, Format, Report, Snapshot, check_url, kind_glyph};

use crate::model::{RefusedReport, ReportFormat, ReportItem, ReportLink, ReportPage, ReportThread, ReportsOverview};

/// The phone's look: the canvas's colours, the system font at the reader's
/// text size, a viewport (pinch to zoom stays).
pub const PHONE: Look = Look {
    head: "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
    css: ":root{color-scheme:dark}html{-webkit-text-size-adjust:100%}\
          body{margin:14px 16px 40px;font:-apple-system-body;line-height:1.45;color:#e6edf3;background:#0d1117;overflow-wrap:anywhere}\
          a{color:#58a6ff}h1{font-size:1.4em;line-height:1.25}h2{font-size:1.2em}h3{font-size:1.05em}\
          pre,code{font-family:ui-monospace,Menlo,monospace;font-size:.85em;background:#161b22;border-radius:4px}\
          code{padding:1px 4px}pre{padding:10px;overflow-x:auto}pre code{padding:0;background:none}\
          blockquote{margin:0 0 0 2px;padding-left:12px;border-left:3px solid #30363d;color:#8b949e}\
          table{border-collapse:collapse;display:block;overflow-x:auto;max-width:100%}td,th{border:1px solid #30363d;padding:4px 8px}\
          img{max-width:100%;height:auto}svg{max-width:100%}hr{border:none;border-top:1px solid #30363d}",
};

/// The inbox as the service last sent it, and what this phone changed since.
#[derive(Default)]
pub struct Inbox {
    pub snapshot: Option<Snapshot>,
    /// Reports already announced (or there before the first snapshot).
    seen: HashSet<String>,
    read_here: HashSet<String>,
    unread_here: HashSet<String>,
    deleted_here: HashSet<String>,
}

/// New unread reports in a snapshot (the newest first).
pub struct Arrived {
    pub newest: Report,
    pub more: u32,
}

impl Inbox {
    /// A snapshot from the service; the reports that are new since the last
    /// one (nothing for the first: those were there before this phone looked).
    pub fn update(&mut self, snapshot: Snapshot) -> Option<Arrived> {
        // Changes made here that the service has applied now.
        self.read_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id && r.read_ms == 0));
        self.unread_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id && r.read_ms != 0));
        self.deleted_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id));
        let primed = self.snapshot.is_some();
        let fresh: Vec<Report> = snapshot.reports.iter().filter(|r| r.unread() && !self.seen.contains(&r.id)).cloned().collect();
        self.seen.extend(snapshot.reports.iter().map(|r| r.id.clone()));
        self.snapshot = Some(snapshot);
        let mut fresh = fresh.into_iter();
        let newest = fresh.next().filter(|_| primed)?;
        Some(Arrived { newest, more: fresh.count() as u32 })
    }

    /// The reports as this phone shows them, newest first.
    pub fn reports(&self) -> Vec<Report> {
        let Some(snapshot) = &self.snapshot else { return Vec::new() };
        snapshot
            .reports
            .iter()
            .filter(|r| !self.deleted_here.contains(&r.id))
            .map(|r| {
                let mut r = r.clone();
                if self.read_here.contains(&r.id) && r.read_ms == 0 {
                    r.read_ms = 1;
                }
                if self.unread_here.contains(&r.id) {
                    r.read_ms = 0;
                }
                r
            })
            .collect()
    }

    /// Changes read state or deletes here at once; the bus request for the
    /// service (which tells every window and phone).
    pub fn request(&mut self, op: &str, ids: &[String]) -> String {
        for id in ids {
            match op {
                "read" => {
                    self.unread_here.remove(id);
                    self.read_here.insert(id.clone());
                }
                "unread" => {
                    self.read_here.remove(id);
                    self.unread_here.insert(id.clone());
                }
                "delete" => {
                    self.deleted_here.insert(id.clone());
                }
                _ => {}
            }
        }
        if op == "read_all" {
            for report in self.reports() {
                self.read_here.insert(report.id);
            }
            self.unread_here.clear();
        }
        serde_json::json!({ "action": "reports", "op": op, "ids": ids }).to_string()
    }

    /// The inbox's rows: reports grouped by thread, the newest thread first.
    pub fn overview(&self, doc: &Doc) -> ReportsOverview {
        let mut threads: Vec<ReportThread> = Vec::new();
        for report in self.reports() {
            let item = item(&report, doc);
            match threads.iter_mut().find(|t| !report.thread.is_empty() && t.latest.thread == report.thread) {
                Some(thread) => {
                    thread.unread += u32::from(item.unread);
                    thread.items.push(item);
                }
                None => threads.push(ReportThread { id: item.id.clone(), unread: u32::from(item.unread), latest: item.clone(), items: vec![item] }),
            }
        }
        let snapshot = self.snapshot.as_ref();
        ReportsOverview {
            loaded: snapshot.is_some(),
            unread: threads.iter().filter(|t| t.unread > 0).count() as u32,
            total: snapshot.map_or(0, |s| s.total),
            threads,
            refused: snapshot
                .map(|s| s.rejected.iter().filter(|r| !self.deleted_here.contains(&r.id)).map(|r| RefusedReport { id: r.id.clone(), title: r.title.clone(), reason: r.reason.clone(), ms: r.ms }).collect())
                .unwrap_or_default(),
        }
    }
}

/// The box a report names (its id, the agent's session id, an earlier
/// conversation of a passive session, or its name), as the desktop finds it.
pub fn node<'a>(doc: &'a Doc, reference: &str) -> Option<&'a Node> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    doc.node(reference)
        .or_else(|| doc.nodes.iter().find(|n| n.agent_session.as_deref() == Some(reference)))
        .or_else(|| doc.nodes.iter().find(|n| n.passive.as_ref().is_some_and(|p| p.episodes.iter().any(|e| e == reference))))
        .or_else(|| doc.nodes.iter().find(|n| n.name.eq_ignore_ascii_case(reference)))
}

impl From<Format> for ReportFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Message => ReportFormat::Message,
            Format::Markdown => ReportFormat::Markdown,
            Format::Html => ReportFormat::Html,
        }
    }
}

/// A report as the app lists it.
pub fn item(report: &Report, doc: &Doc) -> ReportItem {
    let source = node(doc, &report.source);
    let session = node(doc, &report.session).filter(|s| source.is_none_or(|n| n.id != s.id));
    ReportItem {
        id: report.id.clone(),
        title: report.title.clone(),
        format: report.format.into(),
        kind: report.kind.clone(),
        glyph: kind_glyph(&report.kind).to_owned(),
        created_ms: report.created_ms,
        source_name: source.map(|n| n.name.clone()).unwrap_or_else(|| report.source_name.clone()),
        source_box: source.map(|n| n.id.clone()).unwrap_or_default(),
        session_name: match session {
            Some(node) => node.name.clone(),
            None if node(doc, &report.session).is_none() => report.session.clone(),
            None => String::new(),
        },
        session_box: session.map(|n| n.id.clone()).unwrap_or_default(),
        group_name: report.group_name.clone(),
        pr: report.pr.clone(),
        links: report.links.iter().filter(|l| check_url(&l.url).is_ok()).map(|l| ReportLink { title: l.title.clone(), url: l.url.clone() }).collect(),
        thread: report.thread.clone(),
        preview: without_title(&report.preview, &report.title),
        size: report.size,
        unread: report.unread(),
    }
}

/// The preview line without the title it often starts with.
fn without_title(preview: &str, title: &str) -> String {
    let n = title.len();
    if n > 0 && preview.len() >= n && preview.is_char_boundary(n) && preview[..n].eq_ignore_ascii_case(title) {
        return preview[n..].trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | '—' | '·' | '.')).to_owned();
    }
    preview.to_owned()
}

/// The page a report is shown as: sanitized HTML with the phone's look.
pub fn page(content: &Content) -> ReportPage {
    let html = match content.format {
        Format::Html => content.body.clone(),
        Format::Markdown => markdown_html(&content.body),
        Format::Message => message_html(&content.body),
    };
    ReportPage { id: content.id.clone(), format: content.format.into(), html: report_html::document(&html, &PHONE) }
}

/// Markdown as HTML (tables, strikethrough, task lists, like the desktop's
/// viewer); HTML inside it goes through the sanitizer with the rest.
fn markdown_html(text: &str) -> String {
    use pulldown_cmark::{Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = String::with_capacity(text.len() * 3 / 2);
    pulldown_cmark::html::push_html(&mut out, Parser::new_ext(text, options));
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A message as paragraphs; bare `http(s)://` addresses become links.
fn message_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut out = String::from("<div class=\"message\">");
    for paragraph in text.split("\n\n").map(|p| p.trim_matches('\n')).filter(|p| !p.trim().is_empty()) {
        out.push_str("<p>");
        for (i, line) in paragraph.lines().enumerate() {
            if i > 0 {
                out.push_str("<br>");
            }
            out.push_str(&linkify(line));
        }
        out.push_str("</p>");
    }
    out.push_str("</div>");
    out
}

fn linkify(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        let space = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(space);
        let url = word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
        let lower = url.to_ascii_lowercase();
        if (lower.starts_with("http://") || lower.starts_with("https://")) && check_url(url).is_ok() {
            out.push_str(&format!("<a href=\"{}\">{}</a>{}", escape(url), escape(url), escape(&word[url.len()..])));
        } else {
            out.push_str(&escape(word));
        }
        let next = tail.find(|c: char| !c.is_whitespace()).unwrap_or(tail.len());
        out.push_str(&escape(&tail[..next]));
        rest = &tail[next..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, title: &str, thread: &str, read: bool) -> Report {
        Report { id: id.into(), title: title.into(), kind: "info".into(), thread: thread.into(), read_ms: u64::from(read), preview: format!("{title}: details"), ..Default::default() }
    }

    fn snapshot(version: u64, reports: Vec<Report>) -> Snapshot {
        let unread = reports.iter().filter(|r| r.unread()).count() as u32;
        Snapshot { version, total: reports.len() as u32, unread, reports, rejected: Vec::new() }
    }

    #[test]
    fn threads_group_reports_and_only_new_ones_are_announced() {
        let mut inbox = Inbox::default();
        let doc = Doc::default();
        assert!(!inbox.overview(&doc).loaded);
        // The first snapshot: what was there before the phone looked is not news.
        let first = vec![report("3", "PR #42: 2 comments", "pr:42", false), report("2", "Brief", "", true), report("1", "PR #42 opened", "pr:42", false)];
        assert!(inbox.update(snapshot(1, first.clone())).is_none());
        let overview = inbox.overview(&doc);
        assert_eq!(overview.threads.len(), 2, "one row per thread");
        let pr = &overview.threads[0];
        assert_eq!((pr.id.as_str(), pr.items.len(), pr.unread), ("3", 2, 2));
        assert_eq!(pr.latest.preview, "details", "the preview without its title");
        assert_eq!(overview.unread, 1, "rows with something unread");
        // A new report arrives: announced once.
        let mut next = first.clone();
        next.insert(0, report("4", "Deploy failed", "", false));
        let arrived = inbox.update(snapshot(2, next.clone())).expect("news");
        assert_eq!((arrived.newest.id.as_str(), arrived.more), ("4", 0));
        assert!(inbox.update(snapshot(3, next)).is_none(), "no repeat");
    }

    #[test]
    fn this_phones_changes_show_at_once_until_the_service_confirms_them() {
        let mut inbox = Inbox::default();
        let doc = Doc::default();
        inbox.update(snapshot(1, vec![report("2", "B", "t", false), report("1", "A", "t", false), report("0", "C", "", true)]));
        let json = inbox.request("read", &["2".into(), "1".into()]);
        let request: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!((request["action"].as_str(), request["op"].as_str(), request["ids"][1].as_str()), (Some("reports"), Some("read"), Some("1")));
        assert_eq!(inbox.overview(&doc).unread, 0, "read here before the service answers");
        inbox.request("unread", &["0".into()]);
        inbox.request("delete", &["1".into()]);
        let overview = inbox.overview(&doc);
        assert_eq!((overview.unread, overview.threads.len(), overview.threads[0].items.len()), (1, 2, 1));
        // The service's next snapshot has them applied: nothing kept here.
        let mut confirmed = vec![report("2", "B", "t", true), report("0", "C", "", false)];
        confirmed[0].read_ms = 5;
        inbox.update(snapshot(2, confirmed));
        assert!(inbox.read_here.is_empty() && inbox.unread_here.is_empty() && inbox.deleted_here.is_empty());
        inbox.request("read_all", &[]);
        assert_eq!(inbox.overview(&doc).unread, 0);
    }

    #[test]
    fn reports_name_the_boxes_of_this_canvas() {
        let mut doc = Doc::default();
        let node = |id: &str, name: &str| -> Node { serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": 0, "y": 0 })).unwrap() };
        doc.nodes.push(node("box-1", "Drift guard (renamed)"));
        doc.nodes.push(node("box-2", "Review PR"));
        let mut r = report("1", "Brief", "", false);
        (r.source, r.source_name, r.session) = ("box-1".into(), "Drift guard".into(), "review pr".into());
        let item = item(&r, &doc);
        assert_eq!((item.source_name.as_str(), item.source_box.as_str()), ("Drift guard (renamed)", "box-1"), "its current name");
        assert_eq!((item.session_name.as_str(), item.session_box.as_str()), ("Review PR", "box-2"));
        r.source = "gone".into();
        r.session = "Somewhere else".into();
        let item = super::item(&r, &doc);
        assert_eq!((item.source_name.as_str(), item.source_box.as_str(), item.session_name.as_str()), ("Drift guard", "", "Somewhere else"), "not on this canvas: the stored names");
        r.links = vec![canvas_model::reports::Link { title: "PR".into(), url: "https://github.com/x/y/pull/1".into() }, canvas_model::reports::Link { title: "bad".into(), url: "javascript:x()".into() }];
        assert_eq!(super::item(&r, &doc).links.len(), 1, "only links that may open");
    }

    #[test]
    fn every_format_becomes_one_sanitized_page() {
        let page = |format, body: &str| super::page(&Content { id: "1".into(), format, body: body.into() }).html;
        let html = page(Format::Html, "<style>h1{color:#f0f}</style><h1 onclick=x()>Brief</h1><script>alert(1)</script><img src=\"http://evil/b.png\"><a href=\"https://example.com\">ok</a>");
        assert!(html.starts_with("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none';"));
        assert!(html.contains("<meta name=\"viewport\"") && html.contains("font:-apple-system-body"), "the phone's look");
        assert!(html.contains("<style>h1{color:#f0f}</style><h1>Brief</h1>") && html.contains("<a href=\"https://example.com\" rel=\"noopener noreferrer\">ok</a>"));
        for gone in ["<script", "alert", "onclick", "evil"] {
            assert!(!html.contains(gone), "{gone} survived:\n{html}");
        }
        let md = page(Format::Markdown, "# Root cause\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n\n<script>x()</script>\n\nSome <b onmouseover=x()>bold</b> [link](https://example.com) [bad](javascript:x())");
        assert!(md.contains("<h1>Root cause</h1>") && md.contains("<table>") && md.contains("<td>1</td>"), "{md}");
        assert!(md.contains("type=\"checkbox\" checked=\"\"") && md.contains("<b>bold</b>") && md.contains("<a href=\"https://example.com\" rel=\"noopener noreferrer\">link</a>"), "{md}");
        assert!(!md.contains("<script") && !md.contains("onmouseover") && !md.contains("javascript"), "{md}");
        let message = page(Format::Message, "PR #42 has 2 new comments: https://github.com/x/y/pull/42.\nSee <b>them</b> & reply.\r\n\r\nSecond paragraph");
        assert!(message.contains("<p>PR #42 has 2 new comments: <a href=\"https://github.com/x/y/pull/42\" rel=\"noopener noreferrer\">https://github.com/x/y/pull/42</a>.<br>See &lt;b&gt;them&lt;/b&gt; &amp; reply.</p><p>Second paragraph</p>"), "{message}");
    }
}
