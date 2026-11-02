//! The reader (the Notebook's main area): tabs, the header (breadcrumb,
//! view switch, provenance pill and buttons), the conflict banner, the find
//! bar, the note (rendered: the GUI's HTML pane or formatted cells; Source
//! and Live: the editor) and the `[[` completion list.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use canvas_model::notebook::{self as nb, Author, Kind};

use super::tabs::{self, LINK_HOST, Tab, View};
use super::{EMPTY, Hit, tree};
use crate::app::{App, Focus};
use crate::dialogs::Hit as AppHit;
use crate::paint::{ACCENT, ATTENTION, ATTENTION_BG, BLUE, FAINT, GRAY, MODAL, MUTED, NEGATIVE, PANEL_HI, PURPLE, TEXT, bold, fg, fill, put, text_width, truncate, truncate_left};
use crate::view::{HtmlPane, HtmlSource, RenderMode};

fn hit(app: &mut App, rect: Rect, target: Hit) {
    app.layout.hits.push((rect, AppHit::Notebook(target)));
}

/// Pieces laid out left to right, wrapping to the next row.
struct Flow {
    left: i32,
    right: i32,
    x: i32,
    y: i32,
    clip: Rect,
    /// A separator drawn before the next piece when it stays on this row.
    sep: Option<(&'static str, Style)>,
}

impl Flow {
    fn new(area: Rect, y: i32) -> Self {
        Flow { left: area.x as i32 + 1, right: area.right() as i32 - 1, x: area.x as i32 + 1, y, clip: area, sep: None }
    }

    /// A separator before the next piece (dropped where the row wraps).
    fn sep(&mut self, text: &'static str, style: Style) {
        self.sep = Some((text, style));
    }

    fn piece(&mut self, app: &mut App, buf: &mut Buffer, text: &str, style: Style, target: Option<Hit>) {
        let sep = self.sep.take();
        let sep_width = sep.map(|(s, _)| text_width(s) as i32).unwrap_or(0);
        let width = text_width(text) as i32;
        if self.x > self.left && self.x + sep_width + width > self.right {
            self.x = self.left;
            self.y += 1;
        } else if let Some((sep, sep_style)) = sep {
            self.x = put(buf, self.x, self.y, sep, sep_style, self.clip);
        }
        let end = put(buf, self.x, self.y, &truncate(text, (self.right - self.x).max(0) as usize), style, self.clip);
        if let Some(target) = target
            && end > self.x
        {
            hit(app, Rect::new(self.x as u16, self.y as u16, (end - self.x) as u16, 1), target);
        }
        self.x = end;
    }

    /// The row after the last piece.
    fn next_row(&self) -> i32 {
        self.y + 1
    }
}

/// A button's look in the header.
fn button_style() -> Style {
    Style::default().fg(TEXT).bg(PANEL_HI)
}

/// Draws the reader; the caret (the editor's or the find field's) when it has the keyboard.
pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    app.notebook.pane = false;
    if area.width < 10 || area.height < 4 {
        return None;
    }
    if app.notebook.tabs.is_empty() {
        app.notebook.content = Rect::default();
        return draw_empty(app, buf, area);
    }
    app.notebook.active = app.notebook.active.min(app.notebook.tabs.len() - 1);
    let mut y = area.y as i32;
    draw_tabs(app, buf, area, y);
    y += 1;
    y = draw_header(app, buf, area, y);
    y = draw_banner(app, buf, area, y);
    let mut cursor = None;
    if app.notebook.find.is_some() {
        cursor = draw_find(app, buf, area, y);
        y += 1;
    }
    put(buf, area.x as i32 + 1, y, &"─".repeat(area.width.saturating_sub(2) as usize), fg(FAINT), area);
    y += 1;
    let content = Rect::new(area.x + 2, y.min(area.bottom() as i32) as u16, area.width.saturating_sub(3), (area.bottom() as i32 - y).max(0) as u16);
    app.notebook.content = content;
    let caret = draw_content(app, buf, content);
    let focused = app.focus == Focus::Canvas;
    if app.notebook.find.is_none() && focused {
        cursor = caret;
    } else if !focused {
        cursor = None;
    }
    if let Some((cx, cy)) = caret {
        draw_completion(app, buf, area, (cx, cy));
    }
    cursor
}

