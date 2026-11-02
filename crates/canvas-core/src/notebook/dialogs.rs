//! The Notebook's dialogs: names (new note, new folder, rename, the
//! notebook folder setting), *Move to…*, *Compare* (a line diff of a
//! conflict), *Ask agent to revise…* and the mounts list.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::{Hit, SettingRow, file_name, is_absolute, norm, ops, parent, tree};
use crate::app::App;
use crate::dialogs::{Hit as AppHit, Modal, TextInput};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseEvent, MouseEventKind};
use crate::modals::Outcome;
use crate::paint::{ACCENT, GRAY, MODAL, MUTED, NEGATIVE, PANEL_HI, POSITIVE, TEXT, bold, fg, fill, put, truncate, truncate_left};
use crate::ui_modals::{Ctx, button, centered, field, panel};

/// What a name dialog is for.
#[derive(Clone, Debug, PartialEq)]
pub enum NamePurpose {
    /// A new note in this folder.
    NewNote { folder: String },
    /// A new folder in this one.
    NewFolder { folder: String },
    /// A new name for a note or folder.
    Rename { path: String },
    /// The notebook root (the Settings row; empty: the default).
    RootFolder,
}

/// The module's dialogs (`Modal::Notebook`).
#[derive(Clone, Debug, PartialEq)]
pub enum Dialog {
    /// A message with an OK button.
    Info { title: String, lines: Vec<String> },
    /// One line of text.
    Name { purpose: NamePurpose, input: TextInput, error: Option<String> },
    /// Where to move a note or folder: a path, or a pick among known folders.
    Move { path: String, input: TextInput, sel: usize },
    /// A conflict's line diff: `+` yours, `-` the file's.
    Compare { path: String, lines: Vec<(char, String)>, top: usize },
    /// A session of the note's group to ask for a revision, and what to change.
    Ask { path: String, sessions: Vec<(String, String)>, sel: usize, input: TextInput },
    /// The extra folders the explorer shows.
    Mounts { input: TextInput, sel: usize },
}

fn push(app: &mut App, dialog: Dialog) {
    app.modals.push(Modal::Notebook(Box::new(dialog)));
    app.needs_draw = true;
}

/// Asks a name (new note, new folder, rename, the notebook folder).
pub(super) fn ask_name(app: &mut App, purpose: NamePurpose) {
    let text = match &purpose {
        NamePurpose::Rename { path } => file_name(path),
        NamePurpose::RootFolder => app.doc.notebook_root.clone().unwrap_or_default(),
        _ => String::new(),
    };
    ask_name_prefilled(app, purpose, &text);
}

pub(super) fn ask_name_prefilled(app: &mut App, purpose: NamePurpose, text: &str) {
    push(app, Dialog::Name { purpose, input: TextInput::new(text), error: None });
}

/// *Move to…*: starts at the folder holding it.
pub(super) fn ask_move(app: &mut App, path: String) {
    let start = parent(&path).unwrap_or_default();
    push(app, Dialog::Move { path, input: TextInput::new(&start), sel: 0 });
}

/// A Settings row: the notebook folder, or the mounts.
pub(super) fn setting(app: &mut App, row: SettingRow) {
    match row {
        SettingRow::Folder => ask_name(app, NamePurpose::RootFolder),
        SettingRow::Mounts => push(app, Dialog::Mounts { input: TextInput::default(), sel: 0 }),
    }
}

/// *Compare* for the shown tab's conflict.
pub(super) fn compare(app: &mut App) {
    let Some(tab) = app.notebook.tab() else { return };
    let Some(theirs) = &tab.conflict else { return };
    let lines = diff(&tab.text(), &theirs.text);
    let path = tab.path.clone();
    push(app, Dialog::Compare { path, lines, top: 0 });
}

/// *Ask agent to revise…*: the sessions of the note's group (else every session).
pub(super) fn ask_agent(app: &mut App) {
    let Some(path) = app.notebook.tab().map(|t| t.path.clone()) else { return };
    let sessions = sessions_for(app, &path);
    if sessions.is_empty() {
        return app.notify("There is no session on this canvas to ask.", true);
    }
    push(app, Dialog::Ask { path, sessions, sel: 0, input: TextInput::default() });
}

