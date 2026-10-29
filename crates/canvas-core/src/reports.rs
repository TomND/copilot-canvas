//! The Reports inbox in the canvas: the service's inbox
//! (`ServiceMsg::Reports`), its rows in the Reports view (one per thread,
//! unread first in mind: a gold dot and a count on the activity bar), and the
//! read-only report viewer (Markdown, HTML and short messages formatted in
//! cells; the GUI shows HTML in a locked-down WebView2 over the viewer's
//! content area). Reading a report never starts or resumes anything: its
//! session buttons only view the box, links open after an explicit click.
//! Read state belongs to the service (bus requests), so every window and
//! client sees the same.

use std::collections::HashSet;
use std::sync::Arc;

use crate::app::{App, AppEvent};
use crate::dialogs::{Action, Confirm, Modal};
use crate::doc::Node;
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::protocol::ClientMsg;
use crate::reports_model::{Content, Format, Report, Snapshot};
use crate::richtext::{self, Line};

/// What the inbox, its rows and the viewer do.
#[derive(Clone, Debug, PartialEq)]
pub enum ReportAction {
    /// Opens the viewer on a report (its thread, newest first).
    Open(String),
    MarkRead(Vec<String>),
    MarkUnread(Vec<String>),
    MarkAllRead,
    /// Asks, then deletes.
    ConfirmDelete(Vec<String>),
    Delete(Vec<String>),
    /// A link inside a report: asks before opening it (its text may differ from its address).
    Link(String),
    /// Opens a URL in this machine's browser.
    OpenUrl(String),
    /// Views a box on the canvas (never starts it).
    ViewSession(String),
    /// The open report as text on the clipboard.
    Copy,
    /// Shows another report of the open thread (+1 older, -1 newer).
    Step(i32),
}

/// How the GUI's HTML view is doing (the core says so in the viewer).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum WebState {
    /// Not used yet (or this front end has none: the TUI).
    #[default]
    Unknown,
    Ready,
    Failed(String),
}

#[derive(Default)]
pub struct ReportsState {
    /// The inbox from the service (`None`: none arrived yet, e.g. an older service).
    pub inbox: Option<Snapshot>,
    /// Report ids known already (new unread ones are announced once).
    seen: HashSet<String>,
    /// Read here, not confirmed by the service yet.
    read_here: HashSet<String>,
    /// Marked unread or deleted here, not confirmed yet.
    unread_here: HashSet<String>,
    deleted_here: HashSet<String>,
    /// The GUI's WebView2 (set by the front end).
    pub web: WebState,
    /// The open report laid out: (id, width, lines).
    rendered: Option<(String, usize, Arc<Vec<Line>>)>,
    /// The open HTML report's safe document: (id, document).
    document: Option<(String, Arc<String>)>,
}

/// One row of the inbox: a thread (or a single report), newest first.
#[derive(Clone, Debug)]
pub struct Thread {
    pub latest: Report,
    /// Every report of the thread, newest first.
    pub ids: Vec<String>,
    pub unread: usize,
}

/// The read-only viewer of one report (a modal).
#[derive(Clone, Debug)]
pub struct ReportViewer {
    /// The report shown.
    pub id: String,
    /// The thread's reports, newest first.
    pub thread: Vec<String>,
    pub content: Option<Arc<Content>>,
    pub error: Option<String>,
    pub top: usize,
    /// Content rows and lines at the last draw (paging).
    pub rows: usize,
    pub total: usize,
    /// Focused button.
    pub sel: usize,
}

/// What a key did in the viewer.
pub enum ViewerOutcome {
    Keep,
    Close,
    Run(Action),
    CloseAndRun(Action),
}

/// The age of a report as shown ("5m", "2h", "3d") at `now`.
pub fn age(ms: u64, now: u64) -> String {
    crate::paths::format_age(ms, now)
}

/// Largest page the GUI's HTML view loads (WebView2 takes about 2 MB in one
/// piece); a larger HTML report shows as formatted text, and says so.
pub const HTML_VIEW_MAX: usize = 2_000_000;