/// No note open: how to open one.
fn draw_empty(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let x = area.x as i32 + 2;
    let width = area.width.saturating_sub(4) as usize;
    put(buf, x, area.y as i32 + 1, "✎ Notebook", bold(ACCENT), area);
    put(buf, x, area.y as i32 + 3, &truncate(EMPTY, width), fg(TEXT), area);
    hit(app, Rect::new(x as u16, area.y + 3, (text_width(EMPTY) as u16).min(width as u16), 1), Hit::Empty);
    let hints = ["Enter in the explorer opens a note.", "Ctrl+P finds one · Alt+N quick note", "Ctrl+E Rendered ⇄ Source · Ctrl+S saves"];
    for (i, line) in hints.iter().enumerate() {
        put(buf, x, area.y as i32 + 5 + i as i32, &truncate(&app.key_policy.label(line), width), fg(MUTED), area);
    }
    None
}

/// The tab bar: `● name*  ×` per tab, the shown one lit.
fn draw_tabs(app: &mut App, buf: &mut Buffer, area: Rect, y: i32) {
    fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(Color::Reset), area);
    let labels: Vec<(String, bool)> = app
        .notebook
        .tabs
        .iter()
        .map(|t| (format!(" {} {}{} ", t.provenance.audience.glyph(), t.name, if t.unsaved { "*" } else { "" }), t.provenance.audience == nb::Audience::Ai))
        .collect();
    let active = app.notebook.active;
    // The first tab shown keeps the active one on screen.
    let widths: Vec<i32> = labels.iter().map(|(l, _)| text_width(l) as i32 + 3).collect();
    let mut first = 0;
    while first < active && widths[first..=active].iter().sum::<i32>() > area.width as i32 - 2 {
        first += 1;
    }
    let mut x = area.x as i32 + 1;
    if first > 0 {
        x = put(buf, x, y, "‹", fg(MUTED), area);
    }
    for (i, (label, ai)) in labels.iter().enumerate().skip(first) {
        if x >= area.right() as i32 - 2 {
            put(buf, area.right() as i32 - 2, y, "›", fg(MUTED), area);
            break;
        }
        let on = i == active;
        let base = if on { bold(TEXT).bg(PANEL_HI) } else if *ai { fg(MUTED).add_modifier(Modifier::ITALIC) } else { fg(GRAY) };
        let end = put(buf, x, y, label, base, area);
        if end > x {
            hit(app, Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Tab(i));
        }
        let close = put(buf, end, y, "×", if on { fg(GRAY).bg(PANEL_HI) } else { fg(FAINT) }, area);
        if close > end {
            hit(app, Rect::new(end as u16, y as u16, 1, 1), Hit::CloseTab(i));
        }
        x = put(buf, close, y, " ", if on { Style::default().bg(PANEL_HI) } else { Style::default() }, area) + 1;
    }
}

/// The agent's name as people know it (`opencode` → OpenCode).
fn agent_label(agent: &str) -> String {
    crate::agents::AgentKind::parse(agent).map(|k| k.label().to_owned()).unwrap_or_else(|| agent.to_owned())
}

/// The canvas session (node id) a `source_session` names: its id, its
/// agent's session id, else its name.
pub(super) fn session_node(app: &App, session: &str) -> Option<(String, String)> {
    let doc = &app.doc;
    doc.node(session)
        .or_else(|| doc.nodes.iter().find(|n| n.agent_session.as_deref() == Some(session)))
        .or_else(|| doc.nodes.iter().find(|n| n.name == session))
        .map(|n| (n.id.clone(), n.name.clone()))
}

/// How long ago (`created`, RFC 3339): "2h ago".
pub(super) fn age(created: Option<&str>) -> Option<String> {
    let ms = crate::paths::parse_rfc3339_ms(created?.trim())?;
    Some(format!("{} ago", crate::paths::format_age(ms, crate::paths::now_ms())))
}

/// "◇ written by OpenCode" and friends.
pub(super) fn author_line(provenance: &nb::Provenance) -> String {
    let agent = provenance.agent.as_deref().map(agent_label).unwrap_or_else(|| "an agent".into());
    match provenance.author {
        Author::Human => "written by you".into(),
        Author::Ai => format!("◇ written by {agent}"),
        Author::Mixed => format!("◇ written by {agent} and you"),
    }
}

