//! The skill folders editor (the group menu's *Skill folders…*, its details'
//! SKILLS section, *Settings › Skill folders (all sessions)…*): the folders
//! of skills every session of a group gets, or every session of the canvas,
//! each with how many skills it holds. A folder is typed (Tab: the folder
//! browser) and checked on the service's machine, in the background, before
//! it is added: it exists and holds a SKILL.md. Removing one only takes it
//! off the list.

use canvas_model::skills::{FolderSet, MAX_FOLDERS, SourceKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::app::{App, AppEvent};
use crate::dialogs::{Action, Hit, Modal, TextInput};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseEvent, MouseEventKind};
use crate::modals::Outcome;
use crate::paint::{ACCENT, GRAY, MODAL, MUTED, NEGATIVE, PANEL_HI, TEXT, bold, fg, fill, put, truncate, truncate_left};
use crate::rpc::Backend;
use crate::skills::SkillAction;
use crate::ui_modals::{Ctx, button, centered, field, panel};

/// The editor (`Modal::SkillFolders`).
#[derive(Clone, Debug, PartialEq)]
pub struct Dialog {
    /// Whose folders: a group's (its id), else every session's.
    pub group: Option<String>,
    pub input: TextInput,
    pub sel: usize,
    /// Skills found in folders checked here: (folder, count).
    pub counts: Vec<(String, usize)>,
    pub error: Option<String>,
    /// The folder the service is checking (one at a time; the UI never waits).
    pub checking: Option<String>,
}

/// Opens the editor of a group's folders (`None`: every session's).
pub fn open(app: &mut App, group: Option<String>) {
    if let Some(id) = &group
        && app.doc.group(id).is_none()
    {
        return app.notify("That group is no longer on the canvas.", true);
    }
    app.modals.push(Modal::SkillFolders(Box::new(Dialog { group, input: TextInput::default(), sel: 0, counts: Vec::new(), error: None, checking: None })));
    // The counts come with the next listing.
    if !app.skills.loading {
        app.refresh_skill_sources();
    }
    app.needs_draw = true;
}

/// The folders of a group (`None`: every session's).
pub fn dirs(app: &App, group: Option<&str>) -> Vec<String> {
    match group {
        Some(id) => app.doc.group(id).and_then(|g| g.defaults.as_ref()).map(|d| d.skill_dirs.clone()).unwrap_or_default(),
        None => app.doc.defaults.skill_dirs.clone(),
    }
}

/// Sets a group's folders (`None`: every session's); a group whose
/// overrides are left empty has none.
pub fn set_dirs(app: &mut App, group: Option<&str>, list: Vec<String>) {
    match group {
        Some(id) => {
            let Some(group) = app.doc.group_mut(id) else { return };
            let mut defaults = group.defaults.clone().unwrap_or_default();
            defaults.skill_dirs = list;
            group.defaults = (!defaults.is_empty()).then_some(defaults);
        }
        None => app.doc.defaults.skill_dirs = list,
    }
    app.mark_dirty();
    if app.side_view == crate::side::SideView::Skills {
        app.refresh_skill_sources();
    }
}

/// A folder to add, checked on the service's machine (a backend call: run
/// it off the UI thread): its full path and how many skills it holds (at
/// least one). Errors start with what is wrong (the path may be cut off).
pub fn check(backend: &dyn Backend, text: &str) -> Result<(String, usize), String> {
    let text = text.trim().trim_matches('"').trim();
    if text.is_empty() {
        return Err("enter a folder".into());
    }
    let info = backend.path_info(text).map_err(|e| format!("{e:#}"))?;
    if !info.exists {
        return Err(format!("does not exist on the service's machine: {text}"));
    }
    if !info.dir {
        return Err(format!("not a folder: {text}"));
    }
    let set = FolderSet { group: "check".into(), name: String::new(), dirs: vec![info.absolute.clone()] };
    let sources = backend.skill_sources(Vec::new(), None, vec![set]).map_err(|e| {
        let text = format!("{e:#}");
        if text.contains("does not support") { "the service is older than skill folders: update it".to_owned() } else { text }
    })?;
    let count = sources.iter().find(|s| matches!(&s.kind, SourceKind::Group { id, .. } if id == "check")).map(|s| s.skills.len()).unwrap_or(0);
    if count == 0 {
        return Err(format!("no SKILL.md in it or the folders below it: {}", info.absolute));
    }
    Ok((info.absolute, count))
}

