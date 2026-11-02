//! The todo canvas (the main area): drawing the frames and cards of the
//! last layout, the mouse (select, drag cards and boards, resize, pan,
//! wheel and zoom) and the keys (arrows, Enter, Space, N, A, D, Del, + - F).

use std::collections::BTreeMap;

use canvas_model::todos::{Priority, Status, Todo, TodoBook, TodoOp, TodoType};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::book::{self, book, type_color, type_glyph};
use super::layout::{self, Cells, Chip, Dir, Filter, Frame, Input, Target, View};
use super::{Action, Hit};
use crate::app::App;
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::paint::{ACCENT, ATTENTION, FAINT, GRAY, GROUP_COLORS, LIGHT, MUTED, NEGATIVE, PANEL, PANEL_HI, PURPLE, ROUND, SELECT, TEXT, USER_BG, bold, fg, fill, outline, put, text_width, truncate};

/// A press in the todo canvas, followed while its button is down.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Press {
    /// A card: a click selects it; a drag moves it to another column, lane,
    /// board or place.
    Card { id: String, from: (u16, u16), at: (u16, u16), moved: bool },
    /// A board's title: moves the board.
    Board { id: String, from: (u16, u16), at: (u16, u16) },
    /// A board's `◢` corner: resizes it.
    Resize { id: String, from: (u16, u16), at: (u16, u16) },
    /// The background: pans.
    Pan { from: (u16, u16), pan: (i32, i32) },
}

/// The filter the canvas and the sidebar apply.
pub(crate) fn filter(app: &App) -> Filter<'_> {
    Filter { text: app.tasks.filter.text.as_str(), kind: app.tasks.kind }
}

/// The board being moved or resized: (id, dx, dy, dw, dh) in world cells.
fn moving(app: &App) -> Option<(String, i32, i32, i32, i32)> {
    let d = layout::DIVS[(app.tasks.zoom as usize).min(2)];
    let delta = |from: (u16, u16), at: (u16, u16)| ((at.0 as i32 - from.0 as i32) * d, (at.1 as i32 - from.1 as i32) * d);
    match &app.tasks.press {
        Some(Press::Board { id, from, at }) => {
            let (dx, dy) = delta(*from, *at);
            Some((id.clone(), dx, dy, 0, 0))
        }
        Some(Press::Resize { id, from, at }) => {
            let (dw, dh) = delta(*from, *at);
            Some((id.clone(), 0, 0, dw, dh))
        }
        _ => None,
    }
}

/// The view of the canvas in `area` as it is now.
pub(crate) fn view(app: &App, area: Rect) -> View {
    View { area, pan: app.tasks.pan, zoom: app.tasks.zoom.min(2) }
}

/// What the todo canvas's layout is made of besides the pan: it is laid
/// out again only when one of these changes (a pan by whole cells moves it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SceneKey {
    /// `State::book_gen`, then the book's revision and sizes (a book set
    /// some other way).
    book: (u64, u64, usize, usize),
    area: Rect,
    zoom: u8,
    filter: String,
    kind: Option<TodoType>,
    groups: BTreeMap<String, String>,
    moving: Option<(String, i32, i32, i32, i32)>,
}

/// Brings `app.tasks.scene` up to date for the canvas in `area`: the last
/// layout while nothing it is made of changed (moved when only the pan
/// did), else a new one. Laying out walks every todo (and the relations of
/// the blocked ones), so it is not done for every frame of a big book.
pub(crate) fn refresh_scene(app: &mut App, area: Rect) {
    let view = view(app, area);
    let key = match book(app) {
        Some(book) => SceneKey { book: (app.tasks.book_gen, book.rev, book.todos.len(), book.boards.len()), area, zoom: view.zoom, filter: app.tasks.filter.text.clone(), kind: app.tasks.kind, groups: book::group_names(app), moving: moving(app) },
        None => {
            app.tasks.scene = layout::Scene { view, frames: Vec::new() };
            app.tasks.scene_key = None;
            return;
        }
    };
    if app.tasks.scene_key.as_ref() == Some(&key) && app.tasks.scene.repan(view.pan) {
        return;
    }
    let Some(book) = book(app) else { return };
    let input = Input { view, filter: Filter { text: &key.filter, kind: key.kind }, groups: &key.groups, moving: key.moving.as_ref().map(|(id, dx, dy, dw, dh)| (id.as_str(), *dx, *dy, *dw, *dh)) };
    let scene = layout::layout(book, &input);
    app.tasks.scene = scene;
    app.tasks.scene_key = Some(key);
    app.tasks.layouts += 1;
}