/// What the header shows of the shown tab (its editor is not copied each frame).
struct Head {
    path: String,
    name: String,
    kind: Kind,
    view: View,
    views: Vec<View>,
    editable: bool,
    provenance: nb::Provenance,
}

impl Head {
    fn of(tab: &Tab) -> Self {
        Head { path: tab.path.clone(), name: tab.name.clone(), kind: tab.kind, view: tab.view, views: tab.views(), editable: tab.editable(), provenance: tab.provenance.clone() }
    }
}

/// The breadcrumb and view switch, then the provenance pill and the buttons.
fn draw_header(app: &mut App, buf: &mut Buffer, area: Rect, y: i32) -> i32 {
    let Some(tab) = app.notebook.tab().map(Head::of) else { return y };
    // The view switch on the right.
    let views = tab.views.clone();
    let switch: i32 = views.iter().map(|v| text_width(v.label()) as i32 + 2).sum::<i32>() + views.len() as i32 - 1;
    let mut sx = area.right() as i32 - 1 - switch;
    let crumb = format!("{} › {}", tree::place_label(app, &tab.path), tab.name);
    let room = (sx - area.x as i32 - 2).max(0) as usize;
    put(buf, area.x as i32 + 1, y, &truncate_left(&crumb, room), fg(MUTED), area);
    for (i, view) in views.iter().enumerate() {
        if i > 0 {
            sx = put(buf, sx, y, "│", fg(FAINT), area);
        }
        let style = if *view == tab.view { bold(Color::Black).bg(ACCENT) } else { fg(GRAY).bg(PANEL_HI) };
        let end = put(buf, sx, y, &format!(" {} ", view.label()), style, area);
        hit(app, Rect::new(sx as u16, y as u16, (end - sx).max(0) as u16, 1), Hit::Mode(*view));
        sx = end;
    }
    // The pill.
    let p = &tab.provenance;
    let mut flow = Flow::new(area, y + 1);
    let (audience, color) = match p.audience {
        nb::Audience::Human => ("● For you", ACCENT),
        nb::Audience::Ai => ("◇ For agents", PURPLE),
    };
    flow.piece(app, buf, audience, bold(color), None);
    let dot = fg(MUTED);
    flow.sep(" · ", dot);
    flow.piece(app, buf, &author_line(p), fg(GRAY), None);
    let session = p.session.as_deref().and_then(|s| session_node(app, s).map(|n| (s.to_owned(), Some(n))).or(Some((s.to_owned(), None))));
    if let Some((raw, node)) = &session {
        flow.sep(" · ", dot);
        match node {
            Some((id, name)) => flow.piece(app, buf, &format!("session {name}"), fg(BLUE).add_modifier(Modifier::UNDERLINED), Some(Hit::ViewSession(id.clone()))),
            None => flow.piece(app, buf, &format!("session {}", truncate(raw, 12)), fg(GRAY), None),
        }
    }
    if let Some(age) = age(p.created.as_deref()) {
        flow.sep(" · ", dot);
        flow.piece(app, buf, &age, fg(GRAY), None);
    }
    if p.reviewed {
        flow.sep(" · ", dot);
        flow.piece(app, buf, "✓ reviewed", fg(GRAY), None);
    }
    if p.inferred {
        flow.sep(" ", dot);
        flow.piece(app, buf, "(inferred)", fg(MUTED).add_modifier(Modifier::ITALIC), None);
    }
    // The buttons.
    let text_note = matches!(tab.kind, Kind::Markdown | Kind::Html);
    let mut buttons: Vec<(&str, Style, Hit)> = Vec::new();
    if let Some((_, Some((id, _)))) = &session {
        buttons.push((" View session ", button_style(), Hit::ViewSession(id.clone())));
    }
    if text_note && tab.editable && !p.reviewed && p.author != Author::Human {
        buttons.push((" Mark reviewed ", button_style(), Hit::MarkReviewed));
    }
    let gpu = app.render_mode == RenderMode::Gpu;
    if gpu && tab.view == View::Rendered && text_note && !matches!(app.reports.web, crate::reports::WebState::Failed(_)) {
        buttons.push((" Print / PDF ", button_style(), Hit::Print));
    }
    buttons.push((" Open externally ", button_style(), Hit::OpenExternally));
    if text_note {
        buttons.push((" Ask agent to revise… ", button_style(), Hit::AskAgent));
    }
    if gpu && tab.kind == Kind::Html {
        let on = app.doc.notebook_scripts.iter().any(|s| super::same_path(s, &tab.path));
        buttons.push((if on { " Scripts: on " } else { " Scripts: off " }, if on { fg(ATTENTION).bg(PANEL_HI) } else { button_style() }, Hit::Scripts));
    }
    flow.sep("  ", Style::default());
    for (label, style, target) in buttons {
        flow.piece(app, buf, label, style, Some(target));
        flow.sep(" ", Style::default());
    }
    flow.next_row()
}