/// Skills in a folder: as checked here, else as the last listing found them.
fn count(app: &App, dialog: &Dialog, folder: &str) -> Option<usize> {
    let same = |p: &str| canvas_model::doc::same_dir(p, folder);
    if let Some((_, n)) = dialog.counts.iter().find(|(p, _)| same(p)) {
        return Some(*n);
    }
    let sources = app.skills.sources.as_ref().and_then(|s| s.as_ref().ok())?;
    sources.iter().filter(|s| matches!(s.kind, SourceKind::Group { .. })).flat_map(|s| s.folders.iter()).find(|(p, _)| same(p)).map(|(_, n)| *n)
}

/// The list's lines: each folder with its skills.
pub fn lines(app: &App, dialog: &Dialog) -> Vec<String> {
    dirs(app, dialog.group.as_deref())
        .iter()
        .map(|d| match count(app, dialog, d) {
            Some(n) => format!("{d}  · {n} skill{}", if n == 1 { "" } else { "s" }),
            None => format!("{d}  · …"),
        })
        .collect()
}

/// Enter or *Add*: checks the typed folder on the service's machine in the
/// background ([`checked`] adds it then).
fn add(app: &mut App, dialog: &mut Dialog) {
    if dialog.checking.is_some() {
        return;
    }
    let list = dirs(app, dialog.group.as_deref());
    let typed = dialog.input.text.trim().trim_matches('"').trim().to_owned();
    if typed.is_empty() {
        dialog.error = Some("enter a folder".into());
        return;
    }
    if list.iter().any(|d| canvas_model::doc::same_dir(d, &typed)) {
        dialog.error = Some("already in the list".into());
        return;
    }
    if list.len() >= MAX_FOLDERS {
        dialog.error = Some(format!("at most {MAX_FOLDERS} folders here: remove one first"));
        return;
    }
    dialog.checking = Some(typed.clone());
    dialog.error = None;
    let group = dialog.group.clone();
    app.in_background(
        {
            let typed = typed.clone();
            move |backend| check(backend, &typed)
        },
        move |result| AppEvent::Run(Box::new(move |app: &mut App| checked(app, group, typed, result))),
    );
}

/// A check came back: the folder is added (unless the editor was closed or
/// the list has it by now), else the editor says why not.
fn checked(app: &mut App, group: Option<String>, typed: String, result: Result<(String, usize), String>) {
    let mine = |m: &Modal| matches!(m, Modal::SkillFolders(d) if d.group == group && d.checking.as_deref() == Some(typed.as_str()));
    let Some(at) = app.modals.iter().position(mine) else { return };
    let mut list = dirs(app, group.as_deref());
    let outcome = result.and_then(|(path, n)| {
        if group.as_deref().is_some_and(|g| app.doc.group(g).is_none()) {
            Err("that group is no longer on the canvas".to_owned())
        } else if list.iter().any(|d| canvas_model::doc::same_dir(d, &path)) {
            Err("already in the list".to_owned())
        } else if group.is_some() && app.doc.defaults.skill_dirs.iter().any(|d| canvas_model::doc::same_dir(d, &path)) {
            Err("every session already gets it (Settings › Skill folders)".to_owned())
        } else {
            Ok((path, n))
        }
    });
    if let Ok((path, _)) = &outcome {
        list.push(path.clone());
        set_dirs(app, group.as_deref(), list.clone());
    }
    app.needs_draw = true;
    let Some(Modal::SkillFolders(dialog)) = app.modals.get_mut(at) else { return };
    dialog.checking = None;
    match outcome {
        Ok((path, n)) => {
            dialog.counts.push((path, n));
            dialog.sel = list.len() - 1;
            if dialog.input.text.trim().trim_matches('"').trim() == typed {
                dialog.input.set("");
            }
            dialog.error = None;
        }
        Err(why) => dialog.error = Some(why),
    }
}

fn remove(app: &mut App, dialog: &mut Dialog) {
    let mut list = dirs(app, dialog.group.as_deref());
    if dialog.sel < list.len() {
        list.remove(dialog.sel);
        set_dirs(app, dialog.group.as_deref(), list.clone());
    }
    dialog.sel = dialog.sel.min(list.len().saturating_sub(1));
}

/// The editor's title and what it is about.
fn title(app: &App, dialog: &Dialog) -> (String, String) {
    match dialog.group.as_deref().and_then(|g| app.doc.group(g)) {
        Some(group) => (
            format!("Skill folders · “{}”", group.name),
            "Every session of this group gets their skills (all agents), with every session's.".into(),
        ),
        None => ("Skill folders · every session".into(), "Every session this canvas starts gets their skills (all agents); groups add their own.".into()),
    }
}

