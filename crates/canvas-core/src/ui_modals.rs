//! Drawing of modal dialogs; registers click targets for the top-most modal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::App;
use crate::dialogs::{GitState, Hit, Modal, NsField, TextInput};
use crate::paint::{ACCENT, DOUBLE, GRAY, POSITIVE, MODAL, MUTED, PANEL_HI, PURPLE, NEGATIVE, ROUND, SELECT, TEXT, ATTENTION, bold, fg, fill, outline, put, truncate, truncate_left};
use crate::paths::{format_age, now_ms};

const HELP: &[(&str, &str)] = &[
    ("Canvas (mouse)", ""),
    ("Click a box or sidebar row", "select and view it (never starts it)"),
    ("Double-click a box or row · Enter", "start / resume it, focus its terminal"),
    ("Drag a box", "move it (its subagents follow)"),
    ("Drag empty space / middle-drag", "pan"),
    ("Shift-drag or right-drag", "select several sessions"),
    ("Wheel / Shift+wheel", "pan vertical / horizontal"),
    ("Alt+wheel or Ctrl+wheel · toolbar − +", "zoom (100% · 50% · 25%)"),
    ("Drag a group title / ◢ corner", "move / resize the group"),
    ("Click a group's background / border", "select it (arrows move, Del deletes)"),
    ("Double-click empty space", "new session there"),
    ("Right-click", "context menu"),
    ("Keys", ""),
    ("N / S", "new session / subsession of the selection"),
    ("G", "group the selection (box drawn around it)"),
    ("I / V", "import agent sessions / sync VS Code agent sessions"),
    ("Alt+1…8 or activity bar", "Sessions · Automations · Plugins · MCP · Skills · Agents · VS Code · Settings"),
    ("Lists: Enter · Space · Del", "open/edit · enable/disable · remove"),
    ("Enter / Tab", "start selection / cycle sessions"),
    ("Arrows / Shift / Ctrl", "move selection / faster / pan"),
    ("R · Del · X", "rename · remove box or group · stop session"),
    ("T · F · + − · C", "tile terminals · fit all · zoom · center"),
    ("/ · B", "filter sidebar · toggle sidebar"),
    ("O", "open this canvas in the other client (GUI / TUI)"),
    ("K · H", "stack / unstack subsessions · hide / show all subsessions"),
    ("Click a stack · double-click", "view its top card · unstack (back to their places)"),
    ("Click an effect (✦ ◉ ✧ ▲)", "clear it (it never starts the session)"),
    ("F6 · Ctrl+]", "cycle focus · leave a terminal"),
    ("Shift+PgUp/PgDn", "terminal history"),
    ("Q / Ctrl+Q", "quit (sessions can keep running)"),
];

fn centered(screen: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(screen.width.saturating_sub(2));
    let h = h.min(screen.height.saturating_sub(2));
    Rect::new(screen.x + (screen.width - w) / 2, screen.y + (screen.height - h) / 2, w, h)
}

fn panel(buf: &mut Buffer, rect: Rect, title: &str) {
    fill(buf, rect.x as i32, rect.y as i32, rect.width as i32, rect.height as i32, Style::default().bg(MODAL), rect);
    outline(buf, rect.x as i32, rect.y as i32, rect.width as i32, rect.height as i32, &ROUND, fg(ACCENT).bg(MODAL), rect);
    if !title.is_empty() {
        put(buf, rect.x as i32 + 2, rect.y as i32, &format!(" {title} "), bold(TEXT).bg(MODAL), rect);
    }
}

struct Ctx<'a> {
    hits: &'a mut Vec<(Rect, Hit)>,
    top: bool,
}