/// A card or board to bring into view when the canvas is drawn next (its
/// area is known then).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reveal {
    Todo(String),
    Board(String),
}

/// Pans so that a card (else its board's frame), or a board's frame, shows.
fn reveal_now(app: &mut App, area: Rect, what: &Reveal) {
    refresh_scene(app, area);
    let pan = {
        let Some(book) = book(app) else { return };
        let scene = &app.tasks.scene;
        let rect = match what {
            Reveal::Todo(id) => scene.card(id).map(|c| c.rect).or_else(|| book.todo(id).and_then(|t| book.root(&t.board)).and_then(|b| scene.frame(&b.id)).map(|f| f.rect)),
            Reveal::Board(id) => scene.frame(id).map(|f| f.rect),
        };
        let Some(rect) = rect else { return };
        layout::reveal(&scene.view, rect)
    };
    app.tasks.pan = pan;
}

pub fn draw(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    if let Some(what) = app.tasks.reveal.take().filter(|_| area.width > 0 && area.height > 0) {
        reveal_now(app, area, &what);
    }
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, Style::default(), area);
    refresh_scene(app, area);
    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    let shown: &App = app;
    match book(shown) {
        None => {
            let text = if shown.connected { "Waiting for the todo boards from the service…" } else { "The todo boards come from the service (not connected)." };
            put(buf, area.x as i32 + 2, area.y as i32 + 1, &truncate(text, area.width.saturating_sub(4) as usize), fg(MUTED), area);
        }
        Some(book) if book.boards.is_empty() => {
            put(buf, area.x as i32 + 2, area.y as i32 + 1, &truncate("No todo boards yet.", area.width.saturating_sub(4) as usize), fg(TEXT), area);
            let end = put(buf, area.x as i32 + 2, area.y as i32 + 3, " + New board ", bold(ACCENT).bg(PANEL_HI), area);
            hits.push((Rect::new(area.x + 2, area.y + 3, (end - area.x as i32 - 2).max(0) as u16, 1), Hit::Do(Action::NewBoard)));
        }
        Some(book) => {
            let scene = &shown.tasks.scene;
            for frame in &scene.frames {
                draw_frame(shown, book, buf, area, frame, scene);
            }
            draw_drag(shown, book, buf, area);
        }
    }
    zoom_controls(app, buf, area, &mut hits);
    app.layout.hits.extend(hits.into_iter().map(|(r, h)| (r, crate::dialogs::Hit::Tasks(h))));
    None
}

/// `−  100%  +  Fit` at the bottom-right corner.
fn zoom_controls(app: &App, buf: &mut Buffer, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
    if area.height < 3 || area.width < 30 {
        return;
    }
    let percent = format!(" {}% ", [100, 50, 25][(app.tasks.zoom as usize).min(2)]);
    let parts = [(" − ", Some(Action::Zoom(-1))), (percent.as_str(), None), (" + ", Some(Action::Zoom(1))), (" Fit ", Some(Action::Fit))];
    let width: i32 = parts.iter().map(|(t, _)| text_width(t) as i32 + 1).sum();
    let mut x = area.right() as i32 - width - 1;
    let y = area.bottom() as i32 - 1;
    for (text, action) in parts {
        let style = if action.is_some() { fg(TEXT).bg(PANEL_HI) } else { fg(MUTED).bg(PANEL) };
        let end = put(buf, x, y, text, style, area);
        if let Some(action) = action {
            hits.push((Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Do(action)));
        }
        x = end + 1;
    }
}

fn board_color(book: &TodoBook, id: &str) -> ratatui::style::Color {
    GROUP_COLORS[book.board(id).map_or(0, |b| b.color as usize) % GROUP_COLORS.len()]
}