/// Draws the editor; its rectangle and caret.
pub fn draw(app: &mut App, buf: &mut Buffer, screen: Rect, dialog: &Dialog, ctx: &mut Ctx) -> (Rect, Option<(u16, u16)>) {
    let rect = centered(screen, 86, 19);
    let (title, about) = title(app, dialog);
    panel(buf, rect, &title);
    let (x, width) = (rect.x as i32 + 3, rect.width as i32 - 6);
    let text = |buf: &mut Buffer, y: i32, line: &str, color: Color| put(buf, x, y, &truncate(line, width.max(0) as usize), fg(color).bg(MODAL), rect);
    text(buf, rect.y as i32 + 2, &about, MUTED);
    let items = lines(app, dialog);
    let rows = 8usize;
    let top = dialog.sel.saturating_sub(rows - 1);
    for (i, item) in items.iter().enumerate().skip(top).take(rows) {
        let y = rect.y as i32 + 3 + (i - top) as i32;
        let style = if i == dialog.sel { bold(Color::Black).bg(ACCENT) } else { fg(TEXT).bg(PANEL_HI) };
        fill(buf, x, y, width, 1, style, rect);
        put(buf, x + 1, y, &truncate_left(item, (width as usize).saturating_sub(2)), style, rect);
        ctx.hit(Rect::new(x as u16, y as u16, width.max(0) as u16, 1), Hit::ListRow(i));
    }
    if items.is_empty() {
        text(buf, rect.y as i32 + 3, "(none yet)", MUTED);
    }
    text(buf, rect.y as i32 + 12, "Add a folder (Enter adds · Tab browses · Del removes the selected one):", GRAY);
    let cursor = field(buf, x, rect.y as i32 + 13, width, &dialog.input, true, "", rect);
    if let Some(error) = &dialog.error {
        text(buf, rect.y as i32 + 14, error, NEGATIVE);
    } else if let Some(folder) = &dialog.checking {
        text(buf, rect.y as i32 + 14, &format!("Checking on the service's machine… {folder}"), GRAY);
    } else {
        text(buf, rect.y as i32 + 14, "A folder of skills (<name>/SKILL.md), or one skill (a folder with SKILL.md).", MUTED);
    }
    let y = rect.bottom() as i32 - 2;
    let mut bx = button(buf, ctx, x, y, "Add", false, true, 0, rect);
    bx = button(buf, ctx, bx, y, "Remove", false, false, 1, rect);
    button(buf, ctx, bx, y, "Close", false, false, 2, rect);
    (rect, cursor)
}

/// Where Tab's folder browser starts.
fn browse_from(app: &App, dialog: &Dialog) -> String {
    let typed = dialog.input.text.trim().trim_matches('"');
    if !typed.is_empty() {
        return typed.to_owned();
    }
    if app.info.user_home.is_empty() { app.info.home.clone() } else { app.info.user_home.clone() }
}

/// A key in the editor.
pub fn key(app: &mut App, dialog: &mut Dialog, key: KeyEvent) -> Outcome {
    let len = dirs(app, dialog.group.as_deref()).len();
    let step = |sel: &mut usize, delta: i64| *sel = if len == 0 { 0 } else { (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize };
    match key.code {
        KeyCode::Esc => return Outcome::Close,
        KeyCode::Enter if dialog.input.text.trim().is_empty() => return Outcome::Close,
        KeyCode::Enter => add(app, dialog),
        KeyCode::Tab => return Outcome::KeepAndRun(Action::Skills(SkillAction::Browse(browse_from(app, dialog)))),
        KeyCode::Up => step(&mut dialog.sel, -1),
        KeyCode::Down => step(&mut dialog.sel, 1),
        KeyCode::Delete if dialog.input.text.is_empty() => remove(app, dialog),
        _ => {
            if dialog.input.handle(&key) {
                dialog.error = None;
            }
        }
    }
    Outcome::Keep
}

/// The mouse in the editor: its buttons and rows; the wheel moves the selection.
pub fn mouse(app: &mut App, dialog: &mut Dialog, mouse: MouseEvent, hit: Option<Hit>) -> Outcome {
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let code = if mouse.kind == MouseEventKind::ScrollUp { KeyCode::Up } else { KeyCode::Down };
            return key(app, dialog, KeyEvent::new(code, Modifiers::NONE));
        }
        MouseEventKind::Down(_) => {}
        _ => return Outcome::Keep,
    }
    let inside = app.layout.modal_rect.is_some_and(|r| crate::paint::rect_contains(r, mouse.column, mouse.row));
    match hit {
        Some(Hit::Button(0)) => {
            add(app, dialog);
            Outcome::Keep
        }
        Some(Hit::Button(1)) => {
            remove(app, dialog);
            Outcome::Keep
        }
        Some(Hit::Button(_)) => Outcome::Close,
        Some(Hit::ListRow(i)) => {
            dialog.sel = i;
            Outcome::Keep
        }
        _ if !inside => Outcome::Close,
        _ => Outcome::Keep,
    }
}

