//! The explorer (the Notebook's sidebar): the roots as a lazy tree, the
//! Human | All | AI filter, the `/` name filter, *Show internal files*, and
//! its keys, clicks and context menu.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::tree::{self, RowKind};
use super::{Action, Filter, Hit, Load, NamePurpose, file_name, join, load, norm, parent, same_path};
use crate::app::{App, Focus};
use crate::dialogs::{Action as AppAction, Hit as AppHit, MenuItem};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::paint::{ACCENT, ATTENTION, DARK, FAINT, GRAY, MUTED, NEGATIVE, PANEL, PANEL_HI, TEXT, bold, fg, fill, put, truncate};


fn hit(app: &mut App, rect: Rect, hit: Hit) {
    app.layout.hits.push((rect, AppHit::Notebook(hit)));
}

/// A clickable label; returns where the next one goes.
#[allow(clippy::too_many_arguments)]
fn button(app: &mut App, buf: &mut Buffer, x: i32, y: i32, label: &str, style: Style, target: Hit, clip: Rect) -> i32 {
    let end = put(buf, x, y, label, style, clip);
    if end > x {
        hit(app, Rect::new(x as u16, y as u16, (end - x) as u16, 1), target);
    }
    end
}

/// Draws the explorer; the caret of the name filter when it has the keyboard.
pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let bg = Style::default().bg(PANEL);
    fill(buf, area.x as i32, area.y as i32, area.width as i32, area.height as i32, bg, area);
    let focused = app.focus == Focus::Sidebar;
    let (x, mut y) = (area.x as i32 + 1, area.y as i32);
    let (icon, title) = crate::spaces::Space::Notebook.side();
    put(buf, x, y, &format!("{icon} {title}"), bold(if focused { ACCENT } else { GRAY }).bg(PANEL), area);
    let tree = tree::tree(app);
    // "N AI hidden" on the title row, on the right.
    if tree.hidden > 0 && app.notebook.filter != Filter::All {
        let label = app.notebook.filter.hidden_label(tree.hidden);
        let lx = (area.right() as i32 - 1 - crate::paint::text_width(&label) as i32).max(x + 12);
        put(buf, lx, y, &truncate(&label, (area.right() as i32 - lx - 1).max(0) as usize), fg(ATTENTION).bg(PANEL), area);
    }
    y += 1;
    let mut bx = button(app, buf, x, y, "+ Capture", fg(ACCENT).bg(PANEL), Hit::Capture, area) + 2;
    bx = button(app, buf, bx, y, "+ Note", fg(ACCENT).bg(PANEL), Hit::NewNote, area) + 2;
    button(app, buf, bx, y, "⟳", fg(GRAY).bg(PANEL), Hit::Refresh, area);
    y += 1;
    let mut sx = x;
    for (i, filter) in Filter::ALL.into_iter().enumerate() {
        if i > 0 {
            sx = put(buf, sx, y, "│", fg(FAINT).bg(PANEL), area);
        }
        let on = app.notebook.filter == filter;
        let style = if on { bold(ratatui::style::Color::Black).bg(ACCENT) } else { fg(GRAY).bg(PANEL_HI) };
        sx = button(app, buf, sx, y, &format!(" {} ", filter.label()), style, Hit::Filter(filter), area);
    }
    y += 1;
    let width = area.width.saturating_sub(2);
    let filter_rect = Rect::new(area.x + 1, y as u16, width, 1);
    let mut cursor = None;
    if app.notebook.filtering || !app.notebook.name_filter.text.is_empty() {
        put(buf, x, y, "/", fg(ACCENT).bg(PANEL), area);
        cursor = crate::ui_modals::field(buf, x + 2, y, width as i32 - 2, &app.notebook.name_filter, app.notebook.filtering && focused, "", area);
    } else {
        put(buf, x, y, "/ filter names", fg(DARK).bg(PANEL), area);
    }
    hit(app, filter_rect, Hit::NameFilter);
    y += 1;
    // The tree, then *Show internal files* on the last row.
    let footer = area.bottom() as i32 - 1;
    let rows_area = Rect::new(area.x, y as u16, area.width, (footer - y).max(0) as u16);
    draw_rows(app, buf, rows_area, &tree.rows);
    let internal = if app.notebook.show_internal { "[x] Show internal files" } else { "[ ] Show internal files" };
    button(app, buf, x, footer, internal, fg(GRAY).bg(PANEL), Hit::Internal, area);
    cursor
}