fn draw_frame(app: &App, book: &TodoBook, buf: &mut Buffer, area: Rect, frame: &Frame, scene: &layout::Scene) {
    let r = frame.rect;
    if r.clipped(area).is_none() {
        return;
    }
    let color = board_color(book, &frame.board);
    let selected = app.tasks.board.as_deref() == Some(frame.board.as_str());
    fill(buf, r.x, r.y, r.w, r.h, Style::default().bg(PANEL), area);
    outline(buf, r.x, r.y, r.w, r.h, if selected { &ROUND } else { &LIGHT }, if selected { bold(color).bg(PANEL) } else { fg(color).bg(PANEL) }, area);
    put(buf, frame.label.0, r.y, &frame.label.1, bold(color).bg(PANEL), area);
    for (cells, text, chip) in &frame.chips {
        let style = match chip {
            Chip::Group(_) | Chip::Notes(_) => fg(TEXT).bg(PANEL_HI),
            Chip::NoGroup | Chip::NoNotes => fg(MUTED).bg(PANEL_HI),
        };
        put(buf, cells.x, cells.y, text, style, area);
    }
    if let Some((x, text)) = &frame.canceled {
        put(buf, *x, r.y, text, fg(MUTED).bg(PANEL), area);
    }
    put(buf, frame.corner.0, frame.corner.1, "◢", fg(color).bg(PANEL), area);
    if let Some(stats) = &frame.stats {
        return draw_summary(buf, area, frame, stats);
    }
    let target = drop_target(app, scene);
    // Column headers and separators.
    for (i, column) in frame.columns.iter().enumerate() {
        if i > 0 {
            for y in r.y + 1..r.bottom() - 1 {
                put(buf, column.x, y, "│", fg(FAINT).bg(PANEL), area);
            }
        }
        let count = match column.wip {
            Some(wip) => format!("{}/{wip}", column.count),
            None => column.count.to_string(),
        };
        let lit = target.as_ref().is_some_and(|d| d.status == Some(column.status) && frame.lanes.iter().any(|l| l.board == d.board));
        let mut style = if column.over { bold(ATTENTION).bg(PANEL) } else { bold(GRAY).bg(PANEL) };
        if lit {
            style = bold(PANEL).bg(ACCENT);
        }
        let head = truncate(&format!("{} {count}", column.name), (column.w - 2).max(0) as usize);
        put(buf, column.x + 1, r.y + 1, &head, style, area);
    }
    // Swimlane headers.
    for lane in frame.lanes.iter().filter(|l| l.head && l.h > 0) {
        let line = "─".repeat((r.w - 2).max(0) as usize);
        put(buf, r.x + 1, lane.y, &line, fg(FAINT).bg(PANEL), area);
        put(buf, r.x + 2, lane.y, &truncate(&format!(" {} ", lane.name), (r.w - 4).max(0) as usize), bold(MUTED).bg(PANEL), area);
    }
    for card in frame.cards.iter().filter(|c| c.rect.clipped(area).is_some()) {
        // Its place in the book the layout was made from (else, a book set some other way, a search).
        if let Some(todo) = book.todos.get(card.index).filter(|t| t.id == card.id).or_else(|| book.todo(&card.id)) {
            let dragging = matches!(&app.tasks.press, Some(Press::Card { id, moved: true, .. }) if *id == card.id);
            draw_card(app, buf, area, card, todo, dragging);
        }
    }
    for (x, y, count) in &frame.more {
        put(buf, *x, *y, &format!("+{count} more"), fg(MUTED).bg(PANEL), area);
    }
    for (x, y, count) in &frame.done {
        put(buf, *x, *y, &format!("✓ {count}"), fg(MUTED).bg(PANEL), area);
    }
    // Where a dragged card would go: a bar above the card it goes before.
    if let Some(drop) = target
        && let Some(column) = frame.columns.iter().find(|c| Some(c.status) == drop.status)
        && let Some(lane) = frame.lanes.iter().find(|l| l.board == drop.board)
    {
        let y = match (&drop.before, &drop.after) {
            (Some(before), _) => frame.cards.iter().find(|c| c.id == *before).map(|c| c.rect.y - 1),
            (None, Some(after)) => frame.cards.iter().find(|c| c.id == *after).map(|c| c.rect.bottom()),
            (None, None) => Some(lane.y + i32::from(lane.head)),
        };
        if let Some(y) = y.filter(|y| *y >= lane.y && *y < lane.y + lane.h) {
            put(buf, column.x + 1, y, &"▔".repeat((column.w - 2).max(1) as usize), fg(ACCENT).bg(PANEL), area);
        }
    }
}

/// Where the card being dragged would land now.
fn drop_target(app: &App, scene: &layout::Scene) -> Option<layout::Drop> {
    match &app.tasks.press {
        Some(Press::Card { id, at, moved: true, .. }) => scene.drop_at(at.0 as i32, at.1 as i32, id),
        _ => None,
    }
}

