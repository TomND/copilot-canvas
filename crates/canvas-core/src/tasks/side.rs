//! The Tasks sidebar (slot 1 of the activity bar): NEEDS YOU (proposals,
//! todos in review, stale claims), READY (of the boards linked to the group
//! viewed last, else all), CLAIMED BY SESSIONS and the BOARDS tree with
//! `+ New board`; the `/` filter and the type filter apply to every list.

use canvas_model::todos::{Need, Status, Todo, TodoBook};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::book::{self, book, type_color, type_glyph};
use super::canvas::filter;
use super::{Action, Hit};
use crate::app::{App, Focus};
use crate::dialogs::Hit as AppHit;
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseEventKind};
use crate::paint::{ACCENT, ATTENTION, BLUE, GRAY, MUTED, NEGATIVE, PANEL, PANEL_HI, SELECT, TEXT, bold, fg, fill, put, text_width, truncate};

/// A row of the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Row {
    /// A section's title and count (gold: something waits).
    Head { text: String, count: usize, gold: bool },
    /// A todo: a mark before it (the need, else its type) and a dim note after.
    Todo { id: String, mark: &'static str, color: Color, depth: u16, note: String },
    /// A session with claims.
    Session { id: String, name: String, working: bool },
    Board { id: String, depth: u16 },
    NewBoard,
    /// A dim line ("Nothing waits for you.").
    Note(String),
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Todo { .. } | Row::Board { .. } | Row::NewBoard | Row::Session { .. })
    }
}

fn todo_row(todo: &Todo, mark: Option<(&'static str, Color)>, depth: u16, note: String) -> Row {
    let (mark, color) = mark.unwrap_or((type_glyph(todo.kind), type_color(todo.kind)));
    Row::Todo { id: todo.id.clone(), mark, color, depth, note }
}

/// The sidebar's rows for the book as it is.
pub(crate) fn rows(app: &App, book: &TodoBook) -> Vec<Row> {
    let filter = filter(app);
    let mut rows = Vec::new();
    // NEEDS YOU.
    let needs: Vec<_> = book.needs_you(|s| book::session_gone(app, s)).into_iter().filter(|n| filter.matches(n.todo)).collect();
    rows.push(Row::Head { text: "NEEDS YOU".into(), count: needs.len(), gold: !needs.is_empty() });
    for need in &needs {
        let (mark, color, note) = match need.need {
            Need::Proposed => ("◇", ATTENTION, "proposed".to_owned()),
            Need::InReview => ("◎", BLUE, "in review".to_owned()),
            Need::StaleClaim => ("⚠", NEGATIVE, format!("{} gone", book::session_name(app, need.todo))),
        };
        rows.push(todo_row(need.todo, Some((mark, color)), 0, note));
    }
    if needs.is_empty() {
        rows.push(Row::Note("Nothing waits for you.".into()));
    }
    // READY: the group viewed last, else every board.
    let group = app.tasks.last_group.as_deref().filter(|g| !book.boards_of_group(g).is_empty());
    let ready: Vec<&Todo> = book.ready_for_group(group).into_iter().filter(|t| filter.matches(t)).collect();
    let title = match group.and_then(|g| app.doc.group(g)) {
        Some(g) => format!("READY · {}", g.name),
        None => "READY".to_owned(),
    };
    rows.push(Row::Head { text: title, count: ready.len(), gold: false });
    for todo in ready.iter().take(30) {
        rows.push(todo_row(todo, None, 0, todo.priority.marks().to_owned()));
    }
    if ready.len() > 30 {
        rows.push(Row::Note(format!("… {} more", ready.len() - 30)));
    }
    if ready.is_empty() {
        rows.push(Row::Note("Nothing ready (status Todo, not blocked or claimed).".into()));
    }
    // CLAIMED BY SESSIONS, by session.
    let mut sessions: Vec<(String, Vec<&Todo>)> = Vec::new();
    for todo in book.todos.iter().filter(|t| !t.status.closed() && filter.matches(t)) {
        let Some(session) = todo.claim_session() else { continue };
        match sessions.iter_mut().find(|(s, _)| s == session) {
            Some((_, todos)) => todos.push(todo),
            None => sessions.push((session.to_owned(), vec![todo])),
        }
    }
    rows.push(Row::Head { text: "CLAIMED BY SESSIONS".into(), count: sessions.iter().map(|(_, t)| t.len()).sum(), gold: false });
    for (session, todos) in &sessions {
        let name = todos.first().map(|t| book::session_name(app, t)).unwrap_or_default();
        rows.push(Row::Session { id: session.clone(), name, working: book::session_working(app, session) });
        for todo in todos {
            let state = book.root(&todo.board).map(|b| b.status_name(todo.status)).unwrap_or_default();
            rows.push(todo_row(todo, None, 1, state));
        }
    }
    if sessions.is_empty() {
        rows.push(Row::Note("No session claimed a todo.".into()));
    }
    // BOARDS.
    rows.push(Row::Head { text: "BOARDS".into(), count: book.boards.iter().filter(|b| b.parent.is_none()).count(), gold: false });
    for board in book.boards.iter().filter(|b| b.parent.is_none()) {
        rows.push(Row::Board { id: board.id.clone(), depth: 0 });
        for sub in book.sub_boards(&board.id) {
            rows.push(Row::Board { id: sub.id.clone(), depth: 1 });
        }
    }
    rows.push(Row::NewBoard);
    // A blank line before every section but the first.
    let mut out = Vec::with_capacity(rows.len() + 3);
    for row in rows {
        if matches!(row, Row::Head { .. }) && !out.is_empty() {
            out.push(Row::Note(String::new()));
        }
        out.push(row);
    }
    out
}

/// The selectable rows' indexes.
fn selectable(rows: &[Row]) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, r)| r.selectable()).map(|(i, _)| i).collect()
}