/// The tree's rows (scrolled), or why there are none.
fn draw_rows(app: &mut App, buf: &mut Buffer, area: Rect, rows: &[tree::Row]) {
    let failed = matches!(app.notebook.info_load, Load::Failed(_)) && app.notebook.info.is_none();
    let skip = if failed { 4 } else { 0 };
    let height = (area.height as usize).saturating_sub(skip);
    app.notebook.side_rows = height;
    if let Load::Failed(error) = app.notebook.info_load.clone()
        && failed
    {
        let x = area.x as i32 + 1;
        let room = (area.width as usize).saturating_sub(2);
        put(buf, x, area.y as i32, &truncate("The notebook is not available:", room), fg(NEGATIVE).bg(PANEL), area);
        put(buf, x, area.y as i32 + 1, &truncate(&error, room), fg(MUTED).bg(PANEL), area);
        button(app, buf, x, area.y as i32 + 2, "⟳ Retry", fg(ACCENT).bg(PANEL), Hit::Retry, area);
    }
    let selected = app.notebook.selected.clone();
    let at = selected.as_ref().and_then(|key| rows.iter().position(|r| &r.key == key));
    if let Some(at) = at.filter(|_| app.notebook.follow) {
        if at < app.notebook.scroll {
            app.notebook.scroll = at;
        } else if height > 0 && at >= app.notebook.scroll + height {
            app.notebook.scroll = at + 1 - height;
        }
    }
    app.notebook.follow = false;
    app.notebook.scroll = app.notebook.scroll.min(rows.len().saturating_sub(height));
    let focused = app.focus == Focus::Sidebar;
    for (i, row) in rows.iter().enumerate().skip(app.notebook.scroll).take(height) {
        let y = area.y as i32 + (i - app.notebook.scroll + skip) as i32;
        let chosen = Some(i) == at;
        let bg = if chosen { PANEL_HI } else { PANEL };
        if chosen {
            fill(buf, area.x as i32, y, area.width as i32, 1, Style::default().bg(bg), area);
            if focused {
                put(buf, area.x as i32, y, "▌", fg(ACCENT).bg(bg), area);
            }
        }
        let x = area.x as i32 + 1 + row.depth as i32 * 2;
        let (lead, label, style) = row_look(app, row);
        let mut end = put(buf, x, y, lead, style.bg(bg), area);
        end = put(buf, end, y, &truncate(&label, (area.right() as i32 - end - 1).max(0) as usize), style.bg(bg), area);
        if let Some(group) = &row.group {
            let mut marks = String::new();
            if !matches!(&row.kind, RowKind::Root(root) if root.kind == tree::RootKind::Group) {
                marks.push_str(" ◫");
            }
            if crate::tasks::group_counts(app, group).is_some() {
                marks.push_str(" ☑");
            }
            put(buf, end, y, &marks, fg(ACCENT).bg(bg), area);
        }
        hit(app, Rect::new(area.x, y as u16, area.width, 1), Hit::Row(row.key.clone()));
    }
}

/// A row's lead (▸ ▾ ● ◇), label and style: agents' notes dimmed and italic.
fn row_look(app: &App, row: &tree::Row) -> (&'static str, String, Style) {
    let arrow = if row.expanded { "▾ " } else { "▸ " };
    match &row.kind {
        RowKind::Root(root) => (arrow, root.label.clone(), bold(TEXT)),
        RowKind::Dir(entry, _) => {
            let style = if entry.provenance.internal { fg(DARK) } else { fg(TEXT) };
            (arrow, entry.name.clone(), style)
        }
        RowKind::File(entry, _) => {
            let ai = entry.provenance.audience == canvas_model::notebook::Audience::Ai;
            let mut style = if ai { fg(MUTED).add_modifier(Modifier::ITALIC) } else { fg(TEXT) };
            if entry.provenance.internal {
                style = fg(DARK);
            }
            if app.notebook.tab().is_some_and(|t| same_path(&t.path, &entry.path)) {
                style = style.add_modifier(Modifier::BOLD);
            }
            let lead = if ai { "◇ " } else { "● " };
            (lead, entry.name.clone(), style)
        }
        RowKind::Reports(count) => (arrow, format!("Reports ({count})"), bold(TEXT)),
        RowKind::Report(_, title, unread) => ("● ", title.clone(), if *unread { fg(ATTENTION) } else { fg(TEXT) }),
        RowKind::Message(text) => ("", text.clone(), fg(MUTED).add_modifier(Modifier::ITALIC)),
    }
}