impl Ctx<'_> {
    fn hit(&mut self, rect: Rect, hit: Hit) {
        if self.top {
            self.hits.push((rect, hit));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_button(buf: &mut Buffer, x: i32, y: i32, label: &str, focused: bool, primary: bool, clip: Rect) -> i32 {
    let style = if focused {
        Style::default().fg(ratatui::style::Color::Black).bg(if primary { ACCENT } else { GRAY }).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(if primary { ACCENT } else { TEXT }).bg(PANEL_HI)
    };
    put(buf, x, y, &format!(" {label} "), style, clip)
}

#[allow(clippy::too_many_arguments)]
fn button(buf: &mut Buffer, ctx: &mut Ctx, x: i32, y: i32, label: &str, focused: bool, primary: bool, index: usize, clip: Rect) -> i32 {
    let end = draw_button(buf, x, y, label, focused, primary, clip);
    ctx.hit(Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Button(index));
    end + 2
}

/// A form button that maps to a dialog field rather than a generic button index.
#[allow(clippy::too_many_arguments)]
fn field_button(buf: &mut Buffer, ctx: &mut Ctx, x: i32, y: i32, label: &str, field: NsField, focus: NsField, primary: bool, clip: Rect) -> i32 {
    let end = draw_button(buf, x, y, label, focus == field, primary, clip);
    ctx.hit(Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Field(field));
    end + 2
}

#[allow(clippy::too_many_arguments)]
fn field(buf: &mut Buffer, x: i32, y: i32, width: i32, input: &TextInput, focused: bool, placeholder: &str, clip: Rect) -> Option<(u16, u16)> {
    let style = if focused { Style::default().fg(TEXT).bg(PANEL_HI).add_modifier(Modifier::UNDERLINED) } else { Style::default().fg(TEXT).bg(PANEL_HI) };
    fill(buf, x, y, width, 1, style, clip);
    if input.text.is_empty() && !focused {
        put(buf, x, y, &truncate(placeholder, width as usize), fg(MUTED).bg(PANEL_HI), clip);
        return None;
    }
    let (visible, cursor) = input.view(width.max(1) as usize);
    put(buf, x, y, &visible, style, clip);
    focused.then_some(((x + cursor as i32) as u16, y as u16))
}

pub fn draw_modals(app: &mut App, buf: &mut Buffer, screen: Rect) -> Option<(u16, u16)> {
    let count = app.modals.len();
    let mut cursor = None;
    let mut hits = std::mem::take(&mut app.layout.hits);
    // Main-screen click targets do not apply while a modal is open.
    if count > 0 {
        hits.clear();
    }
    for index in 0..count {
        let top = index + 1 == count;
        let modal = app.modals[index].clone();
        let mut ctx = Ctx { hits: &mut hits, top };
        let (rect, modal_cursor) = draw_modal(app, buf, screen, &modal, &mut ctx);
        if top {
            app.layout.modal_rect = Some(rect);
            cursor = modal_cursor;
        }
    }
    app.layout.hits = hits;
    cursor
}

fn draw_modal(app: &mut App, buf: &mut Buffer, screen: Rect, modal: &Modal, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    match modal {
        Modal::Help => {
            let rect = centered(screen, 78, HELP.len() as u16 + 4);
            panel(buf, rect, "Copilot Canvas — help");
            for (i, (key, text)) in HELP.iter().enumerate() {
                let y = rect.y as i32 + 2 + i as i32;
                if text.is_empty() {
                    put(buf, rect.x as i32 + 2, y, key, bold(ACCENT).bg(MODAL), rect);
                } else {
                    put(buf, rect.x as i32 + 3, y, key, fg(TEXT).bg(MODAL), rect);
                    put(buf, rect.x as i32 + 38, y, text, fg(GRAY).bg(MODAL), rect);
                }
            }
            (rect, None)
        }
        Modal::Menu(menu) => {
            let width = menu.items.iter().map(|i| crate::paint::text_width(&i.label) + crate::paint::text_width(&i.hint) + 6).max().unwrap_or(20).max(24) as u16;
            let height = menu.items.len() as u16 + 2;
            let x = menu.x.min(screen.right().saturating_sub(width));
            let y = menu.y.min(screen.bottom().saturating_sub(height));
            let rect = Rect::new(x, y, width, height);
            panel(buf, rect, &truncate(&menu.title, width as usize - 6));
            for (i, item) in menu.items.iter().enumerate() {
                let row_y = rect.y as i32 + 1 + i as i32;
                let selected = i == menu.sel;
                let bg = if selected { PANEL_HI } else { MODAL };
                fill(buf, rect.x as i32 + 1, row_y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
                let style = if !item.enabled { fg(MUTED).bg(bg) } else if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) };
                put(buf, rect.x as i32 + 2, row_y, &item.label, style, rect);
                put(buf, rect.right() as i32 - 2 - crate::paint::text_width(&item.hint) as i32, row_y, &item.hint, fg(MUTED).bg(bg), rect);
                ctx.hit(Rect::new(rect.x + 1, row_y as u16, rect.width - 2, 1), Hit::MenuItem(i));
            }
            (rect, None)
        }
        Modal::Confirm(confirm) => {
            let width = confirm.body.iter().map(|b| crate::paint::text_width(b)).max().unwrap_or(30).max(confirm.title.len()).max(50) as u16 + 6;
            let rect = centered(screen, width, confirm.body.len() as u16 + 6);
            panel(buf, rect, &confirm.title);
            for (i, line) in confirm.body.iter().enumerate() {
                put(buf, rect.x as i32 + 3, rect.y as i32 + 2 + i as i32, line, fg(TEXT).bg(MODAL), rect);
            }
            let mut x = rect.x as i32 + 3;
            let y = rect.bottom() as i32 - 2;
            for (i, (label, _)) in confirm.buttons.iter().enumerate() {
                x = button(buf, ctx, x, y, label, i == confirm.sel, i == 0, i, rect);
            }
            (rect, None)
        }
        Modal::Rename(rename) => {
            let rect = centered(screen, 60, 7);
            let title = match rename.target {
                crate::dialogs::Target::Node(_) => "Rename (canvas label)",
                crate::dialogs::Target::Group(_) => "Rename group",
            };
            panel(buf, rect, title);
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 2, rect.width as i32 - 6, &rename.input, true, "", rect);
            let y = rect.y as i32 + 4;
            let x = button(buf, ctx, rect.x as i32 + 3, y, "Save", false, true, 0, rect);
            button(buf, ctx, x, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Modal::NewSession(dialog) => draw_new_session(app, buf, screen, dialog, ctx),
        Modal::Mcp(picker) => draw_mcp_picker(app, buf, screen, picker, ctx),
        Modal::Viewer(viewer) => draw_viewer(app, buf, screen, viewer, ctx),
        Modal::Inspector(inspector) => draw_inspector(app, buf, screen, inspector, ctx),
        Modal::Folder(browser) => {
            let rect = centered(screen, 76, screen.height.saturating_sub(6).min(28));
            panel(buf, rect, "Choose a folder");
            let path = browser.dir.as_ref().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|| "This PC".into());
            put(buf, rect.x as i32 + 2, rect.y as i32 + 1, &truncate_left(&path, rect.width as usize - 4), bold(TEXT).bg(MODAL), rect);
            let filter = if browser.filter.is_empty() { "type to filter".to_owned() } else { format!("filter: {}", browser.filter) };
            put(buf, rect.x as i32 + 2, rect.y as i32 + 2, &filter, fg(MUTED).bg(MODAL), rect);
            if let Some(error) = &browser.error {
                put(buf, rect.x as i32 + 2, rect.y as i32 + 3, &truncate(error, rect.width as usize - 4), fg(NEGATIVE).bg(MODAL), rect);
            }
            let needle = browser.filter.to_lowercase();
            let visible: Vec<usize> = (0..browser.entries.len()).filter(|&i| needle.is_empty() || browser.entries[i].to_lowercase().contains(&needle)).collect();
            let rows = rect.height.saturating_sub(8) as usize;
            let sel = browser.sel.min(visible.len().saturating_sub(1));
            let scroll = sel.saturating_sub(rows.saturating_sub(1));
            for (row, &entry) in visible.iter().enumerate().skip(scroll).take(rows) {
                let y = rect.y as i32 + 4 + (row - scroll) as i32;
                let selected = row == sel;
                let bg = if selected { PANEL_HI } else { MODAL };
                fill(buf, rect.x as i32 + 1, y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
                let name = &browser.entries[entry];
                let label = if name == ".." { "↑ ..".to_owned() } else { format!("▸ {name}") };
                put(buf, rect.x as i32 + 2, y, &truncate(&label, rect.width as usize - 4), if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) }, rect);
                ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(entry));
            }
            let y = rect.bottom() as i32 - 3;
            put(buf, rect.x as i32 + 2, y, "Enter/→ open · ←/Backspace up · Ctrl+S select current folder", fg(MUTED).bg(MODAL), rect);
            let x = button(buf, ctx, rect.x as i32 + 2, y + 1, "Select this folder", false, true, 0, rect);
            button(buf, ctx, x, y + 1, "Cancel", false, false, 1, rect);
            (rect, None)
        }
        Modal::Pick(pick) => {
            let rect = centered(screen, 72, screen.height.saturating_sub(8).min(24));
            panel(buf, rect, &pick.title);
            let filter = if pick.filter.is_empty() { "type to filter".to_owned() } else { format!("filter: {}", pick.filter) };
            put(buf, rect.x as i32 + 2, rect.y as i32 + 1, &filter, fg(MUTED).bg(MODAL), rect);
            let visible = pick.visible();
            let rows = rect.height.saturating_sub(4) as usize;
            let sel = pick.sel.min(visible.len().saturating_sub(1));
            let scroll = sel.saturating_sub(rows.saturating_sub(1));
            for (row, &index) in visible.iter().enumerate().skip(scroll).take(rows) {
                let y = rect.y as i32 + 2 + (row - scroll) as i32;
                let selected = row == sel;
                let bg = if selected { PANEL_HI } else { MODAL };
                fill(buf, rect.x as i32 + 1, y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
                let (value, note) = &pick.items[index];
                // The value always shows; a long note gets at most half the row.
                let note_w = (crate::paint::text_width(note) as i32).min(rect.width as i32 / 2 - 2).max(0);
                let note = truncate(note, note_w as usize);
                put(buf, rect.x as i32 + 2, y, &truncate_left(value, (rect.width as i32 - note_w - 6).max(4) as usize), if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) }, rect);
                put(buf, rect.right() as i32 - note_w - 2, y, &note, fg(MUTED).bg(bg), rect);
                ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(index));
            }
            if visible.is_empty() {
                put(buf, rect.x as i32 + 2, rect.y as i32 + 2, "No matches.", fg(MUTED).bg(MODAL), rect);
            }
            (rect, None)
        }
        Modal::ImportCopilot(dialog) => {
            let rect = centered(screen, screen.width.saturating_sub(10).min(110), screen.height.saturating_sub(4).min(34));
            panel(buf, rect, "Import sessions (Copilot, OpenCode, Claude Code)");
            let cursor = field(buf, rect.x as i32 + 2, rect.y as i32 + 1, rect.width as i32 - 4, &dialog.filter, true, "", rect);
            let candidates = app.copilot_import_candidates(&dialog.filter.text);
            let rows = rect.height.saturating_sub(6) as usize;
            let sel = dialog.sel.min(candidates.len().saturating_sub(1));
            let scroll = sel.saturating_sub(rows.saturating_sub(1));
            let now = now_ms();
            for (row, id) in candidates.iter().enumerate().skip(scroll).take(rows) {
                let Some(summary) = app.sessions.get(id) else { continue };
                let y = rect.y as i32 + 3 + (row - scroll) as i32;
                let selected = row == sel;
                let bg = if selected { PANEL_HI } else { MODAL };
                fill(buf, rect.x as i32 + 1, y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
                let check = if dialog.chosen.contains(id) { "[x]" } else { "[ ]" };
                let mut x = put(buf, rect.x as i32 + 2, y, check, fg(if dialog.chosen.contains(id) { POSITIVE } else { MUTED }).bg(bg), rect) + 1;
                if summary.live {
                    x = put(buf, x, y, "LIVE", fg(PURPLE).bg(bg), rect) + 1;
                }
                let client = crate::app::client_label(summary);
                let right = format!("{client} · {} ago", format_age(summary.updated_ms, now));
                let right_w = crate::paint::text_width(&right) as i32;
                let folder = summary.cwd.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or_default();
                let label = format!("{}  — {folder}", summary.name);
                put(buf, x, y, &truncate(&label, (rect.right() as i32 - right_w - 3 - x).max(4) as usize), if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) }, rect);
                put(buf, rect.right() as i32 - right_w - 2, y, &right, fg(MUTED).bg(bg), rect);
                ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(row));
            }
            if candidates.is_empty() {
                let text = if app.sessions_loaded { "No sessions match (sessions already on the canvas are hidden)." } else { "Loading sessions…" };
                put(buf, rect.x as i32 + 2, rect.y as i32 + 3, text, fg(MUTED).bg(MODAL), rect);
            }
            let y = rect.bottom() as i32 - 2;
            let summary = format!("{} shown · {} chosen · Space choose · Ctrl+A all · ", candidates.len(), dialog.chosen.len());
            let x = put(buf, rect.x as i32 + 2, y, &summary, fg(MUTED).bg(MODAL), rect);
            let toggle = format!("[Tab] {}", dialog.grouping.label());
            let end = put(buf, x, y, &toggle, fg(ACCENT).bg(MODAL), rect);
            ctx.hit(Rect::new(x as u16, y as u16, (end - x) as u16, 1), Hit::Toggle("grouping"));
            button(buf, ctx, rect.right() as i32 - 12, y, "Import", false, true, 0, rect);
            (rect, cursor)
        }
        Modal::Prompt(prompt) => {
            let rect = centered(screen, 92, 8);
            // The group may have been renamed after this prompt was queued.
            let title = match &prompt.purpose {
                crate::dialogs::PromptPurpose::GroupFolder(id) => match app.doc.group(id) {
                    Some(group) => format!("Folder for “{}” (ai-docs + human-outputs)", group.name),
                    None => prompt.title.clone(),
                },
                _ => prompt.title.clone(),
            };
            panel(buf, rect, &truncate(&title, 80));
            let cursor = field(buf, rect.x as i32 + 3, rect.y as i32 + 2, rect.width as i32 - 6, &prompt.input, true, "", rect);
            let hint = match &prompt.purpose {
                crate::dialogs::PromptPurpose::GroupFolder(id) if prompt.input.text.trim().is_empty() => {
                    format!("Enter to use: {}   ·   Tab: browse   ·   Esc: later", app.suggested_group_folder(id))
                }
                _ => prompt.hint.clone(),
            };
            put(buf, rect.x as i32 + 3, rect.y as i32 + 3, &truncate_left(&hint, rect.width as usize - 6), fg(MUTED).bg(MODAL), rect);
            let y = rect.y as i32 + 5;
            let x = button(buf, ctx, rect.x as i32 + 3, y, "OK", false, true, 0, rect);
            button(buf, ctx, x, y, "Cancel", false, false, 1, rect);
            (rect, cursor)
        }
        Modal::Migrate(dialog) => {
            let rect = centered(screen, screen.width.saturating_sub(8).min(120), screen.height.saturating_sub(4).min(36));
            panel(buf, rect, "Migrate VS Code skills & settings into Copilot CLI");
            let inner_w = rect.width as usize - 4;
            if let Some(result) = &dialog.result {
                for (i, line) in result.iter().enumerate().take(rect.height as usize - 5) {
                    let color = if line.starts_with('✓') { POSITIVE } else if line.starts_with('✗') { NEGATIVE } else { MUTED };
                    put(buf, rect.x as i32 + 2, rect.y as i32 + 2 + i as i32, &truncate(line, inner_w), fg(color).bg(MODAL), rect);
                }
                button(buf, ctx, rect.right() as i32 - 10, rect.bottom() as i32 - 2, "Close", true, true, 0, rect);
                return (rect, None);
            }
            let Some(items) = &dialog.items else {
                put(buf, rect.x as i32 + 2, rect.y as i32 + 2, "Reading VS Code (read-only) and your Copilot configuration…", fg(TEXT).bg(MODAL), rect);
                return (rect, None);
            };
            put(buf, rect.x as i32 + 2, rect.y as i32 + 1, "New items are preselected. Existing Copilot config is backed up first and never overwritten.", fg(MUTED).bg(MODAL), rect);
            let rows = rect.height.saturating_sub(6) as usize;
            let sel = dialog.sel.min(items.len().saturating_sub(1));
            let scroll = sel.saturating_sub(rows.saturating_sub(1));
            for (row, item) in items.iter().enumerate().skip(scroll).take(rows) {
                let y = rect.y as i32 + 3 + (row - scroll) as i32;
                let selected = row == sel;
                let bg = if selected { PANEL_HI } else { MODAL };
                fill(buf, rect.x as i32 + 1, y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
                let (check, color, note) = match &item.status {
                    crate::migrate::Status::New => (if dialog.chosen.get(row).copied().unwrap_or(false) { "[x]" } else { "[ ]" }, POSITIVE, "new".to_owned()),
                    crate::migrate::Status::AlreadyPresent => ("[=]", MUTED, "already in Copilot".to_owned()),
                    crate::migrate::Status::Conflict(reason) => ("[!]", ATTENTION, reason.clone()),
                    crate::migrate::Status::Manual(reason) => ("[?]", ATTENTION, reason.clone()),
                };
                let mut x = put(buf, rect.x as i32 + 2, y, check, fg(color).bg(bg), rect) + 1;
                x = put(buf, x, y, &format!("{:<15}", item.kind.label()), fg(MUTED).bg(bg), rect);
                let label = format!("{}  ({})", item.name, item.products);
                let note_w = (crate::paint::text_width(&note) as i32).min(rect.width as i32 / 2);
                put(buf, x, y, &truncate(&label, (rect.right() as i32 - note_w - 4 - x).max(4) as usize), if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) }, rect);
                put(buf, rect.right() as i32 - note_w - 2, y, &truncate(&note, note_w as usize), fg(color).bg(bg), rect);
                ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(row));
            }
            if items.is_empty() {
                put(buf, rect.x as i32 + 2, rect.y as i32 + 3, "Nothing to migrate was found in VS Code.", fg(MUTED).bg(MODAL), rect);
            }
            let chosen = items.iter().zip(&dialog.chosen).filter(|(i, c)| **c && i.status == crate::migrate::Status::New).count();
            let y = rect.bottom() as i32 - 2;
            let footer = if dialog.applying { "Applying…".to_owned() } else { format!("{chosen} selected · Space toggle · Enter apply · Esc cancel") };
            put(buf, rect.x as i32 + 2, y, &footer, fg(MUTED).bg(MODAL), rect);
            button(buf, ctx, rect.right() as i32 - 18, y, "Apply selected", false, true, 0, rect);
            (rect, None)
        }
    }
}

