//! Drawing for the activity bar, the non-session side views and the right-hand
//! detail panel (automations and configuration items).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::app::{App, Focus};
use crate::dialogs::{Action, Hit};
use crate::paint::{ACCENT, FAINT, GRAY, POSITIVE, LIGHT, MUTED, NEGATIVE, PANEL, PANEL_HI, SELECT, SPINNER, TEXT, ATTENTION, bold, fg, fill, outline, put, truncate};
use crate::status::{Status, StatusStyle};
use crate::side::{ItemRef, RightDetail, SideView, VIEWS, automation_status, status_color, when};
use crate::config::Kind;

pub const BAR_W: u16 = 3;

pub fn draw_activity_bar(app: &mut App, buf: &mut Buffer, area: Rect) {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(ratatui::style::Color::Rgb(13, 17, 23)), area);
    for (i, (view, icon, title)) in VIEWS.iter().enumerate() {
        let y = area.y as i32 + 1 + i as i32 * 2;
        if y >= area.bottom() as i32 {
            break;
        }
        let active = app.side_view == *view && app.doc.ui.sidebar_visible;
        let style = if active { bold(SELECT).bg(PANEL_HI) } else { fg(GRAY).bg(ratatui::style::Color::Rgb(13, 17, 23)) };
        if active {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(PANEL_HI), area);
            put(buf, area.x as i32, y, "▌", fg(ACCENT).bg(PANEL_HI), area);
        }
        put(buf, area.x as i32 + 1, y, icon, style, area);
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
            _ => {
                let mut x = put(buf, area.x as i32 + 2, y, row.icon, fg(row.color).bg(bg), area) + 1;
                // Names win: the right column only gets what the label leaves free (min 6).
                let label_w = crate::paint::text_width(&row.label) as i32;
                let free = (right_edge - x - label_w - 3).max(if row.right.is_empty() { 0 } else { 6 }) as usize;
                let right = truncate(&row.right, free.min((area.width as usize / 2).saturating_sub(2)));
                let right_w = crate::paint::text_width(&right) as i32;
                let room = (right_edge - x - right_w - 2).max(1) as usize;
                let style = if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) };
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
}

impl Lines {
    fn text(&mut self, text: impl Into<String>, style: Style) {
        for line in wrap(&text.into(), self.width) {
            self.lines.push((line, style));
        }
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

fn buttons(app: &mut App, buf: &mut Buffer, x: i32, y: i32, rows: Vec<(i32, i32, String, Action)>, clip: Rect) {
    for (row, dx, text, action) in rows {
        let (bx, by) = (x + dx, y + row);
        let end = put(buf, bx, by, &text, Style::default().fg(ACCENT).bg(PANEL_HI), clip);
        app.layout.hits.push((Rect::new(bx as u16, by as u16, (end - bx).max(0) as u16, 1), Hit::Toolbar(action)));
    }
}

pub fn draw_right_detail(app: &mut App, buf: &mut Buffer, area: Rect, detail: &RightDetail) {
    outline(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, &LIGHT, fg(FAINT), area);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(3));
    let mut out = Lines { lines: Vec::new(), width: inner.width as usize, spans: Default::default() };
    let mut actions: Vec<(String, Action)> = Vec::new();
    let config = app.config.clone().unwrap_or_default();
    match detail {
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
    }
    if truncated {
        put(buf, inner.x as i32, inner.y as i32 + max_lines as i32, "…", fg(MUTED), inner);
    }
    let clip = Rect::new(area.x, area.y, area.width, area.height);
    buttons(app, buf, inner.x as i32, area.bottom() as i32 - 1 - button_lines, rows, clip);
}