// ---------- actions ----------

pub(super) fn set_filter(app: &mut App, filter: Filter) {
    app.notebook.filter = filter;
    app.notebook.follow = true;
    app.needs_draw = true;
}

/// After a filter change: the selected row scrolls into view if it is still there.
pub(super) fn keep_selection_visible(app: &mut App) {
    app.notebook.follow = true;
    app.needs_draw = true;
}

/// A path the explorer knows as a folder (a root, a listed folder).
pub(super) fn is_folder(app: &App, path: &str) -> bool {
    let key = norm(path);
    tree::roots(app.notebook.info.as_ref(), &app.doc).iter().any(|r| norm(&r.path) == key)
        || app.notebook.folders.contains_key(&key)
        || app.notebook.folders.values().any(|f| f.entries.iter().any(|e| e.is_dir() && norm(&e.path) == key))
        || app.doc.groups.iter().any(|g| g.folder.as_deref().is_some_and(|f| norm(f) == key))
}

/// Expands the folders down to `path` (and `path` itself when `expand`),
/// lists what is not listed yet, and selects it. Before the notebook's
/// folders are known it waits for them.
pub(super) fn reveal(app: &mut App, path: &str, expand: bool) {
    // The notebook root may hold it: once it is known, again.
    if app.notebook.info.is_none() {
        app.notebook.pending_reveal = Some((path.to_owned(), expand));
    }
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    let Some(root) = tree::root_of(&roots, path).cloned() else { return };
    let rel = tree::rel(&root.path, path);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let mut dirs = vec![root.path.clone()];
    let mut dir = root.path.clone();
    for (i, part) in parts.iter().enumerate() {
        dir = join(&dir, part);
        if i + 1 < parts.len() || expand {
            dirs.push(dir.clone());
        }
    }
    if !expand && parts.is_empty() {
        dirs.clear();
    }
    for dir in dirs {
        app.notebook.expanded.insert(norm(&dir));
        if !app.notebook.folders.contains_key(&norm(&dir)) {
            load::list(app, &dir, &root.path);
        }
    }
    app.notebook.selected = Some(norm(path));
    app.notebook.follow = true;
    app.needs_draw = true;
}

/// The folder a new note goes to: the selected folder, the selected note's
/// folder, else the notebook root.
fn selected_folder(app: &App) -> Option<String> {
    let tree = tree::tree(app);
    let row = app.notebook.selected.as_ref().and_then(|key| tree.rows.iter().find(|r| &r.key == key));
    match row.map(|r| &r.kind) {
        Some(RowKind::Root(root)) => Some(root.path.clone()),
        Some(RowKind::Dir(entry, _)) => Some(entry.path.clone()),
        Some(RowKind::File(entry, _)) => parent(&entry.path),
        _ => app.notebook.info.as_ref().map(|i| i.root.clone()),
    }
}

/// `+ Note` / `Ctrl+N`: a new note in the selected folder (asks its name).
pub(super) fn new_note_here(app: &mut App) {
    match selected_folder(app) {
        Some(folder) => super::dialogs::ask_name(app, NamePurpose::NewNote { folder }),
        None => app.notify("The notebook's folder is not known yet.", true),
    }
}