/// The card's badges: checklist, blocked, blocks (both from the layout),
/// AI-created, the claiming session at work, pull requests.
fn badges(app: &App, card: &layout::Card, todo: &Todo) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let (done, all) = todo.checked();
    if all > 0 {
        out.push((format!("☑ {done}/{all}"), fg(if done == all { ACCENT } else { GRAY })));
    }
    if card.blocked {
        out.push(("⚑ blocked".into(), fg(NEGATIVE)));
    }
    if card.blocks > 0 {
        out.push((format!("blocks {}", card.blocks), fg(ATTENTION)));
    }
    if todo.ai_created() {
        out.push(("◇".into(), fg(PURPLE)));
    }
    if let Some(session) = todo.claim_session().filter(|s| book::session_working(app, s)) {
        let name = app.doc.node(session).map(|n| n.name.clone()).unwrap_or_default();
        out.push((format!("⟳ {}", truncate(&name, 14)), fg(TEXT)));
    }
    for pr in todo.prs() {
        let state = app.prs_view().and_then(|v| v.prs.iter().find(|p| p.url == pr.target)).map(|p| p.state.word());
        // `owner/repo#42` → `#42`.
        let title = pr.title.rsplit_once('#').map(|(_, n)| format!("#{n}")).unwrap_or_else(|| "PR".to_owned());
        out.push((match state {
            Some(state) => format!("↗ {title} {state}"),
            None => format!("↗ {title}"),
        }, fg(ACCENT)));
    }
    out
}

fn draw_card(app: &App, buf: &mut Buffer, area: Rect, card: &layout::Card, todo: &Todo, dragging: bool) {
    let r = card.rect;
    let selected = app.tasks.selected.as_deref() == Some(todo.id.as_str());
    let bg = if selected { USER_BG } else { PANEL_HI };
    let text = if dragging { MUTED } else if selected { SELECT } else { TEXT };
    let color = type_color(todo.kind);
    fill(buf, r.x, r.y, r.w, r.h, Style::default().bg(bg), area);
    for y in r.y..r.bottom() {
        put(buf, r.x, y, "▌", fg(color).bg(bg), area);
    }
    let width = (r.w - 1).max(0) as usize;
    let mut x = put(buf, r.x + 1, r.y, type_glyph(todo.kind), fg(color).bg(bg), area);
    x = put(buf, x, r.y, &format!(" {}", todo.id), bold(text).bg(bg), area);
    if todo.blocker {
        x = put(buf, x, r.y, " ‼", bold(NEGATIVE).bg(bg), area);
    }
    let marks = todo.priority.marks();
    if !marks.is_empty() {
        let style = if matches!(todo.priority, Priority::Urgent | Priority::High) { bold(ATTENTION).bg(bg) } else { fg(GRAY).bg(bg) };
        let mx = r.right() - 1 - marks.len() as i32;
        if mx > x {
            put(buf, mx, r.y, marks, style, area);
        }
    }
    if r.h < layout::CARD_H {
        // 50 %: the id, then the title.
        put(buf, r.x + 1, r.y + 1, &truncate(&todo.title, width), fg(text).bg(bg), area);
        return;
    }
    for (i, line) in wrap(&todo.title, width.saturating_sub(1), 2).iter().enumerate() {
        put(buf, r.x + 1, r.y + 1 + i as i32, line, fg(text).bg(bg), area);
    }
    let mut bx = r.x + 1;
    for (badge, style) in badges(app, card, todo) {
        let room = (r.right() - bx).max(0) as usize;
        if room < 2 {
            break;
        }
        bx = put(buf, bx, r.y + r.h - 1, &truncate(&badge, room), style.bg(bg), area) + 1;
    }
}

/// Words wrapped to `width`, at most `lines` lines (the last ends in `…` when cut).
pub(crate) fn wrap(text: &str, width: usize, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let candidate = if line.is_empty() { (*word).to_owned() } else { format!("{line} {word}") };
        if text_width(&candidate) <= width || line.is_empty() {
            line = candidate;
            continue;
        }
        out.push(std::mem::take(&mut line));
        if out.len() == lines {
            let last = out.pop().unwrap_or_default();
            out.push(truncate(&format!("{last} {}", words[i..].join(" ")), width));
            return out;
        }
        line = (*word).to_owned();
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter().map(|l| truncate(&l, width)).collect()
}

