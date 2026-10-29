//! Drawing for the activity bar, the non-session side views and the right-hand
//! detail panel (automations and configuration items).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::app::{App, Focus};
use crate::dialogs::{Action, Hit};
use crate::paint::{ACCENT, FAINT, GRAY, POSITIVE, LIGHT, MUTED, NEGATIVE, PANEL, PANEL_HI, SELECT, SPINNER, TEXT, ATTENTION, bold, fg, fill, outline, put, truncate};
use crate::status::Status;
use crate::side::{ItemRef, RightDetail, SideView, VIEWS, automation_status, status_color, when};
use crate::config::Kind;

pub const BAR_W: u16 = 3;

pub fn draw_activity_bar(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(ratatui::style::Color::Rgb(13, 17, 23)), area);
    let unread = app.reports.unread_rows();
    for (i, (view, icon, title)) in VIEWS.iter().enumerate() {
        let y = area.y as i32 + 1 + i as i32 * 2;
        if y >= area.bottom() as i32 {
            break;
        }
        let active = app.side_view == *view && app.doc.ui.sidebar_visible;
        let bg = if active { PANEL_HI } else { ratatui::style::Color::Rgb(13, 17, 23) };
        let style = if active { bold(SELECT).bg(PANEL_HI) } else { fg(GRAY).bg(bg) };
        if active {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(PANEL_HI), area);
            put(buf, area.x as i32, y, "▌", fg(ACCENT).bg(PANEL_HI), area);
        }
        // Unread reports: the icon in gold with their count beside it (9+ as "+").
        let unread_here = *view == SideView::Reports && unread > 0;
        put(buf, area.x as i32 + 1, y, icon, if unread_here { bold(ATTENTION).bg(bg) } else { style }, area);
        if unread_here {
            let count = if unread > 9 { "+".to_owned() } else { unread.to_string() };
            put(buf, area.x as i32 + 2, y, &count, bold(ATTENTION).bg(bg), area);
        }
        let _ = title;
        app.layout.hits.push((Rect::new(area.x, y as u16, area.width, 1), Hit::Toolbar(Action::View(*view))));
    }
}

fn view_title(app: &App) -> String {
    let base = VIEWS.iter().find(|(v, _, _)| *v == app.side_view).map(|(_, _, t)| *t).unwrap_or("");
    match (app.side_view, app.settings_group.as_deref().and_then(|g| app.doc.group(g))) {
        (SideView::Settings, Some(group)) => format!("{base} · {}", group.name),
        _ => base.to_owned(),
    }
}

fn hints(view: SideView) -> &'static str {
    match view {
        SideView::Automations => "Enter details · Space on/off",
        SideView::Plugins => "Space on/off · Del uninstall · A agent",
        SideView::Mcp => "Space on/off · Del remove · A agent",
        SideView::Skills => "Space on/off · Del remove · A agent",
        SideView::Agents => "Enter details · A agent",
        SideView::VsCode => "Enter run / toggle",
        SideView::Settings => "Enter edit · A agent",
        SideView::Reports => "Enter open · Space read/unread · Del delete",
        SideView::Sessions => "",
    }
}

pub fn draw_list(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(PANEL), area);
    let focused = app.focus == Focus::Sidebar;
    let busy = app.cli_busy.is_some() || !app.configs_loading.is_empty() || app.automations_loading || app.vs_syncing;
    let title = view_title(app);
    let end = put(buf, area.x as i32 + 1, area.y as i32, &truncate(&title, area.width as usize - 3), bold(if focused { ACCENT } else { GRAY }).bg(PANEL), area);
    if busy {
        put(buf, end + 1, area.y as i32, SPINNER[app.tick % SPINNER.len()], fg(POSITIVE).bg(PANEL), area);
    }
    put(buf, area.x as i32 + 1, area.y as i32 + 1, &truncate(hints(app.side_view), area.width as usize - 2), fg(MUTED).bg(PANEL), area);
    let rows = app.build_list();
    let top = area.y + 3;
    let visible = area.height.saturating_sub(3) as usize;
    app.list_sel = app.list_sel.min(rows.len().saturating_sub(1));
    if !rows.get(app.list_sel).is_some_and(|r| r.selectable())
        && let Some(first) = rows.iter().position(|r| r.selectable())
    {
        app.list_sel = first;
    }
    if app.list_sel < app.list_scroll {
        app.list_scroll = app.list_sel;
    } else if app.list_sel >= app.list_scroll + visible {
        app.list_scroll = app.list_sel + 1 - visible;
    }
    app.list_scroll = app.list_scroll.min(rows.len().saturating_sub(visible));
    app.layout.sidebar_rows.clear();
    app.layout.sidebar_filter_row = None;
    let right_edge = area.right() as i32;
    for (i, row) in rows.iter().enumerate().skip(app.list_scroll).take(visible) {
        let y = top as i32 + (i - app.list_scroll) as i32;
        app.layout.sidebar_rows.push((y as u16, i));
        let selected = i == app.list_sel && row.selectable();
        let bg = if selected { PANEL_HI } else { PANEL };
        if selected {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(bg), area);
            if focused {
                put(buf, area.x as i32, y, "▎", fg(ACCENT).bg(bg), area);
            }
        }
        match row.item {
            ItemRef::Header => {
                put(buf, area.x as i32 + 1, y, &truncate(&row.label, area.width as usize - 2), bold(MUTED).bg(bg), area);
            }
            ItemRef::Info => {
                put(buf, area.x as i32 + 2, y, &truncate(&row.label, area.width as usize - 3), fg(MUTED).bg(bg), area);
            }
            ItemRef::Detail => {
                put(buf, area.x as i32 + 4, y, &crate::paint::truncate_left(&row.label, (area.width as usize).saturating_sub(5).max(4)), fg(row.color).bg(bg), area);
            }
            ItemRef::ReportLine(_) => {
                put(buf, area.x as i32 + 4, y, &truncate(&row.label, (area.width as usize).saturating_sub(5).max(4)), fg(row.color).bg(bg), area);
            }
            _ => {
                let mut x = put(buf, area.x as i32 + 2, y, row.icon, fg(row.color).bg(bg), area) + 1;
                // Names win: the right column only gets what the label leaves free (min 6).
                let label_w = crate::paint::text_width(&row.label) as i32;
                let free = (right_edge - x - label_w - 3).max(if row.right.is_empty() { 0 } else { 6 }) as usize;
                let right = truncate(&row.right, free.min((area.width as usize / 2).saturating_sub(2)));
                let right_w = crate::paint::text_width(&right) as i32;
                let room = (right_edge - x - right_w - 2).max(1) as usize;
                // An unread report's title is bold.
                let unread = matches!(&row.item, ItemRef::Report(_)) && row.icon == "●";
                let style = if selected || unread { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) };
                x = put(buf, x, y, &truncate(&row.label, room), style, area);
                let _ = x;
                if right_w > 0 {
                    put(buf, right_edge - right_w - 1, y, &right, fg(MUTED).bg(bg), area);
                }
            }
        }
    }
    if rows.len() > visible {
        let hint = format!(" {}–{} of {} ", app.list_scroll + 1, (app.list_scroll + visible).min(rows.len()), rows.len());
        put(buf, right_edge - hint.len() as i32 - 1, area.y as i32, &hint, fg(MUTED).bg(PANEL), area);
    }
}