/// The conflict banner (or a missing file, a failed save).
fn draw_banner(app: &mut App, buf: &mut Buffer, area: Rect, y: i32) -> i32 {
    let Some(tab) = app.notebook.tab() else { return y };
    let (text, buttons): (String, bool) = if tab.conflict.is_some() {
        ("⚠ This note changed on disk while you edited it.".into(), true)
    } else if tab.missing {
        ("⚠ The file is gone: saving writes it again.".into(), false)
    } else if let Some((error, _)) = &tab.save_error {
        (format!("⚠ Not saved: {error}"), false)
    } else {
        return y;
    };
    let style = fg(ATTENTION).bg(ATTENTION_BG);
    let rows = if buttons { 2 } else { 1 };
    fill(buf, area.x as i32 + 1, y, area.width as i32 - 2, rows, Style::default().bg(ATTENTION_BG), area);
    put(buf, area.x as i32 + 2, y, &truncate(&text, area.width.saturating_sub(4) as usize), style, area);
    if buttons {
        let mut x = area.x as i32 + 2;
        for (label, target) in [(" Compare ", Hit::Compare), (" Keep mine ", Hit::KeepMine), (" Take theirs ", Hit::TakeTheirs)] {
            let end = put(buf, x, y + 1, label, bold(Color::Black).bg(ATTENTION), area);
            if end > x {
                hit(app, Rect::new(x as u16, (y + 1) as u16, (end - x) as u16, 1), target);
            }
            x = end + 1;
        }
    }
    y + rows
}

/// The find bar: its field, the count and its buttons; the field's caret.
fn draw_find(app: &mut App, buf: &mut Buffer, area: Rect, y: i32) -> Option<(u16, u16)> {
    let find = app.notebook.find.clone()?;
    let (current, count) = tabs::find_count(app.notebook.tab_mut()?, &find);
    let x = area.x as i32 + 1;
    let label_end = put(buf, x, y, "Find ", fg(GRAY), area);
    let field_width = ((area.width as i32) / 3).clamp(8, 40);
    let caret = crate::ui_modals::field(buf, label_end, y, field_width, &find.input, app.focus == Focus::Canvas, "", area);
    let status = if find.input.text.trim().is_empty() { String::new() } else if count == 0 { " no matches".into() } else { format!(" {current}/{count}") };
    let mut fx = put(buf, label_end + field_width, y, &status, if count == 0 && !status.is_empty() { fg(NEGATIVE) } else { fg(GRAY) }, area) + 1;
    for (label, target) in [(" ↑ ", Hit::FindPrev), (" ↓ ", Hit::FindNext), (" × ", Hit::FindClose)] {
        let end = put(buf, fx, y, label, button_style(), area);
        if end > fx {
            hit(app, Rect::new(fx as u16, y as u16, (end - fx) as u16, 1), target);
        }
        fx = end + 1;
    }
    caret
}