impl ReportsState {
    /// The reports as this client shows them (its own changes applied before the service confirms them).
    pub fn reports(&self) -> Vec<Report> {
        let Some(inbox) = &self.inbox else { return Vec::new() };
        inbox
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

    pub fn report(&self, id: &str) -> Option<Report> {
        self.reports().into_iter().find(|r| r.id == id)
    }

    /// The inbox's rows: reports grouped by thread, the newest thread first.
    pub fn threads(&self) -> Vec<Thread> {
        let mut threads: Vec<Thread> = Vec::new();
        for report in self.reports() {
            let unread = usize::from(report.unread());
            match threads.iter_mut().find(|t| !report.thread.is_empty() && t.latest.thread == report.thread) {
                Some(thread) => {
                    thread.ids.push(report.id.clone());
                    thread.unread += unread;
                }
                None => threads.push(Thread { ids: vec![report.id.clone()], unread, latest: report }),
            }
        }
        threads
    }

    /// Rows with something unread (the activity bar's count).
    pub fn unread_rows(&self) -> usize {
        self.threads().iter().filter(|t| t.unread > 0).count()
    }
}

impl App {
    /// The service's inbox arrived (on connect and after every change).
    pub fn on_reports(&mut self, json: &str) {
        let Ok(snapshot) = serde_json::from_str::<Snapshot>(json) else { return };
        let state = &mut self.reports;
        // Changes made here that the service has applied now.
        state.read_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id && r.read_ms == 0));
        state.unread_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id && r.read_ms != 0));
        state.deleted_here.retain(|id| snapshot.reports.iter().any(|r| &r.id == id));
        let primed = state.inbox.is_some();
        let fresh: Vec<Report> = snapshot.reports.iter().filter(|r| r.unread() && !state.seen.contains(&r.id)).cloned().collect();
        state.seen.extend(snapshot.reports.iter().map(|r| r.id.clone()));
        state.inbox = Some(snapshot);
        self.side_rows_key = 0;
        self.needs_draw = true;
        if !primed || fresh.is_empty() {
            return;
        }
        // New while this window is open: a notice here, a toast and the taskbar on the GUI.
        let newest = &fresh[0];
        let source = self.report_source(newest);
        let from = if source.is_empty() { String::new() } else { format!(" from {source}") };
        let more = if fresh.len() > 1 { format!(" (+{} more)", fresh.len() - 1) } else { String::new() };
        self.notify(format!("New report{from}: {}{more} · Reports (Alt+9)", newest.title), false);
        for report in fresh.iter().take(3) {
            let source = self.report_source(report);
            self.platform.report_arrived(&report.title, &source);
        }
    }

    /// Who published a report: its box's name on this canvas, else the name
    /// it had when it was stored.
    pub fn report_source(&self, report: &Report) -> String {
        match self.report_node(&report.source) {
            Some(node) => node.name.clone(),
            None => report.source_name.clone(),
        }
    }

    /// The box a report's session reference names (its id, the agent's id,
    /// an earlier conversation of a passive session, or its name).
    pub fn report_node(&self, reference: &str) -> Option<&Node> {
        let reference = reference.trim();
        if reference.is_empty() {
            return None;
        }
        self.doc
            .node(reference)
            .or_else(|| self.doc.nodes.iter().find(|n| n.agent_session.as_deref() == Some(reference)))
            .or_else(|| self.doc.nodes.iter().find(|n| n.passive.as_ref().is_some_and(|p| p.episodes.iter().any(|e| e == reference))))
            .or_else(|| self.doc.nodes.iter().find(|n| n.name.eq_ignore_ascii_case(reference)))
    }

    /// Changes read state or deletes, here at once and at the service
    /// (which tells every window and client).
    fn report_request(&mut self, op: &str, ids: &[String]) {
        let state = &mut self.reports;
        for id in ids {
            match op {
                "read" => {
                    state.unread_here.remove(id);
                    state.read_here.insert(id.clone());
                }
                "unread" => {
                    state.read_here.remove(id);
                    state.unread_here.insert(id.clone());
                }
                "delete" => {
                    state.deleted_here.insert(id.clone());
                }
                _ => {}
            }
        }
        if op == "read_all" {
            for report in state.reports() {
                state.read_here.insert(report.id);
            }
            state.unread_here.clear();
        }
        let json = serde_json::json!({ "action": "reports", "op": op, "ids": ids }).to_string();
        self.send(ClientMsg::Bus { json });
        self.side_rows_key = 0;
        self.needs_draw = true;
    }

    pub fn report_action(&mut self, action: ReportAction) {
        self.needs_draw = true;
        match action {
            ReportAction::Open(id) => self.open_report(&id),
            ReportAction::MarkRead(ids) => self.report_request("read", &ids),
            ReportAction::MarkUnread(ids) => self.report_request("unread", &ids),
            ReportAction::MarkAllRead => self.report_request("read_all", &[]),
            ReportAction::ConfirmDelete(ids) => {
                let titles: Vec<String> = ids.iter().filter_map(|id| self.reports.report(id)).map(|r| r.title).collect();
                let rejected = self.reports.inbox.as_ref().map(|i| i.rejected.iter().filter(|r| ids.contains(&r.id)).count()).unwrap_or(0);
                let what = match (titles.as_slice(), rejected) {
                    ([one], 0) => format!("Delete the report “{}”?", crate::paint::truncate(one, 50)),
                    ([], n) if n > 0 => "Dismiss the refused report?".to_owned(),
                    (all, _) => format!("Delete {} reports?", all.len().max(ids.len())),
                };
                self.modals.push(Modal::Confirm(Confirm {
                    title: what,
                    body: vec!["It goes from the inbox in every window and client; this cannot be undone.".into()],
                    buttons: vec![("Delete".into(), Action::Report(ReportAction::Delete(ids))), ("Cancel".into(), Action::Nothing)],
                    sel: 1,
                }));
            }
            ReportAction::Delete(ids) => {
                self.report_request("delete", &ids);
                // A viewer on a deleted report closes.
                self.modals.retain(|m| !matches!(m, Modal::Report(v) if ids.contains(&v.id)));
                self.notify(if ids.len() == 1 { "Report deleted.".to_owned() } else { format!("{} reports deleted.", ids.len()) }, false);
            }
            ReportAction::Link(url) => {
                self.modals.push(Modal::Confirm(Confirm {
                    title: "Open this link from a report?".into(),
                    body: vec![crate::paint::truncate(&url, 100), "It opens in your browser on this machine.".into()],
                    buttons: vec![("Open".into(), Action::Report(ReportAction::OpenUrl(url))), ("Cancel".into(), Action::Nothing)],
                    sel: 0,
                }));
            }
            ReportAction::OpenUrl(url) => {
                if canvas_model::reports::check_url(&url).is_err() {
                    return self.notify("Only http(s) and mailto links open from reports.", true);
                }
                match self.platform.open_link(&url) {
                    Ok(message) => self.notify(message, false),
                    Err(error) => self.notify(format!("{error:#}"), true),
                }
            }
            ReportAction::ViewSession(id) => {
                // Views only: its terminal when it runs here, else its details panel.
                self.reveal_node(&id);
                self.view_node(&id);
            }
            ReportAction::Copy => self.copy_report(),
            ReportAction::Step(delta) => self.step_report(delta),
        }
    }

    /// Opens the viewer on a report's thread (the newest report on top) and
    /// marks the thread read. Nothing is started.
    pub fn open_report(&mut self, id: &str) {
        let threads = self.reports.threads();
        let Some(thread) = threads.iter().find(|t| t.ids.iter().any(|i| i == id)) else {
            return self.notify("This report is no longer in the inbox.", true);
        };
        let shown = if thread.ids.first().is_some_and(|first| first == id) || !thread.ids.iter().any(|i| i == id) { thread.latest.id.clone() } else { id.to_owned() };
        let unread: Vec<String> = thread.ids.iter().filter(|i| self.reports.report(i).is_some_and(|r| r.unread())).cloned().collect();
        self.modals.retain(|m| !matches!(m, Modal::Report(_)));
        self.modals.push(Modal::Report(Box::new(ReportViewer { id: shown.clone(), thread: thread.ids.clone(), content: None, error: None, top: 0, rows: 20, total: 0, sel: 0 })));
        if !unread.is_empty() {
            self.report_request("read", &unread);
        }
        self.load_report(&shown);
    }

    fn viewer_mut(&mut self) -> Option<&mut ReportViewer> {
        self.modals.iter_mut().rev().find_map(|m| if let Modal::Report(v) = m { Some(&mut **v) } else { None })
    }

    /// Reads a report's body (from the backend: locally or through the service).
    fn load_report(&mut self, id: &str) {
        let id = id.to_owned();
        let key = id.clone();
        self.in_background(
            move |backend| backend.report_content(&id).map_err(|e| format!("{e:#}")),
            move |result| {
                AppEvent::Run(Box::new(move |app: &mut App| {
                    if let Some(viewer) = app.viewer_mut().filter(|v| v.id == key) {
                        match result {
                            Ok(content) => viewer.content = Some(Arc::new(content)),
                            Err(error) => viewer.error = Some(format!("Cannot read the report: {error}")),
                        }
                    }
                    app.needs_draw = true;
                }))
            },
        );
    }

    fn step_report(&mut self, delta: i32) {
        let Some(viewer) = self.viewer_mut() else { return };
        let Some(at) = viewer.thread.iter().position(|i| *i == viewer.id) else { return };
        let next = at as i64 + delta as i64;
        let Some(id) = (next >= 0).then(|| viewer.thread.get(next as usize).cloned()).flatten() else { return };
        viewer.id = id.clone();
        viewer.content = None;
        viewer.error = None;
        viewer.top = 0;
        self.load_report(&id);
    }

    /// The open report as text (title, details, content).
    fn copy_report(&mut self) {
        let Some(viewer) = self.modals.iter().rev().find_map(|m| if let Modal::Report(v) = m { Some((**v).clone()) } else { None }) else { return };
        let Some(content) = viewer.content.clone() else { return self.notify("The report is still loading.", false) };
        let report = self.reports.report(&viewer.id);
        let lines = richtext::layout(&blocks(&content), 100);
        let mut text = String::new();
        if let Some(report) = &report {
            let mut shown = report.clone();
            shown.source_name = self.report_source(report);
            text.push_str(&format!("{}\r\n{}\r\n\r\n", report.title, details(&shown, true, crate::paths::now_ms())));
        }
        text.push_str(&richtext::plain(&lines));
        match self.platform.set_clipboard_text(&text) {
            Ok(()) => self.notify(format!("Copied the report ({} lines).", text.lines().count()), false),
            Err(error) => self.notify(format!("Copy failed: {error:#}"), true),
        }
    }

    /// The viewer's buttons: view the sessions it names, its pull request,
    /// the thread's other reports, copy, mark unread, delete, close.
    pub fn report_buttons(&self, viewer: &ReportViewer) -> Vec<(String, Action)> {
        let mut buttons = Vec::new();
        let report = self.reports.report(&viewer.id);
        if let Some(report) = &report {
            let mut viewed = Vec::new();
            for reference in [&report.source, &report.session] {
                if let Some(node) = self.report_node(reference).filter(|n| !viewed.contains(&n.id)) {
                    viewed.push(node.id.clone());
                    buttons.push((format!("View “{}”", crate::paint::truncate(&node.name, 24)), Action::Report(ReportAction::ViewSession(node.id.clone()))));
                }
            }
            if !report.pr.is_empty() {
                buttons.push(("↗ Pull request".into(), Action::Report(ReportAction::OpenUrl(report.pr.clone()))));
            }
        }
        let at = viewer.thread.iter().position(|i| *i == viewer.id).unwrap_or(0);
        if at + 1 < viewer.thread.len() {
            buttons.push(("◂ Older".into(), Action::Report(ReportAction::Step(1))));
        }
        if at > 0 {
            buttons.push(("Newer ▸".into(), Action::Report(ReportAction::Step(-1))));
        }
        buttons.push(("Copy".into(), Action::Report(ReportAction::Copy)));
        buttons.push(("Mark unread".into(), Action::Report(ReportAction::MarkUnread(vec![viewer.id.clone()]))));
        buttons.push(("Delete".into(), Action::Report(ReportAction::ConfirmDelete(vec![viewer.id.clone()]))));
        buttons.push(("Close".into(), Action::Nothing));
        buttons
    }

    /// A key in the viewer.
    pub fn report_key(&mut self, viewer: &mut ReportViewer, key: &KeyEvent) -> ViewerOutcome {
        let page = viewer.rows.max(2) as i64 - 1;
        let buttons = self.report_buttons(viewer);
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let mut scroll = |delta: i64| {
            let max = viewer.total.saturating_sub(viewer.rows) as i64;
            viewer.top = (viewer.top as i64 + delta).clamp(0, max.max(0)) as usize;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ViewerOutcome::Close,
            KeyCode::Char('c') if ctrl => return ViewerOutcome::Run(Action::Report(ReportAction::Copy)),
            KeyCode::Up => scroll(-1),
            KeyCode::Down => scroll(1),
            KeyCode::PageUp => scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll(page),
            KeyCode::Home => viewer.top = 0,
            KeyCode::End => scroll(i64::MAX / 4),
            KeyCode::Left => return ViewerOutcome::Run(Action::Report(ReportAction::Step(1))),
            KeyCode::Right => return ViewerOutcome::Run(Action::Report(ReportAction::Step(-1))),
            KeyCode::Tab => viewer.sel = (viewer.sel + 1) % buttons.len(),
            KeyCode::BackTab => viewer.sel = (viewer.sel + buttons.len() - 1) % buttons.len(),
            KeyCode::Enter => {
                let action = buttons.get(viewer.sel.min(buttons.len() - 1)).map(|(_, a)| a.clone()).unwrap_or(Action::Nothing);
                return match action {
                    Action::Nothing => ViewerOutcome::Close,
                    // Viewing a session or marking the report unread leaves the report.
                    Action::Report(ReportAction::ViewSession(_) | ReportAction::MarkUnread(_)) => ViewerOutcome::CloseAndRun(action),
                    action => ViewerOutcome::Run(action),
                };
            }
            _ => {}
        }
        ViewerOutcome::Keep
    }

    /// The open report laid out `width` cells wide (cached per report and width).
    pub fn report_lines(&mut self, viewer: &ReportViewer, width: usize) -> Arc<Vec<Line>> {
        if let Some((id, w, lines)) = &self.reports.rendered
            && *id == viewer.id
            && *w == width
            && viewer.content.is_some()
        {
            return lines.clone();
        }
        let lines = match (&viewer.content, &viewer.error) {
            (Some(content), _) => Arc::new(richtext::layout(&blocks(content), width)),
            _ => Arc::new(Vec::new()),
        };
        if viewer.content.is_some() {
            self.reports.rendered = Some((viewer.id.clone(), width, lines.clone()));
        }
        lines
    }

    /// The open HTML report's page for the GUI's view (sanitized, with its policy).
    pub fn report_document(&mut self, viewer: &ReportViewer) -> Option<Arc<String>> {
        let content = viewer.content.as_ref().filter(|c| c.format == Format::Html)?;
        if let Some((id, document)) = &self.reports.document
            && *id == viewer.id
        {
            return Some(document.clone());
        }
        let document = Arc::new(crate::report_html::safe_document(&content.body));
        self.reports.document = Some((viewer.id.clone(), document.clone()));
        Some(document)
    }
}