/// Word-wraps text to `width` cells (keeps explicit line breaks).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for paragraph in text.replace("\r\n", "\n").split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() { word.to_owned() } else { format!("{line} {word}") };
            if crate::paint::text_width(&candidate) > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line = word.to_owned();
            } else {
                line = candidate;
            }
            while crate::paint::text_width(&line) > width {
                let cut: String = line.chars().take(width).collect();
                line = line.chars().skip(width).collect();
                out.push(cut);
            }
        }
        out.push(line);
    }
    out
}

struct Lines {
    lines: Vec<(String, Style)>,
    width: usize,
    /// Lines drawn as coloured spans instead of one style (by line index).
    spans: std::collections::HashMap<usize, Vec<(String, Style)>>,
    /// Clickable parts of lines: (line, first cell, cells, action).
    clicks: Vec<(usize, i32, i32, Action)>,
}

impl Lines {
    fn text(&mut self, text: impl Into<String>, style: Style) {
        for line in wrap(&text.into(), self.width) {
            self.lines.push((line, style));
        }
    }
    /// Text wrapped under an indent (every line starts at it).
    fn indented(&mut self, indent: usize, text: impl Into<String>, style: Style) {
        for line in wrap(&text.into(), self.width.saturating_sub(indent)) {
            self.lines.push((format!("{}{line}", " ".repeat(indent)), style));
        }
    }
    /// One line that acts on a click (cut to the width).
    fn link(&mut self, text: &str, style: Style, action: Action) {
        let text = truncate(text, self.width);
        self.clicks.push((self.lines.len(), 0, crate::paint::text_width(&text) as i32, action));
        self.lines.push((text, style));
    }
    /// A row of small buttons (as many as fit).
    fn buttons(&mut self, indent: usize, items: Vec<(String, Action)>) {
        let mut spans = vec![(" ".repeat(indent), Style::default())];
        let mut x = indent as i32;
        for (label, action) in items {
            let text = format!(" {label} ");
            let w = crate::paint::text_width(&text) as i32;
            if x + w > self.width as i32 {
                break;
            }
            self.clicks.push((self.lines.len(), x, w, action));
            spans.push((text, Style::default().fg(ACCENT).bg(PANEL_HI)));
            spans.push((" ".into(), Style::default()));
            x += w + 1;
        }
        self.spans(spans);
    }
    fn field(&mut self, label: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        let pad = 11usize;
        let wrapped = wrap(value, self.width.saturating_sub(pad));
        for (i, line) in wrapped.into_iter().enumerate() {
            let head = if i == 0 { format!("{label:<width$}", width = pad) } else { " ".repeat(pad) };
            self.lines.push((format!("{head}{line}"), fg(TEXT)));
        }
    }
    fn gap(&mut self) {
        self.lines.push((String::new(), Style::default()));
    }

    /// A field on one line; a long value loses its start (paths keep their end).
    fn field_left(&mut self, label: &str, value: &str) {
        if !value.is_empty() {
            let text = crate::paint::truncate_left(value, self.width.saturating_sub(11).max(8));
            self.lines.push((format!("{label:<11}{text}"), fg(TEXT)));
        }
    }

    /// One line of differently coloured pieces (not wrapped).
    fn spans(&mut self, spans: Vec<(String, Style)>) {
        let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        self.spans.insert(self.lines.len(), spans);
        self.lines.push((text, Style::default()));
    }
}

/// Info and health page of a manifest-managed automation.
fn task_detail(app: &App, out: &mut Lines, actions: &mut Vec<(String, Action)>, a: &crate::automation::Automation, live: &crate::automation::Live) {
    let status = automation_status(a);
    let m = &live.manifest;
    out.text(format!("◷ {}", a.name), bold(TEXT));
    let finished = live.runs.iter().find(|r| !r.ended.is_empty());
    let headline = match status {
        Status::Working => "running now".to_owned(),
        Status::Failed => match finished {
            Some(r) if !m.succeeded(r) => format!(
                "last run failed {} · {}",
                when(&r.ended),
                match (r.exit, r.error.is_empty()) {
                    (Some(code), true) => format!("exit code {code}"),
                    (Some(code), false) => format!("exit code {code}: {}", r.error),
                    (None, _) => r.error.clone(),
                }
            ),
            _ => "the last run reported a failure".to_owned(),
        },
        Status::Stopped if live.task.is_none() => "not registered in Task Scheduler (copilot-canvas automations apply)".to_owned(),
        Status::Stopped => "disabled".to_owned(),
        Status::Waiting => "last run reported a warning".to_owned(),
        _ => match finished {
            Some(r) => format!("ok · last run {}", when(&r.ended)),
            None => "no run yet".to_owned(),
        },
    };
    out.text(format!("{} {headline}", status.icon(app.tick)), bold(status.color()));
    out.text(format!("{} · {}{}", m.family, a.schedule, if m.ai { " · uses AI" } else { "" }), fg(MUTED));
    // Run history, oldest to newest.
    let history: Vec<&crate::automation::LogRun> = live.runs.iter().take(24).collect();
    if !history.is_empty() {
        let mut spans = vec![("Runs  ".to_owned(), fg(MUTED))];
        for r in history.iter().rev() {
            let (glyph, color) = match (r.ended.is_empty() && r.error.is_empty(), r.exit) {
                (true, _) => ("◌", TEXT),
                _ if m.succeeded(r) => ("●", POSITIVE),
                _ => ("✗", NEGATIVE),
            };
            spans.push((format!("{glyph} "), fg(color)));
        }
        let failed = history.iter().filter(|r| !r.ended.is_empty() && !m.succeeded(r)).count();
        spans.push((format!(" {failed} of {} failed", history.len()), fg(if failed > 0 { NEGATIVE } else { MUTED })));
        out.spans(spans);
    }
    out.gap();
    if let Some(report) = &live.report {
        let current = finished.is_some_and(|f| crate::automation::report_is_for(report, &f.started)) || finished.is_none();
        let color = match report.state.as_str() {
            "ok" => POSITIVE,
            "warn" => ATTENTION,
            _ => NEGATIVE,
        };
        out.text(format!("REPORT · {}{}", report.state.to_uppercase(), if current { String::new() } else { " (from an earlier run)".into() }), bold(color));
        out.text(report.headline.clone(), bold(TEXT));
        out.field("Written", &when(&report.at));
        for metric in &report.metrics {
            let c = match metric.state.as_str() {
                "ok" => POSITIVE,
                "warn" => ATTENTION,
                "alert" | "failed" => NEGATIVE,
                _ => TEXT,
            };
            out.spans(vec![(format!("  {:<14} ", metric.name), fg(MUTED)), (metric.value.clone(), fg(c))]);
        }
        if !report.summary.trim().is_empty() {
            for line in report.summary.lines().take(14) {
                out.text(line.to_owned(), fg(TEXT));
            }
        }
        for link in report.links.iter().take(6) {
            actions.push((format!("↗ {}", link.title), Action::OpenTarget(link.url.clone())));
        }
        out.gap();
    } else {
        out.text("No status report yet. Automations write one to AUTOMATION_STATUS_FILE (copilot-canvas automations report).", fg(MUTED));
        out.gap();
    }
    match &live.task {
        Some(t) => {
            out.field("Next run", &if a.enabled && t.next_run_ms > 0 { when(&crate::paths::format_rfc3339_ms(t.next_run_ms)) } else { "—".into() });
            out.field("Task", &format!("{} · {} · last result {:#x}", t.state, if t.enabled { "enabled" } else { "disabled" }, t.last_result));
        }
        None => out.field("Task", "not registered"),
    }
    if let Some(r) = finished {
        out.field("Last run", &format!("{} → {}", r.started.chars().take(19).collect::<String>(), r.ended.chars().take(19).collect::<String>()));
    }
    for d in &live.drift {
        out.text(format!("⚠ drift: {d}"), fg(ATTENTION));
    }
    out.field("Command", &format!("{} {}", m.command, m.args));
    out.field("Folder", &m.cwd);
    out.field("Manifest", &m.path.display().to_string());
    // Tails are read on the backend's machine in the background (`App::tails`).
    let tail = app.tails.get(&live.log_path.display().to_string()).cloned().unwrap_or_default();
    let last: Vec<&str> = tail.lines().rev().take(4).collect();
    if !last.is_empty() {
        out.gap();
        out.text("LAUNCHER LOG", bold(MUTED));
        for line in last.into_iter().rev() {
            out.text(line.to_owned(), fg(MUTED));
        }
    }
    if let Some(log) = live.logs.first() {
        let tail = app.tails.get(&log.display().to_string()).cloned().unwrap_or_default();
        out.gap();
        out.text(format!("WORKLOAD LOG {}", log.display()), bold(MUTED));
        for line in tail.lines().filter(|l| !l.trim().is_empty()).rev().take(6).collect::<Vec<_>>().into_iter().rev() {
            out.text(line.to_owned(), fg(MUTED));
        }
    }
    if live.task.is_some() {
        actions.insert(0, ("Run now".into(), Action::RunAutomation(a.key.clone())));
        actions.push((if a.enabled { "Disable".into() } else { "Enable".into() }, Action::ToggleAutomation(a.key.clone())));
    }
    actions.push(("Ask doctor".into(), Action::AskDoctor(a.key.clone())));
    for (title, path) in m.outputs.iter().take(4) {
        actions.push((format!("↗ {title}"), Action::OpenTarget(path.clone())));
    }
    actions.push(("Manifest".into(), Action::OpenAutomationFile(a.key.clone(), 0)));
    actions.push(("Log".into(), Action::OpenAutomationFile(a.key.clone(), 1)));
    if !live.logs.is_empty() {
        actions.push(("Workload log".into(), Action::OpenAutomationFile(a.key.clone(), 2)));
    }
}