/// The read-only viewer: path, a status line, numbered lines, buttons.
fn draw_viewer(app: &mut App, buf: &mut Buffer, screen: Rect, viewer: &crate::viewer::Viewer, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rect = centered(screen, screen.width.saturating_sub(6).min(160), screen.height.saturating_sub(2));
    let title = if viewer.folder { "Folder on the service's machine" } else { "File on the service's machine (read-only)" };
    panel(buf, rect, title);
    let (x, width) = (rect.x as i32 + 2, rect.width as i32 - 4);
    put(buf, x, rect.y as i32 + 1, &truncate_left(&viewer.path, width.max(1) as usize), bold(TEXT).bg(MODAL), rect);
    let rows = rect.height.saturating_sub(6) as usize;
    let lines = viewer.lines.as_deref().unwrap_or_default();
    let status = match (&viewer.lines, &viewer.error) {
        (_, Some(error)) => error.clone(),
        (None, None) => "Loading…".into(),
        (Some(_), None) => {
            let last = (viewer.top + rows).min(lines.len());
            let range = if lines.is_empty() { "empty".into() } else { format!("lines {}–{last} of {}", viewer.top + 1, lines.len()) };
            let keys = if viewer.browsing() { "↑↓ pick · Enter open · Backspace up · Tab buttons · Esc close" } else { "↑↓ PgUp/PgDn scroll · ←→ sideways · Tab buttons · Esc close" };
            format!("{range}{}   ·   {keys}", viewer.note.as_deref().map(|n| format!(" · {n}")).unwrap_or_default())
        }
    };
    let status_style = if viewer.error.is_some() { fg(NEGATIVE).bg(MODAL) } else { fg(MUTED).bg(MODAL) };
    put(buf, x, rect.y as i32 + 2, &truncate(&status, width.max(1) as usize), status_style, rect);
    let gutter = if viewer.folder { 0 } else { lines.len().max(1).to_string().len() + 1 };
    for (row, line) in lines.iter().enumerate().skip(viewer.top).take(rows) {
        let y = rect.y as i32 + 3 + (row - viewer.top) as i32;
        if gutter > 0 {
            put(buf, x, y, &format!("{:>w$}", row + 1, w = gutter - 1), fg(MUTED).bg(MODAL), rect);
        }
        let visible: String = line.chars().skip(viewer.left).collect();
        let text_x = x + gutter as i32 + 1;
        // The folder browser: entries are picked and opened.
        let picked = viewer.browsing() && row == viewer.pick && row < viewer.entries.len();
        let style = if picked && !viewer.on_buttons { bold(SELECT).bg(PANEL_HI) } else { fg(TEXT).bg(MODAL) };
        if picked && !viewer.on_buttons {
            fill(buf, text_x - 1, y, width - gutter as i32, 1, Style::default().bg(PANEL_HI), rect);
            put(buf, text_x - 1, y, "›", bold(ACCENT).bg(PANEL_HI), rect);
        }
        put(buf, text_x, y, &truncate(&visible, (width - gutter as i32 - 1).max(1) as usize), style, rect);
        if viewer.browsing() && row < viewer.entries.len() {
            ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(row));
        }
    }
    let mut bx = x;
    let y = rect.bottom() as i32 - 2;
    for (i, label) in viewer.buttons().iter().enumerate() {
        let focused = i == viewer.sel && (!viewer.browsing() || viewer.on_buttons);
        bx = button(buf, ctx, bx, y, label, focused, i == 0, i, rect);
    }
    if ctx.top
        && let Some(Modal::Viewer(top)) = app.modals.last_mut()
    {
        top.rows = rows.max(1);
    }
    (rect, None)
}