/// 25 %: a board's counts per column, progress and blocked todos.
fn draw_summary(buf: &mut Buffer, area: Rect, frame: &Frame, stats: &layout::Stats) {
    let r = frame.rect;
    let clip = Cells::new(r.x + 1, r.y + 1, r.w - 2, r.h - 2).clipped(area);
    let Some(clip) = clip else { return };
    let width = (r.w - 2).max(0) as usize;
    // (text, style, cells after it).
    let mut lines: Vec<Vec<(String, Style, i32)>> = vec![Vec::new()];
    let mut used = 0usize;
    for &(status, count, wip, over) in &stats.columns {
        let count_text = match wip {
            Some(wip) => format!("{count}/{wip}"),
            None => count.to_string(),
        };
        let piece = format!("{} {count_text}", book::status_short(status));
        let w = text_width(&piece) + 2;
        if used > 0 && used + w > width {
            lines.push(Vec::new());
            used = 0;
        }
        used += w;
        let style = if over { bold(ATTENTION).bg(PANEL) } else if count > 0 { fg(TEXT).bg(PANEL) } else { fg(MUTED).bg(PANEL) };
        if let Some(line) = lines.last_mut() {
            line.push((piece, style, 2));
        }
    }
    let (done, all) = stats.progress;
    let bar_w = width.saturating_sub(text_width(&format!(" {done}/{all}")) + 1).clamp(1, 20);
    let filled = (done * bar_w).checked_div(all).unwrap_or(0).min(bar_w);
    lines.push(vec![("█".repeat(filled), fg(ACCENT).bg(PANEL), 0), ("░".repeat(bar_w - filled), fg(FAINT).bg(PANEL), 0), (format!(" {done}/{all}"), fg(GRAY).bg(PANEL), 0)]);
    let mut last = Vec::new();
    if stats.blocked > 0 {
        last.push((format!("⚑ {} blocked", stats.blocked), fg(NEGATIVE).bg(PANEL), 2));
    }
    if stats.canceled > 0 {
        last.push((format!("{} canceled", stats.canceled), fg(MUTED).bg(PANEL), 2));
    }
    if !last.is_empty() {
        lines.push(last);
    }
    for (i, line) in lines.iter().enumerate() {
        let mut x = r.x + 2;
        for (text, style, gap) in line {
            x = put(buf, x, r.y + 1 + i as i32, text, *style, clip) + gap;
        }
    }
}

/// The card under the pointer while it is dragged.
fn draw_drag(app: &App, book: &TodoBook, buf: &mut Buffer, area: Rect) {
    let Some(Press::Card { id, at, moved: true, .. }) = &app.tasks.press else { return };
    let Some(todo) = book.todo(id) else { return };
    let text = truncate(&format!(" {} {} {} ", type_glyph(todo.kind), todo.id, todo.title), 28);
    put(buf, at.0 as i32 + 1, at.1 as i32, &text, bold(PANEL).bg(ACCENT), area);
}

fn div(app: &App) -> i32 {
    layout::DIVS[(app.tasks.zoom as usize).min(2)]
}

/// Every mouse event of the main area (and the drag and release of a press there).
pub fn mouse(app: &mut App, mouse: MouseEvent) {
    let at = (mouse.column, mouse.row);
    match mouse.kind {
        // Hovering changes nothing.
        MouseEventKind::Moved if app.tasks.press.is_none() => return,
        MouseEventKind::Down(button) => down(app, button, at),
        MouseEventKind::Drag(_) | MouseEventKind::Moved => drag(app, at),
        MouseEventKind::Up(_) => up(app, at),
        kind => wheel(app, kind, mouse.modifiers),
    }
    app.needs_draw = true;
}

fn down(app: &mut App, button: MouseButton, at: (u16, u16)) {
    let target = app.tasks.scene.hit(at.0 as i32, at.1 as i32);
    let pan = Press::Pan { from: at, pan: app.tasks.pan };
    match (button, target) {
        (MouseButton::Left, Target::Card(id)) => {
            super::flows::show_todo(app, &id, false);
            app.tasks.press = Some(Press::Card { id, from: at, at, moved: false });
        }
        (MouseButton::Left, Target::Title(board)) => {
            app.tasks.board = Some(board.clone());
            app.tasks.press = Some(Press::Board { id: board, from: at, at });
        }
        (MouseButton::Left, Target::Corner(board)) => {
            app.tasks.board = Some(board.clone());
            app.tasks.press = Some(Press::Resize { id: board, from: at, at });
        }
        (MouseButton::Left, Target::Chip(board, chip)) => super::flows::chip(app, &board, chip),
        (MouseButton::Left, Target::Cell { board, lane, status }) => {
            app.tasks.board = Some(board);
            app.tasks.cell = Some((lane, status));
            app.tasks.selected = None;
            app.tasks.press = Some(pan);
        }
        (MouseButton::Left, Target::Frame(board)) => {
            app.tasks.board = Some(board);
            app.tasks.press = Some(pan);
        }
        (MouseButton::Left | MouseButton::Middle, _) => app.tasks.press = Some(pan),
        (MouseButton::Right, Target::Card(id)) => {
            super::flows::show_todo(app, &id, false);
            super::flows::todo_menu(app, &id, at);
        }
        (MouseButton::Right, Target::Title(board) | Target::Corner(board) | Target::Chip(board, _) | Target::Frame(board) | Target::Cell { board, .. }) => super::flows::board_menu(app, &board, at),
        (MouseButton::Right, Target::Background) => super::flows::canvas_menu(app, at),
    }
}