/// Button rows (wrapped to `width`), as (row, x offset, label, action).
fn button_rows(items: Vec<(String, Action)>, width: i32) -> Vec<(i32, i32, String, Action)> {
    let (mut row, mut x) = (0, 0);
    let mut out = Vec::new();
    for (label, action) in items {
        let text = format!(" {label} ");
        let w = crate::paint::text_width(&text) as i32;
        if x > 0 && x + w > width {
            row += 1;
            x = 0;
        }
        out.push((row, x, text, action));
        x += w + 2;
    }
    out
}

fn buttons(app: &mut App, buf: &mut Buffer, x: i32, y: i32, rows: Vec<(i32, i32, String, Action)>, clip: Rect, primary_first: bool) {
    for (i, (row, dx, text, action)) in rows.into_iter().enumerate() {
        let (bx, by) = (x + dx, y + row);
        let style = if primary_first && i == 0 { bold(ratatui::style::Color::Black).bg(ACCENT) } else { Style::default().fg(ACCENT).bg(PANEL_HI) };
        let end = put(buf, bx, by, &text, style, clip);
        app.layout.hits.push((Rect::new(bx as u16, by as u16, (end - bx).max(0) as u16, 1), Hit::Toolbar(action)));
    }
}

/// How a session continues its conversation, for the details panel.
fn resume_how(kind: crate::agents::AgentKind) -> &'static str {
    match kind {
        crate::agents::AgentKind::Copilot => "copilot --session-id",
        crate::agents::AgentKind::OpenCode => "opencode -s",
        crate::agents::AgentKind::Claude => "claude -r",
    }
}