/// A report's body as formatted blocks.
pub fn blocks(content: &Content) -> Vec<richtext::Block> {
    match content.format {
        Format::Markdown => richtext::markdown(&content.body),
        Format::Html => richtext::html(&content.body),
        Format::Message => richtext::message(&content.body),
    }
}

/// One line about a report: its kind, who published it and when (`absolute`:
/// with the UTC time too; the screen shows the age only).
pub fn details(report: &Report, absolute: bool, now: u64) -> String {
    let mut parts = vec![format!("{} {}", canvas_model::reports::kind_glyph(&report.kind), report.kind)];
    if !report.source_name.is_empty() {
        let group = if report.group_name.is_empty() { String::new() } else { format!(" ({})", report.group_name) };
        parts.push(format!("from {}{group}", report.source_name));
    }
    let when = crate::paths::format_rfc3339_ms(report.created_ms).chars().take(16).collect::<String>().replace('T', " ");
    parts.push(if absolute { format!("{} ago ({when} UTC)", age(report.created_ms, now)) } else { format!("{} ago", age(report.created_ms, now)) });
    parts.push(report.format.label().to_owned());
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::sync::Opened;
    use crossbeam_channel::{Receiver, bounded, unbounded};

    pub(crate) fn report(id: &str, title: &str, thread: &str, read: bool) -> Report {
        Report {
            id: id.into(),
            title: title.into(),
            format: Format::Markdown,
            kind: "info".into(),
            created_ms: id.split('-').next().and_then(|n| n.parse().ok()).unwrap_or(1),
            source: "box-1".into(),
            source_name: "Fix flaky test".into(),
            thread: thread.into(),
            read_ms: if read { 5 } else { 0 },
            ..Default::default()
        }
    }

    fn fixture() -> (App, Receiver<ClientMsg>) {
        let node: Node = serde_json::from_value(serde_json::json!({ "id": "box-1", "kind": "session", "name": "Fix flaky test", "x": 0, "y": 0, "cwd": "." })).unwrap();
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes: vec![node], ..Doc::default() };
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), false, ".".into(), app_tx, None);
        // "The backend did not answer": there is none here.
        app.notice = None;
        (app, sent)
    }

    fn inbox(reports: Vec<Report>) -> String {
        let unread = reports.iter().filter(|r| r.unread()).count() as u32;
        serde_json::to_string(&Snapshot { version: 1, total: reports.len() as u32, unread, reports, rejected: vec![] }).unwrap()
    }

    fn bus(sent: &Receiver<ClientMsg>) -> Vec<serde_json::Value> {
        sent.try_iter().filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str(&json).ok() } else { None }).collect()
    }

    #[test]
    fn threads_group_updates_and_new_reports_are_announced_once() {
        let (mut app, _sent) = fixture();
        app.on_reports(&inbox(vec![report("0000000000003-a", "Brief", "", false), report("0000000000002-b", "Old", "", true)]));
        assert_eq!(app.reports.unread_rows(), 1);
        assert!(app.notice.is_none(), "what was there on connect is not announced");
        app.on_reports(&inbox(vec![
            report("0000000000009-c", "PR 12 delivered", "pr-12", false),
            report("0000000000005-d", "PR 12 queued", "pr-12", false),
            report("0000000000003-a", "Brief", "", false),
            report("0000000000002-b", "Old", "", true),
        ]));
        let threads = app.reports.threads();
        assert_eq!(threads.len(), 3, "the two PR 12 updates are one row");
        assert_eq!((threads[0].latest.title.as_str(), threads[0].ids.len(), threads[0].unread), ("PR 12 delivered", 2, 2));
        assert_eq!(app.reports.unread_rows(), 2);
        let notice = app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default();
        assert!(notice.contains("New report from Fix flaky test: PR 12 delivered (+1 more)"), "{notice}");
        app.notice = None;
        app.on_reports(&inbox(vec![report("0000000000009-c", "PR 12 delivered", "pr-12", false)]));
        assert!(app.notice.is_none(), "seen ones are not announced again");
    }

    #[test]
    fn opening_a_report_marks_its_thread_read_and_never_launches() {
        let (mut app, sent) = fixture();
        app.on_reports(&inbox(vec![report("0000000000009-c", "PR 12 delivered", "pr-12", false), report("0000000000005-d", "PR 12 queued", "pr-12", false)]));
        app.report_action(ReportAction::Open("0000000000005-d".into()));
        let Some(Modal::Report(viewer)) = app.modals.last() else { panic!("viewer") };
        assert_eq!((viewer.id.as_str(), viewer.thread.len()), ("0000000000005-d", 2), "the one clicked, its thread to step through");
        let messages: Vec<ClientMsg> = sent.try_iter().collect();
        assert!(!messages.iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "reading starts nothing");
        let reads: Vec<serde_json::Value> = messages.iter().filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str(json).ok() } else { None }).collect();
        assert_eq!(reads.len(), 1);
        assert_eq!((reads[0]["action"].as_str(), reads[0]["op"].as_str(), reads[0]["ids"].as_array().map(Vec::len)), (Some("reports"), Some("read"), Some(2)));
        assert_eq!(app.reports.unread_rows(), 0, "read here at once");
        // The viewer's buttons: the source box (view only), the thread, copy, unread, delete, close.
        let viewer = match app.modals.last() {
            Some(Modal::Report(v)) => (**v).clone(),
            _ => unreachable!(),
        };
        let labels: Vec<String> = app.report_buttons(&viewer).into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, ["View “Fix flaky test”", "Newer ▸", "Copy", "Mark unread", "Delete", "Close"]);
        let mut viewer = viewer;
        viewer.sel = 0;
        let outcome = app.report_key(&mut viewer, &KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        assert!(matches!(outcome, ViewerOutcome::CloseAndRun(Action::Report(ReportAction::ViewSession(ref id))) if id == "box-1"));
        app.modals.clear();
        app.report_action(ReportAction::ViewSession("box-1".into()));
        assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "viewing the session starts nothing");
        assert_eq!(app.right_detail, Some(crate::side::RightDetail::Session("box-1".into())), "its details panel");
        // The service confirms: nothing is left pending here.
        app.on_reports(&inbox(vec![report("0000000000009-c", "PR 12 delivered", "pr-12", true), report("0000000000005-d", "PR 12 queued", "pr-12", true)]));
        assert!(app.reports.read_here.is_empty());
    }

    #[test]
    fn unread_delete_and_links_go_through_the_service_and_ask_first() {
        let (mut app, sent) = fixture();
        app.on_reports(&inbox(vec![report("0000000000009-c", "Brief", "", true)]));
        app.report_action(ReportAction::MarkUnread(vec!["0000000000009-c".into()]));
        assert_eq!(app.reports.unread_rows(), 1);
        app.report_action(ReportAction::ConfirmDelete(vec!["0000000000009-c".into()]));
        let Some(Modal::Confirm(confirm)) = app.modals.last().cloned() else { panic!("asks first") };
        assert!(confirm.title.contains("Delete the report “Brief”"));
        assert_eq!(confirm.sel, 1, "Cancel is preselected");
        app.modals.clear();
        app.report_action(ReportAction::Delete(vec!["0000000000009-c".into()]));
        assert!(app.reports.threads().is_empty());
        app.report_action(ReportAction::Link("https://example.com/x".into()));
        assert!(matches!(app.modals.last(), Some(Modal::Confirm(c)) if c.body[0] == "https://example.com/x"), "a link in a report asks before it opens");
        app.modals.clear();
        app.report_action(ReportAction::OpenUrl("file:///C:/secret.txt".into()));
        assert!(app.notice.as_ref().is_some_and(|n| n.error && n.text.contains("Only http(s)")));
        let ops: Vec<String> = bus(&sent).iter().map(|v| v["op"].as_str().unwrap_or_default().to_owned()).collect();
        assert_eq!(ops, ["unread", "delete"]);
    }

    #[test]
    fn report_bodies_lay_out_and_html_gets_a_safe_document() {
        let (mut app, _sent) = fixture();
        let content = Content { id: "1-a".into(), format: Format::Html, body: "<h1>T</h1><script>alert(1)</script><p>x</p>".into() };
        let viewer = ReportViewer { id: "1-a".into(), thread: vec!["1-a".into()], content: Some(Arc::new(content)), error: None, top: 0, rows: 10, total: 0, sel: 0 };
        let lines = app.report_lines(&viewer, 40);
        assert_eq!(lines.iter().map(|l| l.text()).filter(|t| !t.trim().is_empty()).collect::<Vec<_>>(), ["T", "━━━", "x"]);
        assert!(Arc::ptr_eq(&lines, &app.report_lines(&viewer, 40)), "cached for the same width");
        let document = app.report_document(&viewer).unwrap();
        assert!(document.contains("script-src 'none'") && !document.contains("alert"));
    }
}