fn drag(app: &mut App, to: (u16, u16)) {
    let d = div(app);
    match &mut app.tasks.press {
        Some(Press::Card { from, at, moved, .. }) => {
            *at = to;
            *moved |= to != *from;
        }
        Some(Press::Board { at, .. } | Press::Resize { at, .. }) => *at = to,
        Some(Press::Pan { from, pan }) => {
            let (from, pan) = (*from, *pan);
            app.tasks.pan = (pan.0 - (to.0 as i32 - from.0 as i32) * d, pan.1 - (to.1 as i32 - from.1 as i32) * d);
        }
        None => {}
    }
}

fn up(app: &mut App, to: (u16, u16)) {
    drag(app, to);
    let d = div(app);
    let Some(press) = app.tasks.press.take() else { return };
    let op = match press {
        Press::Card { id, at, moved: true, .. } => book(app).and_then(|b| app.tasks.scene.drop_at(at.0 as i32, at.1 as i32, &id).and_then(|drop| layout::move_op(b, &id, &drop))),
        Press::Board { id, from, at } if from != at => book(app).and_then(|b| b.board(&id)).map(|b| TodoOp::MoveBoard { board: b.id.clone(), x: b.x + (at.0 as i32 - from.0 as i32) * d, y: b.y + (at.1 as i32 - from.1 as i32) * d, w: None, h: None }),
        Press::Resize { id, from, at } if from != at => book(app).and_then(|b| b.board(&id)).map(|b| TodoOp::MoveBoard { board: b.id.clone(), x: b.x, y: b.y, w: Some(b.w + (at.0 as i32 - from.0 as i32) * d), h: Some(b.h + (at.1 as i32 - from.1 as i32) * d) }),
        _ => None,
    };
    if let Some(op) = op {
        book::apply(app, op);
    }
}

fn wheel(app: &mut App, kind: MouseEventKind, modifiers: Modifiers) {
    if modifiers.intersects(Modifiers::ALT | Modifiers::CONTROL) {
        match kind {
            MouseEventKind::ScrollUp => zoom(app, 1),
            MouseEventKind::ScrollDown => zoom(app, -1),
            _ => {}
        }
        return;
    }
    let shift = modifiers.contains(Modifiers::SHIFT);
    let (dx, dy) = match kind {
        MouseEventKind::ScrollUp if shift => (-6, 0),
        MouseEventKind::ScrollDown if shift => (6, 0),
        MouseEventKind::ScrollUp => (0, -3),
        MouseEventKind::ScrollDown => (0, 3),
        MouseEventKind::ScrollLeft => (-6, 0),
        MouseEventKind::ScrollRight => (6, 0),
        _ => (0, 0),
    };
    pan(app, dx, dy);
}

/// Moves the view over the board by whole cells (positive: right and down).
pub(crate) fn pan(app: &mut App, dx: i32, dy: i32) {
    let d = div(app);
    app.tasks.pan = (app.tasks.pan.0 + dx * d, app.tasks.pan.1 + dy * d);
    app.needs_draw = true;
}

/// Zooms in (`delta` > 0) or out a level, keeping the middle of the area in place.
pub(crate) fn zoom(app: &mut App, delta: i32) {
    let next = (app.tasks.zoom as i32 - delta.signum()).clamp(0, 2) as u8;
    if next == app.tasks.zoom {
        return;
    }
    let area = app.layout.canvas;
    let before = view(app, area);
    let (cx, cy) = (area.x as i32 + area.width as i32 / 2, area.y as i32 + area.height as i32 / 2);
    let (wx, wy) = before.to_world(cx, cy);
    app.tasks.zoom = next;
    let d = layout::DIVS[next as usize];
    app.tasks.pan = (wx - (area.width as i32 / 2) * d, wy - (area.height as i32 / 2) * d);
}