/// A session or subagent box that does not run here: what it is and what
/// starting it would do. Returns whether the first action (Start, Resume,
/// Open terminal) is the primary one.
fn session_detail(app: &App, out: &mut Lines, actions: &mut Vec<(String, Action)>, node: &crate::doc::Node) -> bool {
    use crate::doc::NodeKind;
    use crate::interact::StartPlan;
    let now = crate::paths::now_ms();
    let status = app.node_status(node);
    let summary = app.summary(node).cloned();
    out.text(format!("{}{}", crate::ui_canvas::box_prefix(node), node.name), bold(TEXT));
    let mut line = format!("{} {}", status.icon(app.tick), crate::ui_canvas::box_label(node, status));
    if app.live_elsewhere(node) && status != Status::External {
        line.push_str(" · ↗ open elsewhere");
    }
    out.text(line, fg(status.color()));
    out.gap();
    if node.kind == NodeKind::Agent {
        let parent = node.parent.as_deref().and_then(|p| app.doc.node(p)).cloned();
        let agent = summary.as_ref().and_then(|s| s.agents.iter().find(|a| Some(&a.key) == node.agent_key.as_ref())).cloned();
        out.field("Subagent of", &parent.as_ref().map(|p| format!("“{}”", p.name)).unwrap_or_else(|| "a removed session".into()));
        out.field("Agent", parent.as_ref().filter(|p| p.command.is_none()).map(|p| crate::agents::agent_of(p).label()).unwrap_or_default());
        if let Some(agent) = &agent {
            out.field("Kind", &agent.name);
            if let Some(ms) = agent.finished_ms.filter(|ms| *ms > 0) {
                out.field("Finished", &format!("{} ago", crate::paths::format_age(ms, now)));
            }
            if !agent.description.is_empty() {
                out.gap();
                out.text(agent.description.clone(), fg(TEXT));
            }
        }
        out.gap();
        out.text("A subagent runs inside its parent session's terminal.", fg(MUTED));
        // Its transcript, and whether its own conversation can resume on its own.
        let inspectable = app.transcript_request(&node.id).is_some();
        if inspectable {
            actions.push(("Inspect transcript".into(), Action::Inspect(node.id.clone())));
        }
        match app.child_resume(node) {
            crate::inspect::ChildResume::Allowed { native } => {
                out.text(format!("Finished and not running anywhere: it can resume in a terminal of its own ({native}), as a subsession linked to its parent."), fg(ACCENT));
                actions.push(("Resume as subsession".into(), Action::ResumeChild(node.id.clone())));
            }
            crate::inspect::ChildResume::Existing(id) => {
                let name = app.doc.node(&id).map(|n| n.name.clone()).unwrap_or_default();
                out.text(format!("Resumed before as its own subsession “{name}”."), fg(ACCENT));
                actions.push(("Open its subsession".into(), Action::Start(id)));
            }
            crate::inspect::ChildResume::No(reason) if inspectable => out.text(reason, fg(MUTED)),
            crate::inspect::ChildResume::No(_) => {}
        }
        if let Some(parent) = parent {
            match app.start_plan(&parent) {
                StartPlan::Focus => actions.push(("Open parent terminal".into(), Action::Start(parent.id.clone()))),
                StartPlan::Blocked(reason) => out.text(reason, fg(crate::paint::PURPLE)),
                plan => actions.push((format!("{} parent", plan.label().unwrap_or("Start")), Action::Start(parent.id.clone()))),
            }
        }
        actions.push(("Remove box".into(), Action::RemoveNode(node.id.clone())));
        return inspectable;
    }
    let kind = crate::agents::agent_of(node);
    let shell = node.command.is_some();
    out.field("Agent", if shell { "none (runs its command)" } else { kind.label() });
    out.field("Model", &app.session_model_text(node));
    let cwd = node.cwd.clone().filter(|c| !c.is_empty()).or_else(|| summary.as_ref().map(|s| s.cwd.clone())).unwrap_or_default();
    out.field_left("Folder", &cwd);
    if let Some(s) = &summary {
        out.field("Branch", &s.branch);
        out.field("Repo", &s.repository);
        if !shell {
            let client = if s.client.is_empty() && crate::agents::agent_of_summary(s) == crate::agents::AgentKind::Copilot { "unknown" } else { crate::app::client_label(s) };
            out.field("Client", client);
        }
        let last = s.updated_ms.max(s.last_event_ms);
        if last > 0 {
            out.field("Last used", &format!("{} ago", crate::paths::format_age(last, now)));
        }
        if !s.agents.is_empty() {
            out.field("Subagents", &s.agents.len().to_string());
        }
        if s.truncated {
            out.text("Activity from the most recent 1 MiB of history.", fg(MUTED));
        }
    }
    if let Some(parent) = node.parent.as_deref().and_then(|p| app.doc.node(p)) {
        out.field("Subsession", &format!("of “{}”", parent.name));
    }
    if !shell && !app.demo {
        out.field("Session", crate::agents::native_id(node));
    }
    out.gap();
    let plan = app.start_plan(node);
    match &plan {
        StartPlan::Focus => out.text("Running in this canvas: Open terminal to type in it.", fg(ACCENT)),
        StartPlan::Start if shell => out.text("Start runs its command in a new canvas terminal.", fg(ACCENT)),
        StartPlan::Start => out.text(format!("Start launches {} in this folder as a new session.", kind.label()), fg(ACCENT)),
        StartPlan::Resume => out.text(format!("Resume continues this conversation in {} ({} …) in a canvas terminal.", kind.label(), resume_how(kind)), fg(ACCENT)),
        StartPlan::Blocked(reason) => out.text(reason.clone(), fg(crate::paint::PURPLE)),
    }
    if let Some(error) = app.node_errors.get(&node.id) {
        out.text(format!("Problem: {error}"), fg(NEGATIVE));
    }
    let mut primary = false;
    if let Some(label) = plan.label() {
        if plan != StartPlan::Focus {
            out.text(format!("Double-click the box or press Enter to {} it.", label.to_lowercase()), fg(MUTED));
            actions.push((format!("▸ {label}"), Action::Start(node.id.clone())));
        } else {
            actions.push((label.to_owned(), Action::Start(node.id.clone())));
        }
        primary = true;
    }
    out.text("S subsession · R rename · G group · X stop", fg(MUTED));
    if app.transcript_request(&node.id).is_some() {
        actions.push(("Inspect transcript".into(), Action::Inspect(node.id.clone())));
    }
    if app.owned_alive(&node.id) {
        actions.push(("Stop".into(), Action::Stop { id: node.id.clone(), force: false }));
    }
    if !app.demo && !shell {
        actions.push(("MCP servers…".into(), Action::McpServers(node.id.clone())));
    }
    actions.push(("Remove".into(), Action::RemoveNode(node.id.clone())));
    primary
}

/// "in 4m" / "3h ago" for a time in ms ("—" for none).
fn at_ms(ms: u64) -> String {
    if ms == 0 { "—".to_owned() } else { when(&crate::paths::format_rfc3339_ms(ms)) }
}

/// The management panel of the canvas-wide agents (top bar's ◎ Agents, the
/// frame's chip): each agent's state and controls, the canvas skills, and
/// the group passive sessions for contrast.
fn canvas_agents_panel(app: &App, out: &mut Lines, actions: &mut Vec<(String, Action)>) {
    use crate::canvas_wide::{AgentAction, AgentState, MARK};
    let agents: Vec<crate::doc::Node> = app.canvas_agents().into_iter().cloned().collect();
    out.text(format!("{MARK} Canvas agents · {}", agents.len()), bold(TEXT));
    out.text(
        "Canvas-wide agents watch every group of this canvas: events from any session (pr.* anywhere), schedules, messages to any session, and the canvas skills you give them. A group's passive sessions (⟲) see only their own group.",
        fg(MUTED),
    );
    let unavailable = app.canvas_agents_unavailable();
    if let Some(why) = &unavailable {
        out.text(format!("Unavailable here: {why}"), fg(NEGATIVE));
    }
    out.gap();
    if agents.is_empty() && unavailable.is_none() {
        out.text("No canvas-wide agent yet: the New canvas agent button below makes one (PR coordinator, Cross-group digest or Custom).", fg(MUTED));
    }
    for node in &agents {
        let state = app.canvas_agent_state(node);
        let passive = node.passive.clone().unwrap_or_default();
        let view = app.passive_view(node).cloned().unwrap_or_default();
        out.link(&format!("{MARK} {}  {} {}", node.name, state.icon(app.tick), state.word()), bold(state.color()), Action::Agents(AgentAction::Show(node.id.clone())));
        let mut facts = vec![App::passive_trigger(&passive)];
        if !passive.groups.is_empty() {
            facts.push(format!("groups: {}", app.agent_groups_text(&passive)));
        }
        if !passive.skills.is_empty() {
            facts.push(format!("skills: {}", passive.skills.join(", ")));
        }
        if view.next_run_ms > 0 && !passive.paused && !passive.disabled {
            facts.push(format!("next {}", at_ms(view.next_run_ms)));
        }
        out.text(format!("   {}", facts.join(" · ")), fg(MUTED));
        let detail = match &state {
            AgentState::Monitoring | AgentState::Waiting | AgentState::Paused | AgentState::Off if !view.headline.is_empty() => format!("{} · last: {}", state.label(), view.headline),
            _ => state.label(),
        };
        if detail != state.word() {
            out.text(format!("   {detail}"), fg(state.color()));
        }
        let mut buttons: Vec<(String, Action)> = Vec::new();
        if !passive.disabled {
            if !passive.paused && !matches!(state, AgentState::Unavailable(_)) {
                buttons.push(("▸ Run now".into(), Action::PassiveRun(node.id.clone())));
            }
            buttons.push((if passive.paused { "▶ Resume".into() } else { "‖ Pause".into() }, Action::PassivePause(node.id.clone())));
        }
        if view.running || app.owned_alive(&node.id) {
            buttons.push(("■ Stop".into(), Action::Agents(AgentAction::Stop(node.id.clone()))));
        }
        buttons.push((if passive.disabled { "⏻ Enable".into() } else { "⏻ Disable".into() }, Action::Agents(AgentAction::Toggle(node.id.clone()))));
        buttons.push(("Open".into(), Action::Agents(AgentAction::Show(node.id.clone()))));
        out.buttons(3, buttons);
    }
    if let Some(summary) = app.prs_summary() {
        out.gap();
        out.text(format!("{} PULL REQUESTS · {summary}", crate::prs::MARK), bold(MUTED));
        out.link(&format!("{} Tracked pull requests and their follow-ups ▸", crate::prs::MARK), fg(TEXT), Action::Prs(crate::prs::PrAction::Panel));
    }
    out.gap();
    match &app.canvas_skills {
        Some(Ok(skills)) => {
            out.text(format!("CANVAS SKILLS · {}", skills.len()), bold(MUTED));
            if skills.is_empty() {
                out.text("None yet: New skill (below) writes a SKILL.md (name, description, rules); an agent's Skills button gives it to that agent.", fg(MUTED));
            }
            for skill in skills {
                let users = agents.iter().filter(|n| n.passive.as_ref().is_some_and(|p| p.skills.contains(&skill.name))).count();
                let about = if skill.problem.is_empty() { skill.description.clone() } else { format!("⚠ {}", skill.problem) };
                out.link(&format!("✦ {} — {about} · {users} agent(s)", skill.name), fg(if skill.problem.is_empty() { TEXT } else { ATTENTION }), Action::Agents(AgentAction::OpenSkill(skill.name.clone())));
            }
        }
        Some(Err(why)) if unavailable.is_none() => out.text(format!("Canvas skills unavailable: {why}"), fg(NEGATIVE)),
        _ => {}
    }
    out.gap();
    let group_passives: Vec<&crate::doc::Node> = app.doc.nodes.iter().filter(|n| n.passive.is_some() && !n.is_canvas_agent()).collect();
    out.text(format!("GROUP PASSIVE SESSIONS · {} (⟲ each sees only its own group)", group_passives.len()), bold(MUTED));
    for node in group_passives.iter().take(8) {
        let group = node.passive.as_ref().and_then(|p| app.doc.group(&p.group)).map(|g| g.name.clone()).unwrap_or_default();
        out.link(&format!("⟲ {} — {group} · {}", node.name, app.passive_line(node)), fg(TEXT), Action::PassiveDetail(node.id.clone()));
    }
    if let Some(dir) = app.canvas_agents_dir() {
        out.gap();
        out.field_left("Folder", &dir.display().to_string());
    }
    if unavailable.is_none() {
        actions.push(("+ New canvas agent".into(), Action::Agents(AgentAction::New)));
        actions.push(("Skills…".into(), Action::Agents(AgentAction::Library)));
        actions.push(("New skill…".into(), Action::Agents(AgentAction::NewSkill(None))));
        actions.push(("Send event…".into(), Action::Agents(AgentAction::SendEvent)));
        actions.push(("Folder".into(), Action::Agents(AgentAction::Folder)));
    }
    if app.doc.canvas_frame().is_some() {
        actions.push(("Show frame".into(), Action::Agents(AgentAction::Reveal)));
    }
}