/// The note itself; the editor's caret in Source and Live.
fn draw_content(app: &mut App, buf: &mut Buffer, content: Rect) -> Option<(u16, u16)> {
    let index = app.notebook.active;
    let focused = app.focus == Focus::Canvas && app.notebook.find.is_none();
    let x = content.x as i32;
    let width = content.width as usize;
    let tab = &mut app.notebook.tabs[index];
    if let Some(error) = &tab.error {
        put(buf, x, content.y as i32, &truncate(error, width), fg(NEGATIVE), content);
        return None;
    }
    if !tab.loaded {
        put(buf, x, content.y as i32, "Loading…", fg(MUTED), content);
        return None;
    }
    match (tab.view, tab.kind) {
        (View::Source | View::Live, _) => tab.editor.draw(buf, content, focused),
        (View::Rendered, Kind::Markdown | Kind::Html) => {
            rendered(app, buf, content);
            None
        }
        (View::Rendered, Kind::Image) => {
            let name = tab.name.clone();
            let page = tab.image.clone();
            match page {
                Some(Ok(page)) if pane_allowed(app) => {
                    publish(app, content, format!("image:{}", tab_path(app)), HtmlSource::Document(page));
                }
                Some(Err(error)) => {
                    put(buf, x, content.y as i32, &truncate(&format!("[image: {name}]"), width), fg(GRAY), content);
                    put(buf, x, content.y as i32 + 1, &truncate(&error, width), fg(MUTED), content);
                }
                _ => {
                    put(buf, x, content.y as i32, &truncate(&format!("[image: {name}]"), width), fg(GRAY), content);
                    put(buf, x, content.y as i32 + 2, &truncate("Open externally shows it in its program.", width), fg(MUTED), content);
                }
            }
            None
        }
        _ => {
            let name = tab.name.clone();
            let line = format!("{name} · {} · no preview here", size_label(tab_size(app)));
            put(buf, x, content.y as i32, &truncate(&line, width), fg(GRAY), content);
            put(buf, x, content.y as i32 + 2, &truncate("Open externally shows it in its program.", width), fg(MUTED), content);
            None
        }
    }
}

fn tab_path(app: &App) -> String {
    app.notebook.tab().map(|t| t.path.clone()).unwrap_or_default()
}

/// The size of the shown file as listed in its folder (0: not known).
fn tab_size(app: &App) -> u64 {
    let path = tab_path(app);
    let folder = super::parent(&path).and_then(|dir| app.notebook.folders.get(&super::norm(&dir)));
    folder.and_then(|f| f.entries.iter().find(|e| super::same_path(&e.path, &path))).map(|e| e.size).unwrap_or(0)
}

fn size_label(bytes: u64) -> String {
    match bytes {
        0 => "size unknown".into(),
        b if b < 1024 => format!("{b} B"),
        b if b < 1 << 20 => format!("{} KB", b.div_ceil(1024)),
        b => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
    }
}

/// The GUI's HTML pane may show now: Gpu mode, no dialog or menu open, the view works.
fn pane_allowed(app: &App) -> bool {
    app.render_mode == RenderMode::Gpu && app.modals.is_empty() && !matches!(app.reports.web, crate::reports::WebState::Failed(_))
}

fn publish(app: &mut App, rect: Rect, id: String, source: HtmlSource) {
    app.layout.html_pane = Some(HtmlPane { rect, id, source, origin: crate::view::HtmlOrigin::Note });
    app.notebook.pane = true;
}