/// F: every board in view.
pub(crate) fn fit(app: &mut App) {
    let area = app.layout.canvas;
    if let Some((pan, zoom)) = book(app).map(|b| layout::fit(b, area)) {
        app.tasks.pan = pan;
        app.tasks.zoom = zoom;
    }
}

/// A key while the main area has the keyboard.
pub fn key(app: &mut App, key: KeyEvent) -> bool {
    let m = key.modifiers;
    let (ctrl, alt) = (m.contains(Modifiers::CONTROL), m.contains(Modifiers::ALT));
    let d = div(app);
    let none = app.tasks.selected.is_none() && app.tasks.cell.is_none();
    let selected = app.tasks.selected.clone();
    let dir = match key.code {
        KeyCode::Up => Some(Dir::Up),
        KeyCode::Down => Some(Dir::Down),
        KeyCode::Left => Some(Dir::Left),
        KeyCode::Right => Some(Dir::Right),
        _ => None,
    };
    if let Some(dir) = dir {
        if alt {
            return false;
        }
        // Nothing selected yet: the first card that shows (else the first one, brought into view).
        let first = if none && !ctrl { first_card(&app.tasks.scene) } else { None };
        if let Some(id) = first {
            super::flows::select_todo(app, &id, false, true);
        } else if ctrl || none {
            let (dx, dy) = match dir {
                Dir::Up => (0, -4),
                Dir::Down => (0, 4),
                Dir::Left => (-8, 0),
                Dir::Right => (8, 0),
            };
            app.tasks.pan = (app.tasks.pan.0 + dx * d, app.tasks.pan.1 + dy * d);
        } else {
            move_selection(app, dir);
        }
        return true;
    }
    if ctrl || alt {
        return false;
    }
    match key.code {
        KeyCode::Enter => match selected {
            Some(id) => super::flows::show_todo(app, &id, true),
            None => return false,
        },
        KeyCode::Char(' ') => match selected {
            Some(id) => super::flows::run(app, Action::Peek(id)),
            None => return false,
        },
        KeyCode::Char('n' | 'N') => new_todo_here(app),
        KeyCode::Char('a' | 'A') | KeyCode::Char('d' | 'D') => {
            let Some(id) = selected else { return false };
            let accept = matches!(key.code, KeyCode::Char('a' | 'A'));
            if book(app).and_then(|b| b.todo(&id)).is_some_and(|t| t.status == Status::Proposed) {
                book::apply(app, if accept { TodoOp::Accept { id, status: None } } else { TodoOp::Decline { id, reason: None } });
            } else {
                app.notify(format!("{id} is not a proposal (A and D accept and decline proposals)."), false);
            }
        }
        KeyCode::Delete => match selected {
            Some(id) => super::flows::run(app, Action::ConfirmCancel(id)),
            None => return false,
        },
        KeyCode::Char('e' | 'E') => match selected {
            Some(id) => super::flows::run(app, Action::EditBody(id)),
            None => return false,
        },
        KeyCode::Char('r' | 'R') => match selected {
            Some(id) => super::flows::run(app, Action::Ask(super::Ask::RenameTodo(id))),
            None => return false,
        },
        KeyCode::Char('+' | '=') => zoom(app, 1),
        KeyCode::Char('-' | '_') => zoom(app, -1),
        KeyCode::Char('f' | 'F') => fit(app),
        // The details on the right scroll.
        KeyCode::PageUp => app.tasks.detail_scroll = app.tasks.detail_scroll.saturating_sub(10),
        KeyCode::PageDown => app.tasks.detail_scroll = app.tasks.detail_scroll.saturating_add(10),
        KeyCode::Char('/') if app.side_view == crate::side::SideView::Sessions && app.doc.ui.sidebar_visible => {
            app.tasks.filter_active = true;
            app.set_focus(crate::app::Focus::Sidebar);
        }
        KeyCode::Esc if app.tasks.press.is_some() => app.tasks.press = None,
        KeyCode::Esc if !none || matches!(app.right_detail, Some(crate::side::RightDetail::Todo(_))) => {
            app.tasks.selected = None;
            app.tasks.cell = None;
            if matches!(app.right_detail, Some(crate::side::RightDetail::Todo(_))) {
                app.right_detail = None;
            }
        }
        _ => return false,
    }
    true
}