/// One canvas-wide agent's panel: what it watches, its skills and state,
/// recent events of the canvas, and every control (none starts it: Open
/// chat is the explicit start).
fn canvas_agent_panel(app: &App, out: &mut Lines, actions: &mut Vec<(String, Action)>, node: &crate::doc::Node) {
    use crate::canvas_wide::{AgentAction, AgentState, MARK};
    use crate::dialogs::PassiveField;
    let id = node.id.clone();
    let passive = node.passive.clone().unwrap_or_default();
    let view = app.passive_view(node).cloned().unwrap_or_default();
    let state = app.canvas_agent_state(node);
    out.text(format!("{MARK} {}", node.name), bold(TEXT));
    out.text(format!("{} canvas-wide agent · {}", state.icon(app.tick), state.label()), bold(state.color()));
    if !view.headline.is_empty() && !matches!(state, AgentState::Attention(_) | AgentState::Done(_)) {
        out.text(format!("Last report: {} {}", view.status, view.headline), fg(TEXT));
    }
    out.gap();
    out.field("Watches", &if passive.groups.is_empty() { "every group of this canvas".to_owned() } else { format!("the groups {}", app.agent_groups_text(&passive)) });
    out.field("Reacts to", &App::passive_trigger(&passive));
    out.field("Filter", &passive.filter);
    let library = app.canvas_skills.as_ref().and_then(|s| s.as_ref().ok());
    let skills = if passive.skills.is_empty() {
        "none yet (its Skills button gives it canvas skills)".to_owned()
    } else {
        passive.skills.iter().map(|s| if library.is_some_and(|l| !l.iter().any(|k| &k.name == s)) { format!("{s} (missing)") } else { s.clone() }).collect::<Vec<_>>().join(", ")
    };
    out.field("Skills", &skills);
    if let Some(settings) = &passive.prs {
        let how = format!(
            "on: every {} min{} · follow-ups {}",
            settings.poll_minutes,
            if settings.auto_track { ", created ones tracked automatically" } else { "" },
            if settings.allow.is_empty() { "read-only".to_owned() } else { format!("may ask to {}", settings.allow.join(", ")) }
        );
        out.field("PRs", &format!("{how} · {}", app.prs_summary().unwrap_or_else(|| "none tracked yet".into())));
    }
    out.field("Template", &passive.template);
    let kind = crate::agents::agent_of(node);
    let model = canvas_model::models::effective(kind, &app.doc.defaults, app.doc.group(&passive.group)).model;
    let model = node.model.clone().filter(|m| !m.is_empty()).unwrap_or(model);
    out.field("Agent", &format!("{} · runs with {}", kind.label(), if model.is_empty() { "its own default model" } else { &model }));
    let limit = if passive.max_runs_per_day == 0 { "no daily limit".to_owned() } else { format!("of {} a day", passive.max_runs_per_day) };
    out.field("Runs today", &format!("{} {limit}", view.runs_today));
    let rollover = if passive.rollover_runs == 0 { "never".to_owned() } else { format!("after {} runs", passive.rollover_runs) };
    out.field("Fresh chat", &format!("{rollover} · conversation {} ({} earlier)", view.episode + 1, passive.episodes.len()));
    out.field("Last run", &at_ms(view.last_run_ms));
    out.field("Next run", &if passive.disabled || passive.paused { "—".to_owned() } else if view.next_run_ms == 0 { "when an event arrives".to_owned() } else { at_ms(view.next_run_ms) });
    if view.pending > 0 {
        out.field("Queued", &format!("{} event(s)", view.pending));
    }
    out.field("Last error", &view.last_error);
    if view.stopped_ms > 0 {
        out.field("Stopped", &format!("by you {}", at_ms(view.stopped_ms)));
    }
    if let Some(paths) = app.passive_paths(node) {
        out.field_left("Folder", &paths.dir.display().to_string());
    }
    let canvas = app.doc.canvas_id.clone().unwrap_or_default();
    let recent: Vec<String> = app
        .bus
        .recent
        .iter()
        .rev()
        .filter(|e| !e.kind.starts_with("passive.") && !e.kind.starts_with("canvas.") && (e.canvas == canvas || app.doc.group(&e.group).is_some()))
        .take(6)
        .map(|e| {
            let group = app.doc.group(&e.group).map(|g| g.name.clone()).unwrap_or_else(|| "no group".into());
            format!("{} · {} · {group} · {}", e.kind, if e.source_name.is_empty() { "canvas" } else { &e.source_name }, at_ms(e.ms))
        })
        .collect();
    if !recent.is_empty() {
        out.gap();
        out.text("RECENT CANVAS EVENTS (every group)", bold(MUTED));
        for line in recent {
            out.text(line, fg(TEXT));
        }
    }
    if !passive.disabled {
        actions.push(("Run now".into(), Action::PassiveRun(id.clone())));
        actions.push((if passive.paused { "Resume".into() } else { "Pause".into() }, Action::PassivePause(id.clone())));
    }
    if view.running || app.owned_alive(&id) {
        actions.push(("Stop run".into(), Action::Agents(AgentAction::Stop(id.clone()))));
    }
    actions.push((if passive.disabled { "Enable".into() } else { "Disable".into() }, Action::Agents(AgentAction::Toggle(id.clone()))));
    actions.push(("Events…".into(), Action::PassiveEdit(id.clone(), PassiveField::Events)));
    actions.push(("Groups…".into(), Action::PassiveEdit(id.clone(), PassiveField::Groups)));
    actions.push(("Schedule…".into(), Action::PassiveEdit(id.clone(), PassiveField::Schedule)));
    actions.push(("Filter…".into(), Action::PassiveEdit(id.clone(), PassiveField::Filter)));
    actions.push(("Limits…".into(), Action::PassiveEdit(id.clone(), PassiveField::Limits)));
    actions.push(("Skills…".into(), Action::Agents(AgentAction::Skills(id.clone()))));
    actions.push(("PRs…".into(), Action::PassiveEdit(id.clone(), PassiveField::Prs)));
    if passive.prs.is_some() {
        actions.push(("Pull requests".into(), Action::Prs(crate::prs::PrAction::Panel)));
    }
    actions.push(("MCP…".into(), Action::McpServers(id.clone())));
    actions.push(("Rules".into(), Action::PassiveOpen(id.clone(), 0)));
    actions.push(("Memory".into(), Action::PassiveOpen(id.clone(), 1)));
    actions.push(("Log".into(), Action::PassiveOpen(id.clone(), 2)));
    actions.push(("Context".into(), Action::Visual(crate::visual::VisualAction::PassiveFile(id.clone(), 4))));
    actions.push(("Goals".into(), Action::Visual(crate::visual::VisualAction::PassiveFile(id.clone(), 5))));
    actions.push(("Folder".into(), Action::PassiveOpen(id.clone(), 3)));
    actions.push(("Message…".into(), Action::MessagePassive(id.clone())));
    actions.push(("Open chat".into(), Action::PassiveChat(id.clone())));
    actions.push(("All agents".into(), Action::Agents(AgentAction::Panel)));
    actions.push(("Remove".into(), Action::RemoveNode(id)));
}