pub fn draw(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default().bg(PANEL), area);
    let focused = app.focus == Focus::Sidebar;
    let (icon, title) = crate::spaces::Space::Tasks.side();
    let (x, w) = (area.x as i32 + 1, area.width.saturating_sub(2) as i32);
    put(buf, x, area.y as i32, &format!("{icon} {title}"), bold(if focused { ACCENT } else { GRAY }).bg(PANEL), area);
    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    // The filters: `/` text, then the type (only its glyph in a narrow
    // sidebar: the chip and its click target stay inside it).
    let full = match app.tasks.kind {
        Some(kind) => format!(" {} {} ▾ ", type_glyph(kind), kind.word()),
        None => " all types ▾ ".to_owned(),
    };
    let kind = if text_width(&full) as i32 + 9 <= w {
        full
    } else {
        match app.tasks.kind {
            Some(kind) => format!(" {} ▾ ", type_glyph(kind)),
            None => " ▾ ".to_owned(),
        }
    };
    let kind_w = text_width(&kind) as i32;
    let field_w = (w - kind_w - 1).max(1);
    let fy = area.y as i32 + 1;
    let mut cursor = None;
    if app.tasks.filter_active {
        cursor = crate::ui_modals::field(buf, x, fy, field_w, &app.tasks.filter, true, "", area);
    } else {
        let (text, style) = if app.tasks.filter.text.is_empty() { ("/ filter".to_owned(), fg(MUTED).bg(PANEL_HI)) } else { (format!("/ {}", app.tasks.filter.text), fg(TEXT).bg(PANEL_HI)) };
        fill(buf, x, fy, field_w, 1, Style::default().bg(PANEL_HI), area);
        put(buf, x, fy, &truncate(&text, field_w as usize), style, area);
    }
    hits.push((Rect::new(x as u16, fy as u16, field_w as u16, 1), Hit::Filter));
    let kx = x + field_w + 1;
    put(buf, kx, fy, &kind, if app.tasks.kind.is_some() { bold(ACCENT).bg(PANEL_HI) } else { fg(GRAY).bg(PANEL_HI) }, area);
    let kind_w = kind_w.min(area.right() as i32 - kx).max(0);
    hits.push((Rect::new(kx as u16, fy as u16, kind_w as u16, 1), Hit::Do(Action::PickTypeFilter)));
    // The lists.
    let rows = match book(app) {
        Some(book) => rows(app, book),
        None => vec![Row::Note("The todo boards are not loaded yet.".into())],
    };
    let top = area.y as i32 + 3;
    let height = (area.bottom() as i32 - top).max(0) as usize;
    let picks = selectable(&rows);
    if !picks.is_empty() {
        app.tasks.side_sel = app.tasks.side_sel.min(picks.len() - 1);
    }
    let lit = picks.get(app.tasks.side_sel).copied().filter(|_| focused && !app.tasks.filter_active);
    let mut scroll = app.tasks.side_scroll.min(rows.len().saturating_sub(height));
    if let Some(lit) = lit {
        if lit < scroll {
            scroll = lit;
        } else if height > 0 && lit >= scroll + height {
            scroll = lit + 1 - height;
        }
    }
    app.tasks.side_scroll = scroll;
    let b = book(app);
    for (i, row) in rows.iter().enumerate().skip(scroll).take(height) {
        let y = top + (i - scroll) as i32;
        let bg = if Some(i) == lit { PANEL_HI } else { PANEL };
        if Some(i) == lit {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(bg), area);
        }
        let text_style = if Some(i) == lit { fg(SELECT).bg(bg) } else { fg(TEXT).bg(bg) };
        let rect = Rect::new(area.x, y as u16, area.width, 1);
        match row {
            Row::Head { text, count, gold } => {
                let style = if *gold { bold(ATTENTION).bg(bg) } else { bold(GRAY).bg(bg) };
                put(buf, x, y, &truncate(&format!("{text} {count}"), w as usize), style, area);
            }
            Row::Todo { id, mark, color, depth, note } => {
                let todo = b.and_then(|b| b.todo(id));
                let mut cx = x + 1 + 2 * *depth as i32;
                cx = put(buf, cx, y, mark, fg(*color).bg(bg), area) + 1;
                cx = put(buf, cx, y, id, bold(TEXT).bg(bg), area) + 1;
                let note_w = if note.is_empty() || text_width(note) > 14 { 0 } else { text_width(note) as i32 + 1 };
                let room = (x + w - cx - note_w).max(0) as usize;
                if let Some(todo) = todo {
                    put(buf, cx, y, &truncate(&todo.title, room), text_style, area);
                }
                if note_w > 0 {
                    put(buf, x + w - note_w + 1, y, note, fg(MUTED).bg(bg), area);
                }
                hits.push((rect, Hit::Todo(id.clone())));
            }
            Row::Session { id, name, working } => {
                let (mark, style) = if *working { ("⟳", fg(TEXT).bg(bg)) } else { ("◫", fg(GRAY).bg(bg)) };
                let cx = put(buf, x + 1, y, mark, style, area) + 1;
                put(buf, cx, y, &truncate(name, (x + w - cx).max(0) as usize), text_style, area);
                hits.push((rect, Hit::Do(Action::ViewSession(id.clone()))));
            }
            Row::Board { id, depth } => {
                if let Some((board, open)) = b.and_then(|b| b.board(id).map(|x| (x, b.todos.iter().filter(|t| t.board == *id && t.status.open()).count()))) {
                    let color = crate::paint::GROUP_COLORS[board.color as usize % crate::paint::GROUP_COLORS.len()];
                    let label = if *depth == 0 { format!("▦ {} · {}", board.name, board.prefix) } else { format!("  └ {}", board.name) };
                    let count = format!("{open}");
                    let room = (w - 2 - text_width(&count) as i32).max(0) as usize;
                    put(buf, x + 1, y, &truncate(&label, room), if *depth == 0 { bold(color).bg(bg) } else { fg(color).bg(bg) }, area);
                    put(buf, x + w - text_width(&count) as i32, y, &count, fg(MUTED).bg(bg), area);
                }
                hits.push((rect, Hit::Board(id.clone())));
            }
            Row::NewBoard => {
                put(buf, x + 1, y, "+ New board", bold(ACCENT).bg(bg), area);
                hits.push((rect, Hit::Do(Action::NewBoard)));
            }
            Row::Note(text) => {
                put(buf, x + 1, y, &truncate(text, (w - 1).max(0) as usize), fg(MUTED).bg(bg), area);
            }
        }
    }
    app.layout.hits.extend(hits.into_iter().map(|(r, h)| (r, AppHit::Tasks(h))));
    app.tasks.side = rows;
    cursor.filter(|_| focused)
}