/// The transcript inspector: what may resume, the status, the transcript
/// (newest at the bottom while it follows), buttons.
fn draw_inspector(app: &mut App, buf: &mut Buffer, screen: Rect, inspector: &crate::inspect::Inspector, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    use crate::inspect::Line;
    let rect = centered(screen, screen.width.saturating_sub(6).min(150), screen.height.saturating_sub(2));
    panel(buf, rect, &format!("Transcript · {} (read-only)", truncate(&inspector.title, 70)));
    let (x, width) = (rect.x as i32 + 2, rect.width as i32 - 4);
    let note = app.inspector_note(inspector);
    put(buf, x, rect.y as i32 + 1, &truncate(&note, width.max(1) as usize), fg(PURPLE).bg(MODAL), rect);
    let rows = rect.height.saturating_sub(6) as usize;
    let lines = inspector.transcript.as_ref().map(|t| crate::inspect::lines(t, width.max(16) as usize)).unwrap_or_default();
    let max_top = lines.len().saturating_sub(rows);
    let top = if inspector.follow { max_top } else { inspector.top.min(max_top) };
    let status = match (&inspector.transcript, &inspector.error) {
        (_, Some(error)) => error.clone(),
        (None, None) => "Loading…".into(),
        (Some(t), None) => {
            let range = if lines.is_empty() { "empty".to_owned() } else { format!("lines {}–{} of {}", top + 1, (top + rows).min(lines.len()), lines.len()) };
            let older = if t.truncated { " (the newest part)" } else { "" };
            let live = if t.running { " · running: refreshing" } else { "" };
            format!("{} entries{older}{live} · {range}   ·   ↑↓ PgUp/PgDn scroll · End newest · Tab buttons · Esc close", t.entries.len())
        }
    };
    let status_style = if inspector.error.is_some() { fg(NEGATIVE).bg(MODAL) } else { fg(MUTED).bg(MODAL) };
    put(buf, x, rect.y as i32 + 2, &truncate(&status, width.max(1) as usize), status_style, rect);
    for (row, (text, kind)) in lines.iter().enumerate().skip(top).take(rows) {
        let style = match kind {
            Line::User => bold(ACCENT).bg(MODAL),
            Line::Assistant => fg(TEXT).bg(MODAL),
            Line::Tool => fg(GRAY).bg(MODAL),
            Line::Result | Line::Blank => fg(MUTED).bg(MODAL),
            Line::Failed => fg(NEGATIVE).bg(MODAL),
            Line::Note => fg(PURPLE).bg(MODAL),
        };
        put(buf, x, rect.y as i32 + 3 + (row - top) as i32, &truncate(text, width.max(1) as usize), style, rect);
    }
    let mut bx = x;
    let y = rect.bottom() as i32 - 2;
    let buttons = app.inspector_buttons(inspector);
    // The first button is the primary one when it resumes (Copy and Close follow it).
    for (i, (label, _)) in buttons.iter().enumerate() {
        bx = button(buf, ctx, bx, y, label, i == inspector.sel, i == 0 && buttons.len() > 2, i, rect);
    }
    if ctx.top
        && let Some(Modal::Inspector(open)) = app.modals.last_mut()
    {
        open.rows = rows.max(1);
        open.total = lines.len();
        open.top = top;
    }
    (rect, None)
}