/// Opens or closes a folder row; opens a note or a report.
fn activate(app: &mut App, row: &tree::Row, toggle: bool) {
    match &row.kind {
        RowKind::Root(_) | RowKind::Dir(..) | RowKind::Reports(_) => {
            if row.expanded && toggle {
                app.notebook.expanded.remove(&row.key);
            } else {
                app.notebook.expanded.insert(row.key.clone());
                match &row.kind {
                    RowKind::Root(root) => load::list(app, &root.path, &root.path),
                    RowKind::Dir(entry, root) => load::list(app, &entry.path, root),
                    _ => {}
                }
            }
        }
        RowKind::File(entry, _) => super::tabs::open_note(app, &entry.path),
        RowKind::Report(id, _, _) => app.open_report(id),
        RowKind::Message(_) => {}
    }
    app.needs_draw = true;
}

/// A click on a row: select it; left opens or toggles, right shows its menu.
pub(super) fn click_row(app: &mut App, key: String, mouse: MouseEvent) {
    let tree = tree::tree(app);
    let Some(row) = tree.rows.iter().find(|r| r.key == key).cloned() else { return };
    app.notebook.selected = Some(key);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Right) => menu(app, &row, mouse.column, mouse.row),
        MouseEventKind::Down(_) => activate(app, &row, true),
        _ => {}
    }
}

/// A row's context menu.
fn menu(app: &mut App, row: &tree::Row, x: u16, y: u16) {
    let item = |label: &str, hint: &str, action: Action| MenuItem { label: label.into(), hint: hint.into(), action: AppAction::Notebook(action), enabled: true };
    let mut items = Vec::new();
    let (path, file, root) = match &row.kind {
        RowKind::Root(root) => (root.path.clone(), false, true),
        RowKind::Dir(entry, _) => (entry.path.clone(), false, false),
        RowKind::File(entry, _) => (entry.path.clone(), true, false),
        RowKind::Report(id, _, _) => {
            let open = MenuItem { label: "Open report".into(), hint: "Enter".into(), action: AppAction::Report(crate::reports::ReportAction::Open(id.clone())), enabled: true };
            return app.open_menu(x, y, ("Report".into(), vec![open]));
        }
        _ => return,
    };
    if file {
        items.push(item("Open", "Enter", Action::OpenPath(path.clone())));
    } else {
        items.push(item("New note…", "Ctrl+N", Action::NewNote(path.clone())));
        items.push(item("New folder…", "", Action::NewFolder(path.clone())));
    }
    if !root {
        items.push(item("Rename…", "F2", Action::Rename(path.clone())));
        items.push(item("Move to…", "", Action::Move(path.clone())));
        items.push(item("Delete", "Del", Action::ConfirmTrash(path.clone())));
    }
    items.push(item("Copy path", "", Action::CopyPath(path.clone())));
    items.push(item("Open externally", "", Action::OpenExternally(path.clone())));
    if file && matches!(canvas_model::notebook::Kind::of(&path), canvas_model::notebook::Kind::Markdown | canvas_model::notebook::Kind::Html) {
        items.push(item("Mark reviewed", "", Action::MarkReviewed(path.clone())));
    }
    if let Some(group) = row.group.clone().or_else(|| group_of_path(app, &path)) {
        items.push(item("Show group on canvas", "", Action::ShowGroup(group)));
    }
    app.open_menu(x, y, (file_name(&path), items));
}

/// The group whose folder holds `path`.
pub(super) fn group_of_path(app: &App, path: &str) -> Option<String> {
    app.doc
        .groups
        .iter()
        .filter_map(|g| g.folder.as_deref().map(str::trim).filter(|f| !f.is_empty() && super::is_inside(path, f)).map(|f| (g.id.clone(), f.len())))
        .max_by_key(|(_, len)| *len)
        .map(|(id, _)| id)
}

// ---------- keys, text and mouse ----------

/// Moves the selection by `delta` rows (messages are skipped).
fn step(app: &mut App, rows: &[tree::Row], delta: i64) {
    let selectable: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| !matches!(r.kind, RowKind::Message(_))).map(|(i, _)| i).collect();
    if selectable.is_empty() {
        return;
    }
    let current = app.notebook.selected.as_ref().and_then(|key| selectable.iter().position(|&i| &rows[i].key == key));
    let next = match current {
        Some(at) => (at as i64 + delta).clamp(0, selectable.len() as i64 - 1) as usize,
        None => 0,
    };
    app.notebook.selected = Some(rows[selectable[next]].key.clone());
    app.notebook.follow = true;
}