/// "3m ago" / "in 4m" against the app's clock ("—" for none).
fn age_at(app: &App, ms: u64) -> String {
    let now = app.visual_now();
    match ms {
        0 => "—".to_owned(),
        ms if ms <= now => format!("{} ago", crate::paths::format_age(ms, now)),
        ms => format!("in {}", crate::paths::format_age(now, ms)),
    }
}

/// The routed canvas's tracked pull requests: each one's state, owner and
/// monitor, its follow-ups (queued, delivered, blocked with what to do,
/// failed) and buttons. Nothing here starts a session but *Resume owner*.
fn prs_panel(app: &App, out: &mut Lines, actions: &mut Vec<(String, Action)>) {
    use crate::prs::{MARK, PrAction, dispatch_look, pr_look};
    use canvas_model::prs::{DispatchState, MonitorStatus, permissions_text};
    let Some(view) = app.prs_view() else {
        out.text(format!("{MARK} Pull requests"), bold(TEXT));
        out.text("No pull request is tracked on this canvas, and no canvas-wide agent has PR coordination. ◎ Agents › + New canvas agent › PR coordinator turns it on; sessions then hand off what they publish with copilot-canvas pr track <url>.", fg(MUTED));
        actions.push(("Canvas agents".into(), Action::Agents(crate::canvas_wide::AgentAction::Panel)));
        return;
    };
    let needing = view.needing_you();
    out.text(format!("{MARK} Pull requests · {} tracked · {} active{}", view.prs.len(), view.active(), if needing > 0 { format!(" · {needing} need you") } else { String::new() }), bold(TEXT));
    if view.paused.is_empty() {
        out.text(format!("Monitored read-only (gh / az) for {}; follow-ups reach owners when they are idle.", view.coordinators.join(", ")), fg(MUTED));
    } else {
        out.text(format!("Not monitored now: {}.", view.paused), fg(ATTENTION));
    }
    out.text(format!("Follow-ups: {}.", permissions_text(&view.allow)), fg(MUTED));
    if view.prs.is_empty() {
        out.gap();
        out.text("Nothing tracked yet: a session hands off a pull request it publishes with copilot-canvas pr track <url> (the canvas-prs skill teaches it).", fg(MUTED));
    }
    for pr in &view.prs {
        out.gap();
        let (glyph, color) = pr_look(pr);
        let title = if pr.title.is_empty() { String::new() } else { format!(" “{}”", pr.title) };
        if pr.state.ended() {
            // Ended: one line (it stays listed for 30 days).
            out.link(&format!("{glyph} {}{title} · {} {} · monitoring stopped", pr.pr.short(), pr.state.word(), age_at(app, pr.last_change_ms)), fg(color), Action::Prs(PrAction::Open(pr.url.clone())));
            continue;
        }
        out.link(&format!("{glyph} {}{title} · {}{}", pr.pr.short(), pr.state.word(), if pr.draft { " (draft)" } else { "" }), bold(color), Action::Prs(PrAction::Open(pr.url.clone())));
        let owner = if pr.owner_name.is_empty() { pr.owner.chars().take(8).collect() } else { pr.owner_name.clone() };
        let monitor = match pr.monitor.status {
            MonitorStatus::Pending => "not checked yet".to_owned(),
            MonitorStatus::Unavailable => format!("unavailable: {}", pr.monitor.reason),
            MonitorStatus::Failing => format!("failing ({}×): {} · next {}", pr.monitor.failures, pr.monitor.reason, age_at(app, pr.monitor.next_ms)),
            _ => format!("checked {} · next {}", age_at(app, pr.monitor.last_ms), if view.paused.is_empty() { age_at(app, pr.monitor.next_ms) } else { "when monitored".to_owned() }),
        };
        let monitor_color = if matches!(pr.monitor.status, MonitorStatus::Unavailable | MonitorStatus::Failing) { ATTENTION } else { MUTED };
        out.indented(2, format!("owner {owner} · {monitor}"), fg(monitor_color));
        let mut facts = Vec::new();
        if pr.comments > 0 {
            facts.push(format!("{} comment(s), {} new", pr.comments, pr.new_comments));
        }
        if !pr.failing.is_empty() {
            facts.push(format!("failing: {}", pr.failing.join(", ")));
        }
        if !pr.decision.is_empty() {
            facts.push(pr.decision.replace('_', " "));
        }
        if !pr.last_change.is_empty() {
            facts.push(format!("last: {} {}", pr.last_change, age_at(app, pr.last_change_ms)));
        }
        if !facts.is_empty() {
            out.indented(2, facts.join(" · "), fg(TEXT));
        }
        let mut buttons = vec![("Open PR".to_owned(), Action::Prs(PrAction::Open(pr.url.clone()))), ("View owner".to_owned(), Action::Prs(PrAction::Owner(pr.owner.clone()))), ("Check now".to_owned(), Action::Prs(PrAction::Check(pr.url.clone())))];
        buttons.push(("Untrack".into(), Action::Prs(PrAction::Untrack(pr.url.clone()))));
        out.buttons(2, buttons);
        // The newest follow-ups (copilot-canvas pr list has them all).
        for d in pr.dispatches.iter().rev().take(3) {
            let (glyph, color) = dispatch_look(d.state);
            let when = age_at(app, d.updated_ms.max(d.created_ms));
            out.indented(2, format!("{glyph} {} {when} → {}: {}", d.state.word(), d.to_name, d.text), fg(color));
            if !d.reason.is_empty() {
                out.indented(4, &d.reason, fg(color));
            }
            let mut buttons = Vec::new();
            if d.state == DispatchState::Blocked && app.doc.node(&d.to).is_some() && !app.owned_alive(&d.to) {
                buttons.push(("▸ Resume owner".to_owned(), Action::Start(d.to.clone())));
            }
            if matches!(d.state, DispatchState::Blocked | DispatchState::Failed) {
                if d.state == DispatchState::Failed {
                    buttons.push(("Retry".into(), Action::Prs(PrAction::Retry(d.id.clone()))));
                }
                buttons.push(("Reassign…".into(), Action::Prs(PrAction::Reassign(d.id.clone()))));
            }
            if d.state.open() || d.state == DispatchState::Failed {
                buttons.push(("Dismiss".into(), Action::Prs(PrAction::Dismiss(d.id.clone()))));
            }
            if !buttons.is_empty() {
                out.buttons(4, buttons);
            }
        }
    }
    if view.active() > 0 {
        actions.push(("Check all now".into(), Action::Prs(PrAction::Check(String::new()))));
    }
    actions.push(("Canvas agents".into(), Action::Agents(crate::canvas_wide::AgentAction::Panel)));
}