fn draw_mcp_picker(app: &App, buf: &mut Buffer, screen: Rect, picker: &crate::dialogs::McpPicker, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let choices = app.mcp_choices(picker.agent);
    let count = choices.as_ref().map(|c| c.len()).unwrap_or(0) as u16;
    let rect = centered(screen, 80, (count + 9).clamp(12, screen.height.saturating_sub(4)));
    let title = match &picker.target {
        crate::dialogs::McpTarget::NewSession => format!("{} MCP servers for the new session", picker.agent.label()),
        crate::dialogs::McpTarget::Node(id) => format!("MCP servers for “{}”", truncate(&app.doc.node(id).map(|n| n.name.clone()).unwrap_or_default(), 40)),
    };
    panel(buf, rect, &title);
    let (left, inner_w) = (rect.x as i32 + 2, rect.width as i32 - 4);
    // Preset chips: the one matching the current choice is highlighted.
    let known = app.mcp_available(picker.agent);
    let current = app.doc.mcp.matching(&picker.off, &known);
    let mut x = put(buf, left, rect.y as i32 + 1, "Preset ", fg(GRAY).bg(MODAL), rect);
    for (i, name) in app.doc.mcp.names().iter().enumerate() {
        let on = current.as_deref() == Some(name.as_str());
        let label = format!(" {name} ");
        let style = if on { bold(MODAL).bg(ACCENT) } else { fg(TEXT).bg(PANEL_HI) };
        let end = put(buf, x, rect.y as i32 + 1, &label, style, rect);
        ctx.hit(Rect::new(x as u16, rect.y + 1, (end - x).max(0) as u16, 1), Hit::Chip(i));
        x = end + 1;
    }
    let Some(choices) = choices else {
        put(buf, left, rect.y as i32 + 3, &format!("Loading {}'s MCP servers…", picker.agent.label()), fg(MUTED).bg(MODAL), rect);
        return (rect, None);
    };
    let on = known.iter().filter(|k| !picker.off.contains(k)).count();
    let note = format!("{on} of {} on · each server runs as its own processes in every session", known.len());
    put(buf, left, rect.y as i32 + 2, &truncate(&note, inner_w as usize), fg(MUTED).bg(MODAL), rect);
    let rows = rect.height.saturating_sub(7) as usize;
    let sel = picker.sel.min(choices.len().saturating_sub(1));
    let scroll = sel.saturating_sub(rows.saturating_sub(1));
    for (row, (name, source, enabled)) in choices.iter().enumerate().skip(scroll).take(rows) {
        let y = rect.y as i32 + 3 + (row - scroll) as i32;
        let selected = row == sel;
        let bg = if selected { PANEL_HI } else { MODAL };
        fill(buf, rect.x as i32 + 1, y, rect.width as i32 - 2, 1, Style::default().bg(bg), rect);
        let (check, color) = if !enabled {
            ("[-]", MUTED)
        } else if picker.off.contains(name) {
            ("[ ]", MUTED)
        } else {
            ("[x]", POSITIVE)
        };
        let x = put(buf, left, y, check, fg(color).bg(bg), rect) + 1;
        let note = if *enabled { source.clone() } else { format!("off in {}'s config", picker.agent.label()) };
        let note_w = (crate::paint::text_width(&note) as i32).min(inner_w / 2);
        let name_style = if !enabled { fg(MUTED).bg(bg) } else if selected { bold(SELECT).bg(bg) } else { fg(TEXT).bg(bg) };
        put(buf, x, y, &truncate(name, (rect.right() as i32 - note_w - 4 - x).max(4) as usize), name_style, rect);
        put(buf, rect.right() as i32 - note_w - 2, y, &truncate(&note, note_w as usize), fg(MUTED).bg(bg), rect);
        ctx.hit(Rect::new(rect.x + 1, y as u16, rect.width - 2, 1), Hit::ListRow(row));
    }
    if choices.is_empty() {
        put(buf, left, rect.y as i32 + 3, &format!("No MCP servers are configured in {}.", picker.agent.label()), fg(MUTED).bg(MODAL), rect);
    }
    let y = rect.bottom() as i32 - 3;
    let help = "Space toggle · A all · N none · ←/→ preset · Ctrl+S save preset · Del delete preset · Enter done · Esc cancel";
    put(buf, left, y, &truncate(help, inner_w as usize), fg(MUTED).bg(MODAL), rect);
    let x = button(buf, ctx, left, y + 1, "Done", false, true, 0, rect);
    let x = button(buf, ctx, x, y + 1, "Save as preset…", false, false, 1, rect);
    button(buf, ctx, x, y + 1, "Cancel", false, false, 2, rect);
    (rect, None)
}