/// The first card that shows in the canvas's area, else the first card.
pub(crate) fn first_card(scene: &layout::Scene) -> Option<String> {
    let cards = || scene.frames.iter().flat_map(|f| &f.cards);
    cards().find(|c| c.rect.clipped(scene.view.area).is_some()).or_else(|| cards().next()).map(|c| c.id.clone())
}

/// N: the new-todo dialog for the selected column (Backlog for Proposed,
/// Done or nothing selected), on the selected board or the first one.
fn new_todo_here(app: &mut App) {
    let Some(book) = book(app) else {
        app.notify("The todo boards are not loaded yet.", true);
        return;
    };
    let here = app.tasks.selected.as_deref().and_then(|id| book.todo(id)).map(|t| (t.board.clone(), t.status)).or_else(|| app.tasks.cell.clone());
    let here = here.or_else(|| app.tasks.board.clone().or_else(|| book.boards.iter().find(|b| b.parent.is_none()).map(|b| b.id.clone())).map(|b| (b, Status::Backlog)));
    match here {
        Some((lane, status)) => {
            let status = if matches!(status, Status::Proposed | Status::Done | Status::Canceled) { Status::Backlog } else { status };
            super::flows::run(app, Action::NewTodo { lane, status });
        }
        None => super::flows::run(app, Action::NewBoard),
    }
}

/// Arrows: the next card or column (see [`layout::step`]); past a board's
/// edge, the next board's (boards in the order they stand, left to right,
/// then top to bottom). Details shown follow the selection; the card, or
/// the board reached, comes into view.
fn move_selection(app: &mut App, dir: Dir) {
    let next = {
        let Some(book) = book(app) else { return };
        let filter = filter(app);
        let mut tops: Vec<&canvas_model::todos::Board> = book.boards.iter().filter(|b| b.parent.is_none()).collect();
        tops.sort_by_key(|b| (b.x, b.y));
        let tops: Vec<String> = tops.into_iter().map(|b| b.id.clone()).collect();
        let current = app.tasks.selected.as_deref().and_then(|id| book.todo(id)).map(|t| (t.board.clone(), t.status, Some(t.id.clone()))).or_else(|| app.tasks.cell.clone().map(|(lane, status)| (lane, status, None)));
        let Some((lane, status, id)) = current else { return };
        let Some(root) = book.root(&lane).map(|b| b.id.clone()) else { return };
        let Some(grid) = layout::grid(book, &root, &filter) else { return };
        let spot = id.and_then(|id| grid.find(&id)).or_else(|| grid.spot(&lane, status)).unwrap_or_default();
        let (grid, spot) = match layout::step(&grid, &spot, dir) {
            Ok(spot) => (grid, spot),
            Err(delta) => {
                let i = tops.iter().position(|b| *b == grid.board).unwrap_or(0) as i32 + delta;
                match tops.get(i.max(0) as usize).filter(|_| i >= 0).and_then(|b| layout::grid(book, b, &filter)) {
                    Some(other) if !other.columns.is_empty() && !other.lanes.is_empty() => {
                        let column = if delta > 0 { 0 } else { other.columns.len() - 1 };
                        let lane = spot.lane.min(other.lanes.len() - 1);
                        let card = (!other.cells[lane][column].is_empty()).then_some(0);
                        (other, layout::Spot { lane, column, card })
                    }
                    _ => (grid, spot),
                }
            }
        };
        let (Some(lane), Some(status)) = (grid.lanes.get(spot.lane), grid.columns.get(spot.column)) else { return };
        (grid.board.clone(), lane.clone(), *status, grid.id(&spot).map(str::to_owned))
    };
    let (board, lane, status, id) = next;
    let other_board = app.tasks.board.as_deref() != Some(board.as_str());
    app.tasks.board = Some(board.clone());
    app.tasks.cell = Some((lane, status));
    app.tasks.selected = id.clone();
    match id {
        Some(id) => {
            if matches!(app.right_detail, Some(crate::side::RightDetail::Todo(_))) {
                app.right_detail = Some(crate::side::RightDetail::Todo(id.clone()));
                app.tasks.detail_scroll = 0;
            }
            super::flows::reveal(app, &id);
        }
        // An empty column of another board: that board comes into view.
        None if other_board => app.tasks.reveal = Some(Reveal::Board(board)),
        None => {}
    }
}