pub fn draw_right_detail(app: &mut App, buf: &mut Buffer, area: Rect, detail: &RightDetail) {
    outline(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, &LIGHT, fg(FAINT), area);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(3));
    let mut out = Lines { lines: Vec::new(), width: inner.width as usize, spans: Default::default(), clicks: Vec::new() };
    let mut actions: Vec<(String, Action)> = Vec::new();
    let mut primary = false;
    let config = app.config.clone().unwrap_or_default();
    match detail {
        RightDetail::Session(id) => match app.doc.node(id).cloned() {
            None => out.text("This box was removed.", fg(MUTED)),
            Some(node) => primary = session_detail(app, &mut out, &mut actions, &node),
        },
        RightDetail::Automation(key) => match app.automation(key).cloned() {
            None => out.text("This automation no longer exists.", fg(MUTED)),
            Some(a) if a.factory.is_some() => {
                let live = a.factory.clone().expect("checked");
                task_detail(app, &mut out, &mut actions, &a, &live);
            }
            Some(a) => {
                let status = automation_status(&a);
                out.text(format!("◷ {}", a.name), bold(TEXT));
                out.text(format!("{} {}{}", status.icon(app.tick), if a.enabled { "enabled" } else { "disabled" }, if a.last_status().is_empty() { String::new() } else { format!(" · last run {}", a.last_status()) }), fg(status.color()));
                out.gap();
                out.field("Kind", &a.kind);
                out.field("Schedule", &a.schedule);
                out.field("Last run", &when(&a.last_run_at));
                if a.enabled {
                    out.field("Next run", &when(&a.next_run_at));
                }
                out.field("Model", &a.model);
                out.field("Effort", &a.reasoning_effort);
                out.field("Mode", &a.mode);
                out.field("Project", &a.project);
                out.field("Workspace", &a.workspace);
                out.field("Session", &a.session_id);
                out.field("Updated", &when(&a.updated_at));
                out.gap();
                out.text("PROMPT", bold(MUTED));
                out.text(if a.prompt.is_empty() { "(empty)".to_owned() } else { a.prompt.clone() }, fg(TEXT));
                if !a.runs.is_empty() {
                    out.gap();
                    out.text("RECENT RUNS", bold(MUTED));
                    for run in &a.runs {
                        let mut line = format!("● {:<10} {:<14} {}", run.status, when(&run.started_at), run.trigger);
                        if !run.error.is_empty() {
                            line.push_str(&format!(" — {}", run.error));
                        }
                        out.text(line, fg(status_color(&run.status)));
                    }
                }
                if a.toggleable {
                    actions.push((if a.enabled { "Disable".into() } else { "Enable".into() }, Action::ToggleAutomation(a.key.clone())));
                }
                out.gap();
                out.text(format!("Source: {}", app.automation_db().display()), fg(MUTED));
            }
        },
        RightDetail::CanvasAgents => canvas_agents_panel(app, &mut out, &mut actions),
        RightDetail::Prs => prs_panel(app, &mut out, &mut actions),
        RightDetail::Passive(id) if app.doc.node(id).is_some_and(|n| n.is_canvas_agent()) => {
            let node = app.doc.node(id).cloned().expect("checked");
            canvas_agent_panel(app, &mut out, &mut actions, &node);
        }
        RightDetail::Passive(id) => match app.doc.node(id).cloned() {
            None => out.text("This passive session was removed.", fg(MUTED)),
            Some(node) => {
                use crate::dialogs::PassiveField;
                let passive = node.passive.clone().unwrap_or_default();
                let view = app.passive_view(&node).cloned().unwrap_or_default();
                let status = app.node_status(&node);
                let color = match view.status.as_str() {
                    "alert" => ATTENTION,
                    "warn" => ATTENTION,
                    "done" => crate::paint::FINISHED,
                    _ => status.color(),
                };
                out.text(format!("⟲ {}", node.name), bold(TEXT));
                out.text(format!("{} {}", status.icon(app.tick), app.passive_line(&node)), fg(color));
                out.gap();
                out.field("Template", &passive.template);
                out.field("Reacts to", &App::passive_trigger(&passive));
                out.field("Filter", &passive.filter);
                out.field("Group", &app.doc.group(&passive.group).map(|g| g.name.clone()).unwrap_or_default());
                out.field("Scope", "this group's events only (◎ Agents: canvas-wide agents)");
                let limit = if passive.max_runs_per_day == 0 { "no daily limit".to_owned() } else { format!("of {} a day", passive.max_runs_per_day) };
                out.field("Runs today", &format!("{} {limit}", view.runs_today));
                let rollover = if passive.rollover_runs == 0 { "never".to_owned() } else { format!("after {} runs", passive.rollover_runs) };
                out.field("Fresh chat", &format!("{rollover} · conversation {} ({} earlier)", view.episode + 1, passive.episodes.len()));
                let at = |ms: u64| if ms == 0 { "—".to_owned() } else { when(&crate::paths::format_rfc3339_ms(ms)) };
                out.field("Last run", &at(view.last_run_ms));
                out.field("Next run", &if view.next_run_ms == 0 { "when an event arrives".to_owned() } else { at(view.next_run_ms) });
                if view.pending > 0 {
                    out.field("Queued", &format!("{} event(s)", view.pending));
                }
                out.field("Last error", &view.last_error);
                if let Some(paths) = app.passive_paths(&node) {
                    out.field("Folder", &paths.dir.display().to_string());
                }
                let recent: Vec<String> = app
                    .bus
                    .recent
                    .iter()
                    .rev()
                    .filter(|e| e.group == passive.group && !e.kind.starts_with("passive."))
                    .take(6)
                    .map(|e| format!("{} · {} · {}", e.kind, if e.source_name.is_empty() { "canvas" } else { &e.source_name }, at(e.ms)))
                    .collect();
                if !recent.is_empty() {
                    out.gap();
                    out.text("RECENT GROUP EVENTS", bold(MUTED));
                    for line in recent {
                        out.text(line, fg(TEXT));
                    }
                }
                actions.push(("Run now".into(), Action::PassiveRun(id.clone())));
                actions.push((if passive.paused { "Resume".into() } else { "Pause".into() }, Action::PassivePause(id.clone())));
                actions.push(("Events…".into(), Action::PassiveEdit(id.clone(), PassiveField::Events)));
                actions.push(("Schedule…".into(), Action::PassiveEdit(id.clone(), PassiveField::Schedule)));
                actions.push(("Filter…".into(), Action::PassiveEdit(id.clone(), PassiveField::Filter)));
                actions.push(("Limits…".into(), Action::PassiveEdit(id.clone(), PassiveField::Limits)));
                actions.push(("MCP…".into(), Action::McpServers(id.clone())));
                actions.push(("Rules".into(), Action::PassiveOpen(id.clone(), 0)));
                actions.push(("Memory".into(), Action::PassiveOpen(id.clone(), 1)));
                actions.push(("Log".into(), Action::PassiveOpen(id.clone(), 2)));
                actions.push(("Context".into(), Action::Visual(crate::visual::VisualAction::PassiveFile(id.clone(), 4))));
                actions.push(("Goals".into(), Action::Visual(crate::visual::VisualAction::PassiveFile(id.clone(), 5))));
                actions.push(("Folder".into(), Action::PassiveOpen(id.clone(), 3)));
                actions.push(("Message…".into(), Action::MessagePassive(id.clone())));
                actions.push(("Open chat".into(), Action::PassiveChat(id.clone())));
            }
        },
        RightDetail::Item(item) => match item {
            ItemRef::Plugin(name) => match config.plugins.iter().find(|p| &p.name == name) {
                Some(p) => {
                    out.text(format!("⧉ {}", p.name), bold(TEXT));
                    out.text(if p.enabled { "● enabled" } else { "○ disabled" }, fg(if p.enabled { POSITIVE } else { MUTED }));
                    out.gap();
                    out.field("Version", &p.version);
                    out.field("Marketplace", &p.marketplace);
                    out.field("Source", &p.source);
                    actions.push((if p.enabled { "Disable".into() } else { "Enable".into() }, Action::ToggleConfig(Kind::Plugin, p.name.clone(), !p.enabled)));
                    actions.push(("Update".into(), Action::UpdatePlugin(p.name.clone())));
                    // The backend found its folder when it listed the plugins.
                    if !p.local_folder.is_empty() {
                        actions.push(("Open folder".into(), Action::OpenPath(p.local_folder.clone())));
                    }
                }
                None => out.text("Plugin not found; refresh the list.", fg(MUTED)),
            },
            ItemRef::Mcp(name) => match config.mcp.iter().find(|s| &s.name == name) {
                Some(s) => {
                    out.text(format!("⌁ {}", s.name), bold(TEXT));
                    out.text(if s.enabled { "● enabled" } else { "○ disabled" }, fg(if s.enabled { POSITIVE } else { MUTED }));
                    out.gap();
                    out.field("Source", &s.source);
                    out.field("Transport", &s.transport);
                    out.field("Runs", &s.target);
                    out.gap();
                    out.text("Environment values and headers are not shown.", fg(MUTED));
                    actions.push((if s.enabled { "Disable".into() } else { "Enable".into() }, Action::ToggleConfig(Kind::Mcp, s.name.clone(), !s.enabled)));
                }
                None => out.text("Server not found; refresh the list.", fg(MUTED)),
            },
            ItemRef::Skill(name) => match config.skills.iter().find(|s| &s.name == name) {
                Some(s) => {
                    out.text(format!("✦ {}", s.name), bold(TEXT));
                    out.text(if s.enabled { "● enabled" } else { "○ disabled" }, fg(if s.enabled { POSITIVE } else { MUTED }));
                    out.gap();
                    out.field("Source", &s.source);
                    out.field("Path", &s.path);
                    out.gap();
                    out.text(s.description.clone(), fg(TEXT));
                    actions.push((if s.enabled { "Disable".into() } else { "Enable".into() }, Action::ToggleConfig(Kind::Skill, s.name.clone(), !s.enabled)));
                    if !s.path.is_empty() {
                        actions.push(("Open folder".into(), Action::OpenPath(s.path.clone())));
                    }
                }
                None => out.text("Skill not found; refresh the list.", fg(MUTED)),
            },
            ItemRef::Agent(path) => match config.agents.iter().find(|a| &a.path == path) {
                Some(a) => {
                    out.text(format!("◈ {}", a.name), bold(TEXT));
                    out.gap();
                    out.field("Source", &a.source);
                    out.field("File", &a.path);
                    out.gap();
                    out.text(a.description.clone(), fg(TEXT));
                    out.gap();
                    out.text("Use it for new sessions via Settings › Custom agent.", fg(MUTED));
                    actions.push(("Open folder".into(), Action::OpenPath(a.path.clone())));
                }
                None => out.text("Agent not found; refresh the list.", fg(MUTED)),
            },
            ItemRef::Instruction(path) => match config.instructions.iter().find(|i| &i.source_path == path) {
                Some(i) => {
                    out.text(format!("≡ {}", if i.label.is_empty() { &i.source_path } else { &i.label }), bold(TEXT));
                    out.gap();
                    out.field("Location", &i.location);
                    out.field("Type", &i.kind);
                    out.field("File", &i.source_path);
                    if i.default_disabled {
                        out.text("Disabled by default.", fg(ATTENTION));
                    }
                    actions.push(("Open folder".into(), Action::OpenPath(i.source_path.clone())));
                }
                None => out.text("Instruction source not found; refresh the list.", fg(MUTED)),
            },
            _ => out.text("Select an item.", fg(MUTED)),
        },
    }
    actions.push(("Close".into(), Action::CloseDetail));
    let rows = button_rows(actions, inner.width as i32);
    let button_lines = rows.last().map(|r| r.0 + 1).unwrap_or(0);
    let max_lines = (inner.height as i32 - 1 - button_lines).max(1) as usize;
    let truncated = out.lines.len() > max_lines;
    for (i, (text, style)) in out.lines.iter().take(max_lines).enumerate() {
        let y = inner.y as i32 + i as i32;
        match out.spans.get(&i) {
            Some(spans) => {
                let mut x = inner.x as i32;
                for (piece, style) in spans {
                    x = put(buf, x, y, piece, *style, inner);
                }
            }
            None => {
                put(buf, inner.x as i32, y, &truncate(text, inner.width as usize), *style, inner);
            }
        }
        for (_, dx, w, action) in out.clicks.iter().filter(|(line, ..)| *line == i) {
            let x = inner.x as i32 + dx;
            let w = (*w).min(inner.right() as i32 - x);
            if w > 0 {
                app.layout.hits.push((Rect::new(x as u16, y as u16, w as u16, 1), Hit::Toolbar(action.clone())));
            }
        }
    }
    if truncated {
        put(buf, inner.x as i32, inner.y as i32 + max_lines as i32, "…", fg(MUTED), inner);
    }
    let clip = Rect::new(area.x, area.y, area.width, area.height);
    buttons(app, buf, inner.x as i32, area.bottom() as i32 - 1 - button_lines, rows, clip, primary);
}