/// A key while the explorer has the keyboard.
pub(super) fn key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(Modifiers::CONTROL) && !key.modifiers.contains(Modifiers::ALT);
    let tree = tree::tree(app);
    if app.notebook.filtering {
        match key.code {
            KeyCode::Esc => {
                app.notebook.filtering = false;
                app.notebook.name_filter.set("");
            }
            KeyCode::Enter | KeyCode::Tab => app.notebook.filtering = false,
            KeyCode::Up => step(app, &tree.rows, -1),
            KeyCode::Down => step(app, &tree.rows, 1),
            _ => {
                if app.notebook.name_filter.handle(&key) {
                    app.notebook.scroll = 0;
                }
            }
        }
        return true;
    }
    let page = app.notebook.side_rows.max(2) as i64 - 1;
    let row = app.notebook.selected.as_ref().and_then(|k| tree.rows.iter().find(|r| &r.key == k)).cloned();
    match key.code {
        KeyCode::Up => step(app, &tree.rows, -1),
        KeyCode::Down => step(app, &tree.rows, 1),
        KeyCode::PageUp => step(app, &tree.rows, -page),
        KeyCode::PageDown => step(app, &tree.rows, page),
        KeyCode::Home => step(app, &tree.rows, -1_000_000),
        KeyCode::End => step(app, &tree.rows, 1_000_000),
        KeyCode::Enter => match row {
            Some(row) => activate(app, &row, true),
            None => step(app, &tree.rows, 0),
        },
        KeyCode::Right => match row {
            Some(row) if row.is_folder() && row.expanded => step(app, &tree.rows, 1),
            Some(row) => activate(app, &row, false),
            None => step(app, &tree.rows, 0),
        },
        KeyCode::Left => match row {
            Some(row) if row.is_folder() && row.expanded => {
                app.notebook.expanded.remove(&row.key);
            }
            Some(row) => {
                // To the folder holding it.
                let at = tree.rows.iter().position(|r| r.key == row.key).unwrap_or(0);
                if let Some(up) = tree.rows[..at].iter().rev().find(|r| r.depth < row.depth) {
                    app.notebook.selected = Some(up.key.clone());
                    app.notebook.follow = true;
                }
            }
            None => step(app, &tree.rows, 0),
        },
        KeyCode::Char('/') if !ctrl => app.notebook.filtering = true,
        KeyCode::F(2) => {
            if let Some(path) = row.as_ref().filter(|r| matches!(r.kind, RowKind::Dir(..) | RowKind::File(..))).and_then(|r| r.path().map(str::to_owned)) {
                super::run(app, Action::Rename(path));
            }
        }
        KeyCode::Delete => {
            if let Some(path) = row.as_ref().filter(|r| matches!(r.kind, RowKind::Dir(..) | RowKind::File(..))).and_then(|r| r.path().map(str::to_owned)) {
                super::run(app, Action::ConfirmTrash(path));
            }
        }
        KeyCode::Esc if !app.notebook.name_filter.text.is_empty() => app.notebook.name_filter.set(""),
        _ => return false,
    }
    app.needs_draw = true;
    true
}

/// Typed or pasted text while the explorer has the keyboard: the name filter.
pub(super) fn text(app: &mut App, text: &str) -> bool {
    if app.notebook.filtering {
        app.notebook.name_filter.insert(text);
        app.notebook.scroll = 0;
        return true;
    }
    if text == "/" {
        app.notebook.filtering = true;
        return true;
    }
    false
}

/// The wheel over the explorer scrolls it.
pub(super) fn mouse(app: &mut App, mouse: MouseEvent) {
    let delta: i64 = match mouse.kind {
        MouseEventKind::ScrollUp => -3,
        MouseEventKind::ScrollDown => 3,
        _ => return,
    };
    app.notebook.scroll = (app.notebook.scroll as i64 + delta).max(0) as usize;
    app.needs_draw = true;
}