/// Sessions to ask about a note: its group's (the folder holding it, else
/// the group of the session that wrote it), else every session.
pub(super) fn sessions_for(app: &App, path: &str) -> Vec<(String, String)> {
    use crate::doc::NodeKind;
    let written_by = app.notebook.tab().and_then(|t| t.provenance.session.clone()).and_then(|s| super::reader::session_node(app, &s)).and_then(|(id, _)| app.doc.node(&id).and_then(|n| app.doc.group_of(n)).map(|g| g.id.clone()));
    let group = super::explorer::group_of_path(app, path).or(written_by);
    let sessions = |members: Option<Vec<String>>| -> Vec<(String, String)> {
        app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session && n.passive.is_none() && members.as_ref().is_none_or(|m| m.contains(&n.id))).map(|n| (n.id.clone(), n.name.clone())).collect()
    };
    match group.map(|g| sessions(Some(app.doc.members(&g)))).filter(|s| !s.is_empty()) {
        Some(list) => list,
        None => sessions(None),
    }
}

/// Most lines [`diff`] compares (longer texts: a note says so).
const DIFF_MAX: usize = 2_000;

/// A line diff of `mine` against `theirs`: `' '` both, `'+'` mine only, `'-'` theirs only.
pub fn diff(mine: &str, theirs: &str) -> Vec<(char, String)> {
    let a: Vec<&str> = mine.lines().collect();
    let b: Vec<&str> = theirs.lines().collect();
    if a.len() > DIFF_MAX || b.len() > DIFF_MAX {
        return vec![(' ', format!("Too long to compare here ({} and {} lines).", a.len(), b.len()))];
    }
    // Common prefix and suffix first, an LCS table for the middle.
    let start = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let end = a[start..].iter().rev().zip(b[start..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[start..a.len() - end], &b[start..b.len() - end]);
    let mut table = vec![vec![0u32; mb.len() + 1]; ma.len() + 1];
    for i in (0..ma.len()).rev() {
        for j in (0..mb.len()).rev() {
            table[i][j] = if ma[i] == mb[j] { table[i + 1][j + 1] + 1 } else { table[i + 1][j].max(table[i][j + 1]) };
        }
    }
    let mut out: Vec<(char, String)> = a[..start].iter().map(|l| (' ', (*l).to_owned())).collect();
    let (mut i, mut j) = (0, 0);
    while i < ma.len() || j < mb.len() {
        if i < ma.len() && j < mb.len() && ma[i] == mb[j] {
            out.push((' ', ma[i].to_owned()));
            i += 1;
            j += 1;
        } else if j < mb.len() && (i == ma.len() || table[i][j + 1] >= table[i + 1][j]) {
            out.push(('-', mb[j].to_owned()));
            j += 1;
        } else {
            out.push(('+', ma[i].to_owned()));
            i += 1;
        }
    }
    out.extend(a[a.len() - end..].iter().map(|l| (' ', (*l).to_owned())));
    out
}

/// Folders a note can move to: the roots, the listed folders and group
/// folders, matching `query` (any case; all for an absolute path).
pub(super) fn move_targets(app: &App, query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let roots = tree::roots(app.notebook.info.as_ref(), &app.doc);
    let listed = app.notebook.folders.values().flat_map(|f| f.entries.iter().filter(|e| e.is_dir() && !e.provenance.internal).map(|e| e.path.clone()));
    let query = query.trim().to_lowercase();
    for path in roots.into_iter().map(|r| r.path).chain(listed) {
        if !seen.insert(norm(&path)) {
            continue;
        }
        if query.is_empty() || is_absolute(&query) || path.to_lowercase().contains(&query) {
            out.push(path);
        }
    }
    out.sort_by_key(|p| p.to_lowercase());
    out
}

// ---------- drawing ----------

fn name_title(app: &App, purpose: &NamePurpose) -> (String, String) {
    match purpose {
        NamePurpose::NewNote { folder } => (format!("New note in {}", file_name(folder)), "A name; .md is added unless it has an extension.".into()),
        NamePurpose::NewFolder { folder } => (format!("New folder in {}", file_name(folder)), "The folder's name.".into()),
        NamePurpose::Rename { path } => (format!("Rename {}", file_name(path)), "The new name (same folder).".into()),
        NamePurpose::RootFolder => {
            let now = app.notebook.info.as_ref().map(|i| i.root.clone()).unwrap_or_else(|| "not known yet".into());
            ("Notebook folder".into(), format!("A folder on the service's machine; empty: <home>\\notebook. Now: {now}"))
        }
    }
}

/// A list of rows inside a dialog (`Hit::Pick`), scrolled to keep `sel` shown.
#[allow(clippy::too_many_arguments)]
fn list(buf: &mut Buffer, ctx: &mut Ctx, rect: Rect, x: i32, y: i32, rows: usize, items: &[String], sel: usize) {
    let width = rect.width as i32 - 6;
    let top = if sel < items.len() { sel.saturating_sub(rows.saturating_sub(1)) } else { 0 };
    for (i, item) in items.iter().enumerate().skip(top).take(rows) {
        let row_y = y + (i - top) as i32;
        let on = i == sel;
        let style = if on { bold(Color::Black).bg(ACCENT) } else { fg(TEXT).bg(PANEL_HI) };
        fill(buf, x, row_y, width, 1, style, rect);
        put(buf, x + 1, row_y, &truncate_left(item, (width as usize).saturating_sub(2)), style, rect);
        ctx.hit(Rect::new(x as u16, row_y as u16, width as u16, 1), AppHit::Notebook(Hit::Pick(i)));
    }
    if items.is_empty() {
        put(buf, x, y, "(none)", fg(MUTED).bg(MODAL), rect);
    }
}

/// Draws a dialog; its rectangle and caret.
pub(super) fn draw(app: &mut App, buf: &mut Buffer, screen: Rect, dialog: &Dialog, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let text = |buf: &mut Buffer, x: i32, y: i32, line: &str, style: Style, rect: Rect| put(buf, x, y, &truncate(line, rect.width.saturating_sub(6) as usize), style.bg(MODAL), rect);
    match dialog {
        Dialog::Info { title, lines } => {
            let width = lines.iter().map(|l| crate::paint::text_width(l)).max().unwrap_or(20).max(title.len()).max(40) as u16 + 6;
            let rect = centered(screen, width, lines.len() as u16 + 6);
            panel(buf, rect, title);
            for (i, line) in lines.iter().enumerate() {
                text(buf, rect.x as i32 + 3, rect.y as i32 + 2 + i as i32, line, fg(TEXT), rect);
            }
            button(buf, ctx, rect.x as i32 + 3, rect.bottom() as i32 - 2, "OK", true, true, 0, rect);
            (rect, None)
        }
        Dialog::Name { purpose, input, error } => {
            let (title, hint) = name_title(app, purpose);
            let rect = centered(screen, 72, 9);
            panel(buf, rect, &title);
            text(buf, rect.x as i32 + 3, rect.y as i32 + 2, &hint, fg(MUTED), rect);
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 3, rect.width as i32 - 6, input, true, "", rect);
            if let Some(error) = error {
                text(buf, rect.x as i32 + 3, rect.y as i32 + 4, error, fg(NEGATIVE), rect);
            }
            let y = rect.bottom() as i32 - 2;
            let x = button(buf, ctx, rect.x as i32 + 3, y, "OK", false, true, 0, rect);
            button(buf, ctx, x, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Dialog::Move { path, input, sel } => {
            let targets = move_targets(app, &input.text);
            let rect = centered(screen, 80, 18);
            panel(buf, rect, &format!("Move {} to…", file_name(path)));
            text(buf, rect.x as i32 + 3, rect.y as i32 + 2, "A folder (type to filter, or a full path); Enter moves.", fg(MUTED), rect);
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 3, rect.width as i32 - 6, input, true, "", rect);
            let shown = if is_absolute(input.text.trim()) { usize::MAX } else { *sel };
            list(buf, ctx, rect, rect.x as i32 + 3, rect.y as i32 + 5, 9, &targets, shown);
            let y = rect.bottom() as i32 - 2;
            let x = button(buf, ctx, rect.x as i32 + 3, y, "Move", false, true, 0, rect);
            button(buf, ctx, x, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Dialog::Compare { path, lines, top } => {
            let rect = centered(screen, screen.width.saturating_sub(8).min(120), screen.height.saturating_sub(4));
            panel(buf, rect, &format!("Compare {}", file_name(path)));
            text(buf, rect.x as i32 + 3, rect.y as i32 + 1, "+ yours (not saved)   - the file on disk", fg(MUTED), rect);
            let rows = rect.height.saturating_sub(5) as usize;
            let top = (*top).min(lines.len().saturating_sub(rows));
            for (i, (mark, line)) in lines.iter().enumerate().skip(top).take(rows) {
                let style = match mark {
                    '+' => fg(POSITIVE),
                    '-' => fg(NEGATIVE),
                    _ => fg(GRAY),
                };
                text(buf, rect.x as i32 + 3, rect.y as i32 + 2 + (i - top) as i32, &format!("{mark} {line}"), style, rect);
            }
            let y = rect.bottom() as i32 - 2;
            let mut x = button(buf, ctx, rect.x as i32 + 3, y, "Keep mine", false, true, 0, rect);
            x = button(buf, ctx, x, y, "Take theirs", false, false, 1, rect);
            button(buf, ctx, x, y, "Close", false, false, 2, rect);
            (rect, None)
        }
        Dialog::Ask { path, sessions, sel, input } => {
            let rect = centered(screen, 76, 18);
            panel(buf, rect, "Ask agent to revise…");
            text(buf, rect.x as i32 + 3, rect.y as i32 + 2, &format!("Sends {} to the session (typed in when it is idle).", file_name(path)), fg(MUTED), rect);
            let names: Vec<String> = sessions.iter().map(|(_, name)| name.clone()).collect();
            list(buf, ctx, rect, rect.x as i32 + 3, rect.y as i32 + 3, 8, &names, *sel);
            text(buf, rect.x as i32 + 3, rect.y as i32 + 12, "What to change (optional):", fg(GRAY), rect);
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 13, rect.width as i32 - 6, input, true, "", rect);
            let y = rect.bottom() as i32 - 2;
            let x = button(buf, ctx, rect.x as i32 + 3, y, "Send", false, true, 0, rect);
            button(buf, ctx, x, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Dialog::Mounts { input, sel } => {
            let mounts = app.doc.notebook_mounts.clone();
            let rect = centered(screen, 80, 18);
            panel(buf, rect, "Notebook mounts");
            text(buf, rect.x as i32 + 3, rect.y as i32 + 2, "Extra folders the explorer shows (on the service's machine).", fg(MUTED), rect);
            list(buf, ctx, rect, rect.x as i32 + 3, rect.y as i32 + 3, 8, &mounts, *sel);
            text(buf, rect.x as i32 + 3, rect.y as i32 + 12, "Add a folder (Enter adds; Del removes the selected one):", fg(GRAY), rect);
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 13, rect.width as i32 - 6, input, true, "", rect);
            let y = rect.bottom() as i32 - 2;
            let mut x = button(buf, ctx, rect.x as i32 + 3, y, "Add", false, true, 0, rect);
            x = button(buf, ctx, x, y, "Remove", false, false, 1, rect);
            button(buf, ctx, x, y, "Close", false, false, 2, rect);
            (rect, cursor)
        }
    }
}

// ---------- input ----------

/// The dialog's main button (Enter, or button 0).
fn submit(app: &mut App, dialog: &mut Dialog) -> Outcome {
    match dialog {
        Dialog::Info { .. } | Dialog::Compare { .. } => Outcome::Close,
        Dialog::Name { purpose, input, error } => {
            let result = match purpose.clone() {
                NamePurpose::NewNote { folder } => ops::create_note(app, &folder, &input.text),
                NamePurpose::NewFolder { folder } => ops::create_folder(app, &folder, &input.text),
                NamePurpose::Rename { path } => ops::rename(app, &path, &input.text),
                NamePurpose::RootFolder => {
                    ops::set_root(app, &input.text);
                    Ok(())
                }
            };
            match result {
                Ok(()) => Outcome::Close,
                Err(why) => {
                    *error = Some(why);
                    Outcome::Keep
                }
            }
        }
        Dialog::Move { path, input, sel } => {
            let typed = input.text.trim().to_owned();
            let target = if is_absolute(&typed) { Some(typed) } else { move_targets(app, &input.text).get(*sel).cloned() };
            match target {
                Some(folder) => match ops::move_to(app, path, &folder) {
                    Ok(()) => Outcome::Close,
                    Err(why) => {
                        app.notify(why, true);
                        Outcome::Keep
                    }
                },
                None => Outcome::Keep,
            }
        }
        Dialog::Ask { path, sessions, sel, input } => match sessions.get(*sel) {
            Some((id, name)) => {
                ops::ask_revision(app, id, name, path, &input.text);
                Outcome::Close
            }
            None => Outcome::Keep,
        },
        Dialog::Mounts { input, .. } => {
            let text = input.text.trim().to_owned();
            if !text.is_empty() {
                ops::add_mount(app, &text);
                input.set("");
            }
            Outcome::Keep
        }
    }
}

/// A list dialog's selection moves (`len`: its rows).
fn step(sel: &mut usize, len: usize, delta: i64) {
    *sel = if len == 0 { 0 } else { (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize };
}

/// A key in a dialog.
pub(super) fn key(app: &mut App, dialog: &mut Dialog, key: KeyEvent) -> Outcome {
    if key.code == KeyCode::Esc {
        return Outcome::Close;
    }
    if key.code == KeyCode::Enter {
        return submit(app, dialog);
    }
    match dialog {
        Dialog::Info { .. } => {}
        Dialog::Name { input, error, .. } => {
            if input.handle(&key) {
                *error = None;
            }
        }
        Dialog::Move { input, sel, .. } => {
            let len = move_targets(app, &input.text).len();
            match key.code {
                KeyCode::Up => step(sel, len, -1),
                KeyCode::Down => step(sel, len, 1),
                _ => {
                    if input.handle(&key) {
                        *sel = 0;
                    }
                }
            }
        }
        Dialog::Compare { top, lines, .. } => {
            let max = lines.len();
            *top = match key.code {
                KeyCode::Up => top.saturating_sub(1),
                KeyCode::Down => (*top + 1).min(max),
                KeyCode::PageUp => top.saturating_sub(20),
                KeyCode::PageDown => (*top + 20).min(max),
                KeyCode::Home => 0,
                KeyCode::End => max,
                _ => *top,
            };
        }
        Dialog::Ask { sessions, sel, input, .. } => match key.code {
            KeyCode::Up => step(sel, sessions.len(), -1),
            KeyCode::Down => step(sel, sessions.len(), 1),
            _ => {
                input.handle(&key);
            }
        },
        Dialog::Mounts { input, sel } => match key.code {
            KeyCode::Up => step(sel, app.doc.notebook_mounts.len(), -1),
            KeyCode::Down => step(sel, app.doc.notebook_mounts.len(), 1),
            KeyCode::Delete if input.text.is_empty() => {
                ops::remove_mount(app, *sel);
                step(sel, app.doc.notebook_mounts.len(), 0);
            }
            _ => {
                input.handle(&key);
            }
        },
    }
    Outcome::Keep
}

/// The mouse in a dialog: its buttons and list rows; the wheel scrolls.
pub(super) fn mouse(app: &mut App, dialog: &mut Dialog, mouse: MouseEvent, hit: Option<AppHit>) -> Outcome {
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let code = if mouse.kind == MouseEventKind::ScrollUp { KeyCode::Up } else { KeyCode::Down };
            for _ in 0..3 {
                key(app, dialog, KeyEvent::new(code, Modifiers::NONE));
            }
            return Outcome::Keep;
        }
        MouseEventKind::Down(_) => {}
        _ => return Outcome::Keep,
    }
    let inside = app.layout.modal_rect.is_some_and(|r| crate::paint::rect_contains(r, mouse.column, mouse.row));
    match hit {
        // The note compared (another tab may show by now).
        Some(AppHit::Button(0)) => match dialog {
            Dialog::Compare { path, .. } => {
                super::tabs::keep_mine(app, path);
                Outcome::Close
            }
            _ => submit(app, dialog),
        },
        Some(AppHit::Button(1)) => match dialog {
            Dialog::Compare { path, .. } => {
                super::tabs::take_theirs(app, path);
                Outcome::Close
            }
            Dialog::Mounts { sel, .. } => {
                ops::remove_mount(app, *sel);
                step(sel, app.doc.notebook_mounts.len(), 0);
                Outcome::Keep
            }
            _ => Outcome::Close,
        },
        Some(AppHit::Button(_)) => Outcome::Close,
        Some(AppHit::Notebook(Hit::Pick(index))) => {
            match dialog {
                Dialog::Move { sel, input, .. } => {
                    if is_absolute(input.text.trim()) {
                        input.set("");
                    }
                    *sel = index;
                }
                Dialog::Ask { sel, .. } | Dialog::Mounts { sel, .. } => *sel = index,
                _ => {}
            }
            Outcome::Keep
        }
        // A press outside a dialog closes it.
        _ if !inside => Outcome::Close,
        _ => Outcome::Keep,
    }
}

/// Pasted or IME text: the dialog's field.
pub(super) fn text(_app: &mut App, dialog: &mut Dialog, text: &str) {
    match dialog {
        Dialog::Name { input, error, .. } => {
            input.insert(text);
            *error = None;
        }
        Dialog::Move { input, sel, .. } => {
            input.insert(text);
            *sel = 0;
        }
        Dialog::Ask { input, .. } | Dialog::Mounts { input, .. } => input.insert(text),
        Dialog::Info { .. } | Dialog::Compare { .. } => {}
    }
}