/// Pasted text goes into the folder field.
pub fn text(dialog: &mut Dialog, text: &str) {
    dialog.input.insert(text.trim());
    dialog.error = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::tests::{rig, settle};

    fn press(app: &mut App, code: KeyCode) {
        app.modal_key(KeyEvent::new(code, Modifiers::NONE));
    }

    fn type_text(app: &mut App, text: &str) {
        app.modal_paste(text);
    }

    fn dialog(app: &App) -> Dialog {
        match app.modals.last() {
            Some(Modal::SkillFolders(d)) => (**d).clone(),
            other => panic!("the editor, not {other:?}"),
        }
    }

    /// Types a folder, presses Enter and waits for the service's answer.
    fn submit(r: &mut crate::skills::tests::Rig, typed: &str) -> Dialog {
        type_text(&mut r.app, typed);
        press(&mut r.app, KeyCode::Enter);
        settle(r, |app| matches!(app.modals.last(), Some(Modal::SkillFolders(d)) if d.checking.is_none()));
        dialog(&r.app)
    }

    #[test]
    fn a_groups_folders_are_checked_on_the_services_machine_added_and_removed() {
        let mut r = rig();
        // The group's menu has it.
        let (_, items) = r.app.group_menu("g1");
        let item = items.iter().find(|i| i.label == "Skill folders…").expect("the menu item");
        r.app.run(item.action.clone());
        assert_eq!(dialog(&r.app).group.as_deref(), Some("g1"));
        settle(&mut r, |app| !app.skills.loading);
        assert_eq!(lines(&r.app, &dialog(&r.app)), [r"C:\skills\pay  · 2 skills"], "counted by the listing");
        // Not there, empty, already listed: refused with why (said first: a long path is cut off).
        for (typed, why) in [(r"D:\nowhere", "does not exist on the service's machine: "), (r"C:\skills\empty", "no SKILL.md in it"), (r"c:/SKILLS/pay", "already in the list")] {
            let error = submit(&mut r, typed).error.unwrap_or_default();
            assert!(error.starts_with(why), "{typed}: {error}");
            dialog_input_clear(&mut r.app);
        }
        // A folder of the canvas's own list is refused for a group.
        r.app.doc.defaults.skill_dirs = vec![r"C:\skills\all".into()];
        assert!(submit(&mut r, r"C:\skills\all").error.unwrap_or_default().contains("every session already gets it"));
        dialog_input_clear(&mut r.app);
        let added = submit(&mut r, r#""C:\skills\team""#);
        let skill_dirs = |app: &App| app.doc.group("g1").and_then(|g| g.defaults.clone()).map(|d| d.skill_dirs).unwrap_or_default();
        assert_eq!(skill_dirs(&r.app), [r"C:\skills\pay", r"C:\skills\team"]);
        assert_eq!((added.input.text.as_str(), added.error.clone(), added.sel), ("", None, 1));
        assert!(lines(&r.app, &dialog(&r.app))[1].ends_with("· 2 skills"));
        // Del removes the selected one; with none left the group keeps no empty overrides.
        press(&mut r.app, KeyCode::Delete);
        press(&mut r.app, KeyCode::Up);
        press(&mut r.app, KeyCode::Delete);
        assert!(r.app.doc.group("g1").unwrap().defaults.is_none());
        press(&mut r.app, KeyCode::Esc);
        assert!(r.app.modals.is_empty());
    }

    #[test]
    fn checking_a_folder_never_blocks_the_window_and_a_list_holds_at_most_twenty() {
        let mut r = rig();
        let (open, gate) = crossbeam_channel::bounded(4);
        *r.backend.gate.lock().unwrap() = Some(gate);
        r.app.run(Action::Skills(SkillAction::EditFolders(Some("g1".into()))));
        type_text(&mut r.app, r"C:\skills\team");
        // Enter returns at once while the service checks (the path check is held back).
        let started = std::time::Instant::now();
        press(&mut r.app, KeyCode::Enter);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "Enter waited for the service");
        assert_eq!(dialog(&r.app).checking.as_deref(), Some(r"C:\skills\team"));
        let mut buf = Buffer::empty(Rect::new(0, 0, 160, 45));
        crate::ui::render(&mut r.app, &mut buf, Rect::new(0, 0, 160, 45), None);
        let screen: String = (0..45).map(|y| (0..160).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect();
        assert!(screen.contains("Checking on the service's machine…"), "the editor says so: {screen}");
        // One check at a time: Enter again asks nothing more.
        press(&mut r.app, KeyCode::Enter);
        let asked = |r: &crate::skills::tests::Rig| r.backend.requests.lock().unwrap().iter().filter(|q| matches!(q, crate::rpc::Request::PathInfo { .. })).count();
        assert!(asked(&r) <= 1);
        open.send(()).unwrap();
        settle(&mut r, |app| matches!(app.modals.last(), Some(Modal::SkillFolders(d)) if d.checking.is_none()));
        assert_eq!(asked(&r), 1);
        assert_eq!(r.app.doc.group("g1").and_then(|g| g.defaults.clone()).map(|d| d.skill_dirs).unwrap_or_default(), [r"C:\skills\pay", r"C:\skills\team"]);
        // Closed before the answer: nothing is added.
        type_text(&mut r.app, r"C:\skills\late");
        press(&mut r.app, KeyCode::Enter);
        press(&mut r.app, KeyCode::Esc);
        assert!(r.app.modals.is_empty());
        open.send(()).unwrap();
        while let Ok(event) = r.events.recv_timeout(std::time::Duration::from_millis(500)) {
            r.app.on_app_event(event);
        }
        assert_eq!(r.app.doc.group("g1").and_then(|g| g.defaults.clone()).map(|d| d.skill_dirs.len()), Some(2));
        // A full list takes no more.
        *r.backend.gate.lock().unwrap() = None;
        r.app.doc.defaults.skill_dirs = (0..MAX_FOLDERS).map(|i| format!(r"C:\skills\s{i}")).collect();
        r.app.run(Action::Skills(SkillAction::EditFolders(None)));
        let full = submit(&mut r, r"C:\skills\one-more");
        assert!(full.error.unwrap_or_default().starts_with("at most 20 folders here"));
        assert_eq!(r.app.doc.defaults.skill_dirs.len(), MAX_FOLDERS);
    }

    fn dialog_input_clear(app: &mut App) {
        if let Some(Modal::SkillFolders(d)) = app.modals.last_mut() {
            d.input.set("");
        }
    }

    #[test]
    fn every_sessions_folders_come_from_settings_and_tab_browses() {
        let mut r = rig();
        r.app.set_view(crate::side::SideView::Settings);
        let rows = r.app.build_list();
        let at = rows.iter().position(|row| row.item == crate::side::ItemRef::SkillFolders(None)).expect("the Settings row");
        assert_eq!((rows[at].label.as_str(), rows[at].right.as_str()), ("Skill folders (all sessions)…", "(none)"));
        r.app.list_activate(at);
        assert_eq!(dialog(&r.app).group, None);
        type_text(&mut r.app, r"C:\skills");
        // Tab: the folder browser over the editor; choosing a folder fills it in.
        press(&mut r.app, KeyCode::Tab);
        assert!(matches!(r.app.modals.last(), Some(Modal::Folder(_))));
        r.app.modal_key(KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL));
        assert_eq!(dialog(&r.app).input.text, r"C:\skills");
        press(&mut r.app, KeyCode::Enter);
        settle(&mut r, |app| matches!(app.modals.last(), Some(Modal::SkillFolders(d)) if d.checking.is_none()));
        assert_eq!(r.app.doc.defaults.skill_dirs, [r"C:\skills"]);
        let rows = r.app.build_list();
        assert_eq!(rows[at].right, "1 folder");
        // Enter on an empty field closes the editor.
        press(&mut r.app, KeyCode::Enter);
        assert!(r.app.modals.is_empty());
        // A group's own Settings show its folders.
        r.app.settings_group = Some("g1".into());
        let rows = r.app.build_list();
        assert!(rows.iter().any(|row| row.item == crate::side::ItemRef::SkillFolders(Some("g1".into())) && row.label == "Skill folders (this group)…" && row.right == "1 folder"));
    }
}