/// Rewrites a page's links for the cells view: `#note:`/`#todo:` and
/// relative links travel as [`LINK_HOST`] addresses (`richtext` keeps http(s) links only).
pub fn cell_links(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find("href=\"") {
        let (head, tail) = rest.split_at(at + 6);
        out.push_str(head);
        let end = tail.find('"').unwrap_or(tail.len());
        let href = &tail[..end];
        let lower = href.to_ascii_lowercase();
        if let Some(app_link) = href.strip_prefix("#note:").map(|r| format!("note:{r}")).or_else(|| href.strip_prefix("#todo:").map(|r| format!("todo:{r}"))) {
            out.push_str(&format!("{LINK_HOST}{app_link}"));
        } else if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") || href.starts_with('#') || lower.contains(':') {
            out.push_str(href);
        } else {
            out.push_str(&format!("{LINK_HOST}rel/{}", href.replace(' ', "%20")));
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The note laid out as formatted cells (cached per version and width).
fn cell_lines(tab: &mut Tab, width: usize) -> Arc<Vec<crate::richtext::Line>> {
    if let Some((version, w, lines)) = &tab.lines
        && *version == tab.version
        && *w == width
    {
        return lines.clone();
    }
    let text = tab.text();
    let html = if tab.kind == Kind::Markdown { nb::markdown_html(&text) } else { text };
    let lines = Arc::new(crate::richtext::layout(&crate::richtext::html(&cell_links(&html)), width.max(10)));
    tab.lines = Some((tab.version, width, lines.clone()));
    lines
}

/// The page of a Markdown note for the GUI's pane (cached per version).
fn markdown_page(tab: &mut Tab) -> Arc<String> {
    if let Some((version, page)) = &tab.document
        && *version == tab.version
    {
        return page.clone();
    }
    let page = Arc::new(crate::report_html::document(&nb::markdown_html(&tab.text()), &crate::report_html::NOTEBOOK));
    tab.document = Some((tab.version, page.clone()));
    page
}

/// Rendered: the GUI's HTML pane when it can, else formatted cells.
fn rendered(app: &mut App, buf: &mut Buffer, content: Rect) {
    let index = app.notebook.active;
    let mut top = content.y as i32;
    if pane_allowed(app) {
        let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
        let scripts = app.doc.notebook_scripts.iter().any(|s| super::same_path(s, &app.notebook.tabs[index].path));
        let tab = &mut app.notebook.tabs[index];
        let path = tab.path.clone();
        match (tab.kind, tab.rev.clone()) {
            (Kind::Markdown, _) => {
                let page = markdown_page(tab);
                if page.len() <= crate::reports::HTML_VIEW_MAX {
                    let id = format!("note:{path}#{}", tab.version);
                    return publish(app, content, id, HtmlSource::Document(page));
                }
                put(buf, content.x as i32, top, &truncate("Shown as formatted text: too large for the HTML view.", content.width as usize), fg(MUTED), content);
                top += 2;
            }
            (Kind::Html, Some(rev)) => {
                let root = tree::root_of(&roots, &path).map(|r| r.path.clone()).or_else(|| super::parent(&path)).unwrap_or_default();
                return publish(app, content, format!("note:{path}@{rev}"), HtmlSource::File { path, root, scripts });
            }
            _ => {}
        }
    } else if let crate::reports::WebState::Failed(why) = &app.reports.web
        && app.render_mode == RenderMode::Gpu
    {
        put(buf, content.x as i32, top, &truncate(&format!("Shown as formatted text: the HTML view is unavailable ({why})."), content.width as usize), fg(MUTED), content);
        top += 2;
    }
    let area = Rect::new(content.x, top as u16, content.width, (content.bottom() as i32 - top).max(0) as u16);
    let find_line = app.notebook.find.as_ref().and_then(|f| f.line);
    let tab = &mut app.notebook.tabs[index];
    let lines = cell_lines(tab, area.width as usize);
    let rows = area.height as usize;
    tab.rows = rows;
    tab.total = lines.len();
    tab.scroll = tab.scroll.min(lines.len().saturating_sub(rows));
    let scroll = tab.scroll;
    if lines.is_empty() {
        put(buf, area.x as i32, area.y as i32, "(empty note)", fg(MUTED), area);
    }
    for (row, line) in lines.iter().enumerate().skip(scroll).take(rows) {
        let y = area.y as i32 + (row - scroll) as i32;
        let mut x = area.x as i32;
        for (text, style, link) in &line.spans {
            let style = if find_line == Some(row) { style.bg(ATTENTION_BG) } else { *style };
            let end = put(buf, x, y, text, style, area);
            if let Some(url) = link
                && end > x
            {
                hit(app, Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Link(url.clone()));
            }
            x = end;
        }
    }
}

/// The `[[` completion list under (or over) the caret.
fn draw_completion(app: &mut App, buf: &mut Buffer, area: Rect, (cx, cy): (u16, u16)) {
    let Some(completion) = app.notebook.completion.clone().filter(|c| !c.items.is_empty()) else { return };
    let width = (completion.items.iter().map(|i| text_width(i)).max().unwrap_or(10) as u16 + 4).max(16).min(area.width.saturating_sub(2));
    if width < 8 {
        return;
    }
    let height = completion.items.len() as u16;
    let x = cx.min(area.right().saturating_sub(width + 1)).max(area.x + 1);
    let y = if cy + 1 + height <= area.bottom() { cy + 1 } else { cy.saturating_sub(height) };
    for (i, item) in completion.items.iter().enumerate() {
        let row = y + i as u16;
        let on = i == completion.sel;
        let style = if on { bold(Color::Black).bg(ACCENT) } else { fg(TEXT).bg(MODAL) };
        fill(buf, x as i32, row as i32, width as i32, 1, style, area);
        put(buf, x as i32 + 1, row as i32, &format!("✎ {}", truncate(item, width as usize - 4)), style, area);
        hit(app, Rect::new(x, row, width, 1), Hit::Completion(i));
    }
}