/// A click on a row (screen row `y`): the keyboard goes on from it.
pub fn follow(app: &mut App, y: u16) {
    let top = app.layout.sidebar.y + 3;
    let Some(index) = y.checked_sub(top).map(|dy| app.tasks.side_scroll + dy as usize) else { return };
    if let Some(at) = selectable(&app.tasks.side).iter().position(|i| *i == index) {
        app.tasks.side_sel = at;
    }
}

/// Opens what a row stands for.
fn activate(app: &mut App, row: &Row) {
    match row {
        Row::Todo { id, .. } => super::flows::show_todo(app, id, true),
        Row::Board { id, .. } => super::flows::show_board(app, id),
        Row::NewBoard => super::flows::run(app, Action::NewBoard),
        Row::Session { id, .. } => super::flows::run(app, Action::ViewSession(id.clone())),
        Row::Head { .. } | Row::Note(_) => {}
    }
}

/// A key while the sidebar has the keyboard.
pub fn key(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT) {
        return false;
    }
    let picks = selectable(&app.tasks.side);
    let last = picks.len().saturating_sub(1);
    let sel = app.tasks.side_sel.min(last);
    let row = picks.get(sel).and_then(|i| app.tasks.side.get(*i)).cloned();
    let todo = match &row {
        Some(Row::Todo { id, .. }) => Some(id.clone()),
        _ => None,
    };
    match key.code {
        KeyCode::Up => app.tasks.side_sel = sel.saturating_sub(1),
        KeyCode::Down => app.tasks.side_sel = (sel + 1).min(last),
        KeyCode::PageUp => app.tasks.side_sel = sel.saturating_sub(10),
        KeyCode::PageDown => app.tasks.side_sel = (sel + 10).min(last),
        KeyCode::Home => app.tasks.side_sel = 0,
        KeyCode::End => app.tasks.side_sel = last,
        KeyCode::Enter | KeyCode::Right => match row {
            Some(row) => activate(app, &row),
            None => return false,
        },
        KeyCode::Char('/') => app.tasks.filter_active = true,
        KeyCode::Char('t' | 'T') => {
            let all = canvas_model::todos::TodoType::ALL;
            app.tasks.kind = match app.tasks.kind.and_then(|k| all.iter().position(|x| *x == k)) {
                None => Some(all[0]),
                Some(i) if i + 1 < all.len() => Some(all[i + 1]),
                Some(_) => None,
            };
        }
        KeyCode::Char('a' | 'A') | KeyCode::Char('d' | 'D') | KeyCode::Delete => {
            let Some(id) = todo else { return false };
            let proposal = book(app).and_then(|b| b.todo(&id)).is_some_and(|t| t.status == Status::Proposed);
            match key.code {
                KeyCode::Delete => super::flows::run(app, Action::ConfirmCancel(id)),
                KeyCode::Char('a' | 'A') if proposal => super::flows::run(app, Action::Op(Box::new(canvas_model::todos::TodoOp::Accept { id, status: None }))),
                _ if proposal => super::flows::run(app, Action::Op(Box::new(canvas_model::todos::TodoOp::Decline { id, reason: None }))),
                _ => app.notify(format!("{id} is not a proposal."), false),
            }
        }
        _ => return false,
    }
    true
}

/// Keys while the `/` filter is being typed.
pub fn filter_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            app.tasks.filter_active = false;
            app.tasks.filter = crate::dialogs::TextInput::default();
        }
        KeyCode::Enter | KeyCode::Down | KeyCode::Tab => {
            app.tasks.filter_active = false;
            app.set_focus(Focus::Sidebar);
        }
        _ => {
            if !app.tasks.filter.handle(&key) {
                return false;
            }
            app.tasks.side_sel = 0;
            app.tasks.side_scroll = 0;
        }
    }
    true
}

pub fn wheel(app: &mut App, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollUp => app.tasks.side_scroll = app.tasks.side_scroll.saturating_sub(3),
        MouseEventKind::ScrollDown => app.tasks.side_scroll = app.tasks.side_scroll.saturating_add(3),
        _ => {}
    }
    app.needs_draw = true;
}