fn draw_new_session(app: &App, buf: &mut Buffer, screen: Rect, dialog: &crate::dialogs::NewSession, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let repo = matches!(dialog.git, GitState::Repo(_));
    let height = 20 + if repo { 2 } else { 0 } + if repo && dialog.worktree { 4 } else { 0 } + if dialog.can_fork { 2 } else { 0 };
    let rect = centered(screen, 84, height);
    let title = match dialog.parent.as_ref().and_then(|p| app.doc.node(p)) {
        Some(parent) => format!("New subsession of “{}”", truncate(&parent.name, 40)),
        None => "New session".into(),
    };
    panel(buf, rect, &title);
    outline(buf, rect.x as i32, rect.y as i32, rect.width as i32, rect.height as i32, &DOUBLE, fg(ACCENT).bg(MODAL), rect);
    put(buf, rect.x as i32 + 2, rect.y as i32, &format!(" {title} "), bold(TEXT).bg(MODAL), rect);
    let label_x = rect.x as i32 + 3;
    let field_x = rect.x as i32 + 18;
    let field_w = rect.width as i32 - 21;
    let mut y = rect.y as i32 + 2;
    let mut cursor = None;
    let label = |buf: &mut Buffer, y: i32, text: &str, focused: bool| {
        put(buf, label_x, y, text, if focused { bold(ACCENT).bg(MODAL) } else { fg(GRAY).bg(MODAL) }, rect);
    };
    let focus = dialog.focus;

    label(buf, y, "Name", focus == NsField::Name);
    cursor = field(buf, field_x, y, field_w, &dialog.name, focus == NsField::Name, "defaults to the folder name", rect).or(cursor);
    ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::Name));
    y += 2;

    label(buf, y, "Folder", focus == NsField::Folder);
    let folder_w = field_w - 24;
    cursor = field(buf, field_x, y, folder_w, &dialog.folder, focus == NsField::Folder, "", rect).or(cursor);
    ctx.hit(Rect::new(field_x as u16, y as u16, folder_w as u16, 1), Hit::Field(NsField::Folder));
    let bx = field_x + folder_w + 1;
    let bx_end = field_button(buf, ctx, bx, y, "Browse…", NsField::Browse, focus, false, rect);
    field_button(buf, ctx, bx_end, y, "Recent ▾", NsField::Recent, focus, false, rect);
    y += 1;
    let (git_text, git_style) = match &dialog.git {
        GitState::Unknown => ("".to_owned(), fg(MUTED)),
        GitState::Checking(_) => ("Checking Git…".to_owned(), fg(MUTED)),
        GitState::NotRepo => ("Not a Git repository".to_owned(), fg(MUTED)),
        GitState::Error(error) => (format!("⚠ {error}"), fg(ATTENTION)),
        GitState::Repo(info) => (
            format!("Git: {} on {}", truncate_left(&info.root.to_string_lossy(), 40), info.branch.as_deref().unwrap_or("detached HEAD")),
            fg(POSITIVE),
        ),
    };
    put(buf, field_x, y, &truncate(&git_text, field_w as usize), git_style.bg(MODAL), rect);
    y += 2;

    if let GitState::Repo(_) = &dialog.git {
        label(buf, y, "Workspace", focus == NsField::Mode);
        let option = |on: bool, text: &str| format!("{} {text}", if on { "◉" } else { "○" });
        let style = if focus == NsField::Mode { bold(SELECT).bg(MODAL) } else { fg(TEXT).bg(MODAL) };
        let end = put(buf, field_x, y, &option(!dialog.worktree, "Use this folder"), style, rect);
        let end = put(buf, end + 3, y, &option(dialog.worktree, "New Git worktree"), style, rect);
        ctx.hit(Rect::new(field_x as u16, y as u16, (end - field_x) as u16, 1), Hit::Field(NsField::Mode));
        y += 2;
        if dialog.worktree {
            label(buf, y, "Base branch", focus == NsField::Base);
            let style = if focus == NsField::Base { Style::default().fg(TEXT).bg(PANEL_HI).add_modifier(Modifier::UNDERLINED) } else { Style::default().fg(TEXT).bg(PANEL_HI) };
            fill(buf, field_x, y, field_w, 1, style, rect);
            let base = if dialog.base.is_empty() { "choose…".to_owned() } else { format!("{}  ▾", dialog.base) };
            put(buf, field_x, y, &truncate(&base, field_w as usize), style, rect);
            ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::Base));
            y += 1;
            label(buf, y, "New branch", focus == NsField::NewBranch);
            cursor = field(buf, field_x, y, field_w, &dialog.new_branch, focus == NsField::NewBranch, "empty: check out the base branch itself", rect).or(cursor);
            ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::NewBranch));
            y += 1;
            label(buf, y, "Worktree path", focus == NsField::WtPath);
            cursor = field(buf, field_x, y, field_w, &dialog.wt_path, focus == NsField::WtPath, "", rect).or(cursor);
            ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::WtPath));
            y += 2;
        }
    }

    label(buf, y, "Agent", focus == NsField::Agent);
    {
        let style = if focus == NsField::Agent { bold(SELECT).bg(MODAL) } else { fg(TEXT).bg(MODAL) };
        let mut x = field_x;
        for kind in crate::agents::ALL {
            let on = kind == dialog.agent;
            let missing = !app.demo && !app.installed_agents.contains(&kind);
            let text = format!("{} {}{}", if on { "◉" } else { "○" }, kind.label(), if missing { " (not installed)" } else { "" });
            x = put(buf, x, y, &text, if missing && !on { fg(MUTED).bg(MODAL) } else { style }, rect) + 3;
        }
        ctx.hit(Rect::new(field_x as u16, y as u16, (x - field_x).max(1) as u16, 1), Hit::Field(NsField::Agent));
        y += 2;
    }

    if dialog.can_fork {
        label(buf, y, "Context", focus == NsField::Context);
        let option = |on: bool, text: &str| format!("{} {text}", if on { "◉" } else { "○" });
        let style = if focus == NsField::Context { bold(SELECT).bg(MODAL) } else { fg(TEXT).bg(MODAL) };
        let end = put(buf, field_x, y, &option(dialog.fork, "Fork parent's conversation"), style, rect);
        let end = put(buf, end + 3, y, &option(!dialog.fork, "Start fresh"), style, rect);
        ctx.hit(Rect::new(field_x as u16, y as u16, (end - field_x) as u16, 1), Hit::Field(NsField::Context));
        y += 2;
    }

    label(buf, y, "MCP servers", focus == NsField::Mcp);
    let style = if focus == NsField::Mcp { Style::default().fg(TEXT).bg(PANEL_HI).add_modifier(Modifier::UNDERLINED) } else { Style::default().fg(TEXT).bg(PANEL_HI) };
    fill(buf, field_x, y, field_w, 1, style, rect);
    let summary = format!("{}  ▾ choose", app.mcp_summary(dialog.agent, &dialog.mcp_off));
    put(buf, field_x, y, &truncate(&summary, field_w as usize), style, rect);
    ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::Mcp));
    y += 2;

    label(buf, y, "First prompt", focus == NsField::Prompt);
    cursor = field(buf, field_x, y, field_w, &dialog.prompt, focus == NsField::Prompt, "optional — sent when the session starts", rect).or(cursor);
    ctx.hit(Rect::new(field_x as u16, y as u16, field_w as u16, 1), Hit::Field(NsField::Prompt));
    y += 2;

    if let Some(error) = &dialog.error {
        put(buf, label_x, y, &truncate(error, rect.width as usize - 6), fg(NEGATIVE).bg(MODAL), rect);
    } else {
        let hint = format!("Tab/↑↓ move · Space/←→ agent · Enter create · Esc cancel · {} permissions per Settings", dialog.agent.label());
        put(buf, label_x, y, &truncate(&hint, rect.width as usize - 6), fg(MUTED).bg(MODAL), rect);
    }
    let y = rect.bottom() as i32 - 2;
    let create = if dialog.worktree && repo { "Create worktree & start" } else { "Create & start" };
    let x = field_button(buf, ctx, label_x, y, create, NsField::Create, focus, true, rect);
    field_button(buf, ctx, x, y, "Cancel", NsField::Cancel, focus, false, rect);
    (rect, cursor)
}
