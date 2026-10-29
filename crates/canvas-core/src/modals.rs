//! Keyboard, mouse and paste handling for modal dialogs.

use std::path::PathBuf;

use crate::app::App;
use crate::dialogs::{Action, FolderBrowser, GitState, Hit, Modal, NewSession, NsField, Pick, PickPurpose, PromptPurpose, Target};
use crate::input::{KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use crate::paint::rect_contains;

enum Outcome {
    Keep,
    Close,
    CloseAndRun(Action),
    /// Keeps the dialog and runs an action (a dialog opened on top of it).
    KeepAndRun(Action),
}

fn list_move(sel: &mut usize, len: usize, delta: i64) {
    if len == 0 {
        *sel = 0;
        return;
    }
    *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
}

fn nav_delta(code: KeyCode) -> Option<i64> {
    match code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        KeyCode::PageUp => Some(-10),
        KeyCode::PageDown => Some(10),
        KeyCode::Home => Some(-1_000_000),
        KeyCode::End => Some(1_000_000),
        _ => None,
    }
}

impl App {
    pub fn modal_paste(&mut self, text: &str) {
        match self.modals.last_mut() {
            Some(Modal::Rename(rename)) => rename.input.insert(text),
            Some(Modal::NewSession(dialog)) => {
                let input = match dialog.focus {
                    NsField::Name => &mut dialog.name,
                    NsField::Folder => {
                        dialog.folder_changed_at = Some(std::time::Instant::now());
                        &mut dialog.folder
                    }
                    NsField::NewBranch => {
                        dialog.branch_edited = true;
                        &mut dialog.new_branch
                    }
                    NsField::WtPath => {
                        dialog.path_edited = true;
                        &mut dialog.wt_path
                    }
                    _ => &mut dialog.prompt,
                };
                input.insert(text);
            }
            Some(Modal::ImportCopilot(dialog)) => dialog.filter.insert(text),
            Some(Modal::Folder(browser)) => browser.filter.push_str(text.trim()),
            Some(Modal::Pick(pick)) => pick.filter.push_str(text.trim()),
            _ => {}
        }
        self.needs_draw = true;
    }

    pub fn modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modals.pop() else { return };
        let outcome = match &mut modal {
            Modal::Help => Outcome::Close,
            Modal::Menu(menu) => match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Enter => match menu.items.get(menu.sel) {
                    Some(item) if item.enabled => Outcome::CloseAndRun(item.action.clone()),
                    _ => Outcome::Keep,
                },
                code => {
                    if let Some(delta) = nav_delta(code) {
                        let len = menu.items.len();
                        let mut next = menu.sel;
                        for _ in 0..len {
                            list_move(&mut next, len, delta.signum());
                            if menu.items[next].enabled || next == 0 || next == len - 1 {
                                break;
                            }
                        }
                        if menu.items.get(next).is_some_and(|i| i.enabled) {
                            menu.sel = next;
                        }
                    }
                    Outcome::Keep
                }
            },
            Modal::Confirm(confirm) => match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Left | KeyCode::BackTab => {
                    confirm.sel = (confirm.sel + confirm.buttons.len() - 1) % confirm.buttons.len();
                    Outcome::Keep
                }
                KeyCode::Right | KeyCode::Tab => {
                    confirm.sel = (confirm.sel + 1) % confirm.buttons.len();
                    Outcome::Keep
                }
                KeyCode::Enter => Outcome::CloseAndRun(confirm.buttons[confirm.sel].1.clone()),
                _ => Outcome::Keep,
            },
            Modal::Rename(rename) => match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Enter => {
                    let text = rename.input.text.trim().to_owned();
                    if text.is_empty() {
                        self.notify("A name cannot be empty.", true);
                        Outcome::Keep
                    } else {
                        match &rename.target {
                            Target::Node(id) => {
                                if let Some(node) = self.doc.node_mut(id) {
                                    node.name = text.chars().take(120).collect();
                                }
                            }
                            Target::Group(id) => {
                                if let Some(group) = self.doc.group_mut(id) {
                                    group.name = text.chars().take(120).collect();
                                }
                            }
                        }
                        self.mark_dirty();
                        Outcome::Close
                    }
                }
                _ => {
                    rename.input.handle(&key);
                    Outcome::Keep
                }
            },
            Modal::NewSession(dialog) => self.new_session_key(dialog, key),
            Modal::Folder(browser) => self.folder_key(browser, key),
            Modal::Pick(pick) => self.pick_key(pick, key),
            Modal::Mcp(picker) => self.mcp_key(picker, key),
            Modal::Viewer(viewer) => match viewer.key(&key) {
                crate::viewer::ViewerKey::Keep => Outcome::Keep,
                crate::viewer::ViewerKey::Close => Outcome::Close,
                crate::viewer::ViewerKey::Button(button) if self.viewer_button(viewer, button) => Outcome::Close,
                crate::viewer::ViewerKey::Button(_) => Outcome::Keep,
                crate::viewer::ViewerKey::Open(i) => match viewer.entry_path(Some(i)) {
                    Some((path, true)) => {
                        viewer.navigate(path.clone());
                        self.load_browser(path);
                        Outcome::Keep
                    }
                    Some((path, false)) => Outcome::KeepAndRun(Action::Visual(crate::visual::VisualAction::Browse(path))),
                    None => Outcome::Keep,
                },
                crate::viewer::ViewerKey::Up => {
                    if let Some((path, _)) = viewer.entry_path(None) {
                        viewer.navigate(path.clone());
                        self.load_browser(path);
                    }
                    Outcome::Keep
                }
            },
            Modal::Inspector(inspector) => match self.inspector_key(inspector, &key) {
                crate::inspect::KeyOutcome::Keep => Outcome::Keep,
                crate::inspect::KeyOutcome::Close => Outcome::Close,
                crate::inspect::KeyOutcome::Run(action) => Outcome::CloseAndRun(action),
            },
            Modal::Report(viewer) => match self.report_key(viewer, &key) {
                crate::reports::ViewerOutcome::Keep => Outcome::Keep,
                crate::reports::ViewerOutcome::Close => Outcome::Close,
                crate::reports::ViewerOutcome::Run(action) => Outcome::KeepAndRun(action),
                crate::reports::ViewerOutcome::CloseAndRun(action) => Outcome::CloseAndRun(action),
            },
            Modal::ImportCopilot(dialog) => {
                let candidates = self.copilot_import_candidates(&dialog.filter.text);
                let ctrl = key.modifiers.contains(Modifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => Outcome::Close,
                    KeyCode::Tab => {
                        dialog.grouping = dialog.grouping.next();
                        Outcome::Keep
                    }
                    KeyCode::Char(' ') => {
                        if let Some(id) = candidates.get(dialog.sel) {
                            if !dialog.chosen.remove(id) {
                                dialog.chosen.insert(id.clone());
                            }
                            list_move(&mut dialog.sel, candidates.len(), 1);
                        }
                        Outcome::Keep
                    }
                    KeyCode::Char('a') if ctrl => {
                        let all = candidates.iter().all(|c| dialog.chosen.contains(c));
                        for id in &candidates {
                            if all {
                                dialog.chosen.remove(id);
                            } else {
                                dialog.chosen.insert(id.clone());
                            }
                        }
                        Outcome::Keep
                    }
                    KeyCode::Enter => {
                        let ids: Vec<String> = if dialog.chosen.is_empty() {
                            candidates.get(dialog.sel).cloned().into_iter().collect()
                        } else {
                            candidates.iter().filter(|c| dialog.chosen.contains(*c)).cloned().collect()
                        };
                        if ids.is_empty() {
                            Outcome::Keep
                        } else {
                            let count = ids.len();
                            self.import_ids(ids, dialog.grouping);
                            self.notify(format!("Imported {count} session(s)."), false);
                            Outcome::Close
                        }
                    }
                    code => {
                        if let Some(delta) = nav_delta(code) {
                            list_move(&mut dialog.sel, candidates.len(), delta);
                        } else if dialog.filter.handle(&key) {
                            dialog.sel = 0;
                            dialog.scroll = 0;
                        }
                        Outcome::Keep
                    }
                }
            }
            Modal::Prompt(prompt) => match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Tab if matches!(prompt.purpose, PromptPurpose::GroupFolder(_)) => {
                    let start = if prompt.input.text.trim().is_empty() { self.info.home.clone() } else { prompt.input.text.clone() };
                    self.modals.push(Modal::Prompt(prompt.clone()));
                    self.open_folder_browser(&start);
                    Outcome::Close
                }
                KeyCode::Enter => match self.apply_prompt(prompt.purpose.clone(), prompt.input.text.clone()) {
                    Ok(()) => Outcome::Close,
                    Err(error) => {
                        self.notify(error, true);
                        Outcome::Keep
                    }
                },
                _ => {
                    prompt.input.handle(&key);
                    Outcome::Keep
                }
            },
            Modal::Migrate(dialog) => {
                let count = dialog.items.as_ref().map(|i| i.len()).unwrap_or(0);
                if dialog.result.is_some() {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => Outcome::Close,
                        _ => Outcome::Keep,
                    }
                } else {
                    match key.code {
                        KeyCode::Esc if !dialog.applying => Outcome::Close,
                        KeyCode::Char(' ') => {
                            let new = dialog.items.as_ref().and_then(|i| i.get(dialog.sel)).is_some_and(|i| i.status == crate::migrate::Status::New);
                            if let Some(chosen) = dialog.chosen.get_mut(dialog.sel).filter(|_| new) {
                                *chosen = !*chosen;
                            }
                            list_move(&mut dialog.sel, count, 1);
                            Outcome::Keep
                        }
                        KeyCode::Enter if !dialog.applying && count > 0 => {
                            let items = dialog.items.clone().unwrap_or_default();
                            let chosen: Vec<crate::migrate::Item> = items
                                .into_iter()
                                .zip(dialog.chosen.iter())
                                .filter(|(item, on)| **on && item.status == crate::migrate::Status::New)
                                .map(|(item, _)| item)
                                .collect();
                            if chosen.is_empty() {
                                self.notify("Nothing selected to migrate.", false);
                            } else {
                                dialog.applying = true;
                                self.apply_migration(chosen);
                            }
                            Outcome::Keep
                        }
                        code => {
                            if let Some(delta) = nav_delta(code) {
                                list_move(&mut dialog.sel, count, delta);
                            }
                            Outcome::Keep
                        }
                    }
                }
            }
        };
        self.finish_modal(modal, outcome);
    }

    fn finish_modal(&mut self, modal: Modal, outcome: Outcome) {
        self.needs_draw = true;
        match outcome {
            Outcome::Keep => self.modals.push(modal),
            Outcome::KeepAndRun(action) => {
                self.modals.push(modal);
                self.run(action);
            }
            Outcome::Close => {}
            Outcome::CloseAndRun(action) => self.run(action),
        }
    }

    fn new_session_key(&mut self, dialog: &mut NewSession, key: KeyEvent) -> Outcome {
        let shift = key.modifiers.contains(Modifiers::SHIFT);
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Tab => {
                dialog.cycle(true);
                return Outcome::Keep;
            }
            KeyCode::BackTab => {
                dialog.cycle(false);
                return Outcome::Keep;
            }
            KeyCode::Down if !matches!(dialog.focus, NsField::Base) => {
                dialog.cycle(true);
                return Outcome::Keep;
            }
            KeyCode::Up => {
                dialog.cycle(false);
                return Outcome::Keep;
            }
            _ => {}
        }
        let _ = shift;
        let activate = matches!(key.code, KeyCode::Enter) || (matches!(key.code, KeyCode::Char(' ')) && !is_text(dialog.focus));
        match dialog.focus {
            NsField::Browse if activate => {
                let start = dialog.folder.text.clone();
                self.modals.push(Modal::NewSession(Box::new(dialog.clone())));
                self.open_folder_browser(&start);
                return Outcome::Close;
            }
            NsField::Recent if activate => {
                let items: Vec<(String, String)> = self
                    .doc
                    .recent_folders
                    .iter()
                    .map(|f| (f.path.clone(), crate::paths::format_age(f.used_ms, crate::paths::now_ms())))
                    .chain(self.recent_session_folders())
                    .collect();
                let mut seen = std::collections::HashSet::new();
                let items: Vec<(String, String)> = items.into_iter().filter(|(p, _)| seen.insert(p.to_lowercase())).take(60).collect();
                if items.is_empty() {
                    self.notify("No recent folders yet.", false);
                    return Outcome::Keep;
                }
                self.modals.push(Modal::NewSession(Box::new(dialog.clone())));
                self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::RecentFolder, title: "Recent folders".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
                return Outcome::Close;
            }
            NsField::Mode if activate || matches!(key.code, KeyCode::Left | KeyCode::Right) => {
                dialog.worktree = !dialog.worktree;
                Self::refresh_worktree_defaults(dialog);
                return Outcome::Keep;
            }
            NsField::Base if activate => {
                if let GitState::Repo(info) = &dialog.git {
                    let items = info
                        .branches
                        .iter()
                        .enumerate()
                        .map(|(i, b)| (b.clone(), if i < info.local_count { "local".into() } else { "remote".into() }))
                        .collect();
                    self.modals.push(Modal::NewSession(Box::new(dialog.clone())));
                    self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::Branch, title: "Base branch".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
                    return Outcome::Close;
                }
                return Outcome::Keep;
            }
            NsField::Context if activate || matches!(key.code, KeyCode::Left | KeyCode::Right) => {
                dialog.fork = !dialog.fork;
                return Outcome::Keep;
            }
            NsField::Agent if activate || matches!(key.code, KeyCode::Left | KeyCode::Right) => {
                self.cycle_dialog_agent(dialog, key.code != KeyCode::Left);
                return Outcome::Keep;
            }
            NsField::Model if activate => {
                self.modals.push(Modal::NewSession(Box::new(dialog.clone())));
                self.open_model_picker(crate::dialogs::ModelTarget::Dialog);
                return Outcome::Close;
            }
            NsField::Mcp if activate => {
                self.modals.push(Modal::NewSession(Box::new(dialog.clone())));
                self.open_mcp_picker(crate::dialogs::McpTarget::NewSession);
                return Outcome::Close;
            }
            NsField::Cancel if activate => return Outcome::Close,
            _ => {}
        }
        if matches!(key.code, KeyCode::Enter) {
            return match self.submit_new_session(dialog) {
                Ok(()) => Outcome::Close,
                Err(error) => {
                    dialog.error = Some(error);
                    Outcome::Keep
                }
            };
        }
        let changed = match dialog.focus {
            NsField::Name => dialog.name.handle(&key),
            NsField::Folder => {
                let changed = dialog.folder.handle(&key);
                if changed {
                    dialog.folder_changed_at = Some(std::time::Instant::now());
                }
                changed
            }
            NsField::NewBranch => {
                let changed = dialog.new_branch.handle(&key);
                dialog.branch_edited |= changed;
                changed
            }
            NsField::WtPath => {
                let changed = dialog.wt_path.handle(&key);
                dialog.path_edited |= changed;
                changed
            }
            NsField::Prompt => dialog.prompt.handle(&key),
            _ => false,
        };
        if changed {
            dialog.error = None;
            Self::refresh_worktree_defaults(dialog);
        }
        Outcome::Keep
    }

    fn mcp_key(&mut self, picker: &mut crate::dialogs::McpPicker, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let choices = self.mcp_choices(picker.agent).unwrap_or_default();
        let known = self.mcp_available(picker.agent);
        let names = self.doc.mcp.names();
        match key.code {
            KeyCode::Esc => Outcome::Close,
            KeyCode::Enter => {
                let target = picker.target.clone();
                self.apply_mcp_choice(&target, picker.agent, picker.off.clone());
                Outcome::Close
            }
            KeyCode::Char(' ') => {
                if let Some((name, _, true)) = choices.get(picker.sel) {
                    match picker.off.iter().position(|o| o == name) {
                        Some(pos) => {
                            picker.off.remove(pos);
                        }
                        None => picker.off.push(name.clone()),
                    }
                }
                list_move(&mut picker.sel, choices.len(), 1);
                Outcome::Keep
            }
            KeyCode::Char('a') if !ctrl => {
                picker.off.retain(|o| !known.contains(o));
                Outcome::Keep
            }
            KeyCode::Char('n') if !ctrl => {
                picker.off = crate::doc::normalize_off(&[picker.off.clone(), known].concat());
                Outcome::Keep
            }
            KeyCode::Left | KeyCode::Right => {
                let current = self.doc.mcp.matching(&picker.off, &known).and_then(|m| names.iter().position(|n| *n == m));
                let next = match (current, key.code) {
                    (Some(i), KeyCode::Right) => (i + 1) % names.len(),
                    (Some(i), _) => (i + names.len() - 1) % names.len(),
                    (None, KeyCode::Right) => 0,
                    (None, _) => names.len() - 1,
                };
                if let Some(off) = self.doc.mcp.preset_off(&names[next], &known) {
                    picker.off = off;
                }
                Outcome::Keep
            }
            KeyCode::Char('s') if ctrl => {
                let off = picker.off.clone();
                self.modals.push(Modal::Mcp(picker.clone()));
                self.modals.push(Modal::Prompt(crate::dialogs::Prompt {
                    title: "Save MCP preset".into(),
                    hint: "Name for this selection (an existing name is replaced)".into(),
                    input: crate::dialogs::TextInput::default(),
                    purpose: PromptPurpose::McpPreset(off),
                }));
                Outcome::Close
            }
            KeyCode::Delete => {
                match self.doc.mcp.matching(&picker.off, &known).filter(|m| self.doc.mcp.presets.iter().any(|p| p.name == *m)) {
                    Some(name) => {
                        self.doc.mcp.presets.retain(|p| p.name != name);
                        self.mark_dirty();
                        self.notify(format!("Deleted MCP preset “{name}”."), false);
                    }
                    None => self.notify("Choose a saved preset (←/→) to delete it.", false),
                }
                Outcome::Keep
            }
            code => {
                if let Some(delta) = nav_delta(code) {
                    list_move(&mut picker.sel, choices.len(), delta);
                }
                Outcome::Keep
            }
        }
    }

    fn recent_session_folders(&self) -> Vec<(String, String)> {
        let mut list: Vec<&crate::protocol::SessionSummary> = self.sessions.values().filter(|s| !s.cwd.is_empty()).collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        let now = crate::paths::now_ms();
        list.into_iter().take(200).map(|s| (s.cwd.clone(), format!("session {}", crate::paths::format_age(s.updated_ms, now)))).collect()
    }

    fn folder_visible(browser: &FolderBrowser) -> Vec<usize> {
        let needle = browser.filter.to_lowercase();
        (0..browser.entries.len()).filter(|&i| needle.is_empty() || browser.entries[i].to_lowercase().contains(&needle)).collect()
    }

    fn folder_open(backend: &dyn crate::rpc::Backend, browser: &mut FolderBrowser, name: &str) {
        let target = match (&browser.dir, name) {
            (Some(dir), "..") => dir.parent().map(PathBuf::from),
            (Some(dir), name) => Some(dir.join(name)),
            (None, drive) => Some(PathBuf::from(drive)),
        };
        Self::folder_navigate(backend, browser, target);
    }

    fn choose_folder(&mut self, path: String) {
        if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
            dialog.folder.set(&path);
            dialog.folder_changed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
            dialog.focus = NsField::Name;
            dialog.error = None;
        } else if let Some(Modal::Prompt(prompt)) = self.modals.last_mut() {
            prompt.input.set(&path);
        }
    }

    fn folder_key(&mut self, browser: &mut FolderBrowser, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let visible = Self::folder_visible(browser);
        let backend = self.backend.clone();
        match key.code {
            KeyCode::Esc => Outcome::Close,
            KeyCode::Char('s') if ctrl => match &browser.dir {
                Some(dir) => {
                    self.choose_folder(dir.to_string_lossy().into_owned());
                    Outcome::Close
                }
                None => Outcome::Keep,
            },
            KeyCode::Enter | KeyCode::Right => {
                if let Some(&index) = visible.get(browser.sel) {
                    let name = browser.entries[index].clone();
                    Self::folder_open(&*backend, browser, &name);
                }
                Outcome::Keep
            }
            KeyCode::Left => {
                Self::folder_open(&*backend, browser, "..");
                Outcome::Keep
            }
            KeyCode::Backspace if browser.filter.is_empty() => {
                Self::folder_open(&*backend, browser, "..");
                Outcome::Keep
            }
            KeyCode::Backspace => {
                browser.filter.pop();
                browser.sel = 0;
                Outcome::Keep
            }
            KeyCode::Char(c) if !ctrl => {
                browser.filter.push(c);
                browser.sel = 0;
                browser.scroll = 0;
                Outcome::Keep
            }
            code => {
                if let Some(delta) = nav_delta(code) {
                    list_move(&mut browser.sel, visible.len(), delta);
                }
                Outcome::Keep
            }
        }
    }

    fn apply_pick(&mut self, purpose: PickPurpose, value: String) {
        if let PickPurpose::Default(field, group) = purpose {
            return self.apply_default(field, group, &value);
        }
        if let PickPurpose::Model(target) = purpose {
            if let Err(error) = self.apply_model_choice(target, &value) {
                self.notify(error, true);
            }
            return;
        }
        if let PickPurpose::PassiveTemplate(group) = purpose {
            let template = crate::passive::TEMPLATES.iter().position(|t| t.name == value).unwrap_or(2);
            self.modals.push(Modal::Prompt(crate::dialogs::Prompt {
                title: format!("Name the {} passive session", crate::passive::TEMPLATES[template].name),
                hint: "for example: Drift guard · PR !164587 (Enter keeps the template name)".into(),
                input: crate::dialogs::TextInput::new(crate::passive::TEMPLATES[template].name),
                purpose: PromptPurpose::PassiveName(group, template),
            }));
            return;
        }
        if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
            match purpose {
                PickPurpose::RecentFolder => {
                    dialog.folder.set(&value);
                    dialog.folder_changed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
                }
                PickPurpose::Branch => {
                    dialog.base = value;
                    Self::refresh_worktree_defaults(dialog);
                }
                PickPurpose::Default(..) | PickPurpose::PassiveTemplate(_) | PickPurpose::Model(_) => {}
            }
            dialog.error = None;
        }
    }

    fn pick_key(&mut self, pick: &mut Pick, key: KeyEvent) -> Outcome {
        let visible = pick.visible();
        match key.code {
            KeyCode::Esc => Outcome::Close,
            // A model picker takes any id: one that matches nothing is used as typed.
            KeyCode::Enter if visible.is_empty() && !pick.filter.trim().is_empty() && matches!(pick.purpose, PickPurpose::Model(_)) => {
                let (purpose, value) = (pick.purpose.clone(), pick.filter.trim().to_owned());
                self.apply_pick(purpose, value);
                Outcome::Close
            }
            KeyCode::Enter => {
                if let Some(&index) = visible.get(pick.sel) {
                    let value = pick.items[index].0.clone();
                    let purpose = pick.purpose.clone();
                    self.apply_pick(purpose, value);
                }
                Outcome::Close
            }
            KeyCode::Backspace => {
                pick.filter.pop();
                pick.sel = 0;
                Outcome::Keep
            }
            KeyCode::Char(c) if !key.modifiers.contains(Modifiers::CONTROL) => {
                pick.filter.push(c);
                pick.sel = 0;
                pick.scroll = 0;
                Outcome::Keep
            }
            code => {
                if let Some(delta) = nav_delta(code) {
                    list_move(&mut pick.sel, visible.len(), delta);
                }
                Outcome::Keep
            }
        }
    }

    pub fn modal_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        let wheel = match mouse.kind {
            MouseEventKind::ScrollUp => Some(-3i32),
            MouseEventKind::ScrollDown => Some(3),
            _ => None,
        };
        if let Some(delta) = wheel {
            let key = KeyEvent::new(if delta < 0 { KeyCode::Up } else { KeyCode::Down }, Modifiers::NONE);
            if !matches!(self.modals.last(), Some(Modal::NewSession(_)) | Some(Modal::Confirm(_)) | Some(Modal::Rename(_))) {
                for _ in 0..delta.abs() {
                    self.modal_key(key);
                }
            }
            return;
        }
        let MouseEventKind::Down(button) = mouse.kind else { return };
        let inside = self.layout.modal_rect.is_some_and(|r| rect_contains(r, x, y));
        if !inside {
            if matches!(self.modals.last(), Some(Modal::Menu(_)) | Some(Modal::Help)) {
                self.modals.pop();
                self.needs_draw = true;
            }
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let Some(hit) = self.layout.hits.iter().rev().find(|(r, _)| rect_contains(*r, x, y)).map(|(_, h)| h.clone()) else {
            if matches!(self.modals.last(), Some(Modal::Help)) {
                self.modals.pop();
            }
            return;
        };
        let double = {
            let now = std::time::Instant::now();
            let double = self.last_click.is_some_and(|(t, cx, cy)| now - t < std::time::Duration::from_millis(400) && cx == x && cy == y);
            self.last_click = if double { None } else { Some((now, x, y)) };
            double
        };
        let enter = KeyEvent::new(KeyCode::Enter, Modifiers::NONE);
        self.needs_draw = true;
        match (self.modals.last_mut(), hit) {
            (Some(Modal::Menu(menu)), Hit::MenuItem(i)) => {
                if menu.items.get(i).is_some_and(|item| item.enabled) {
                    menu.sel = i;
                    self.modal_key(enter);
                }
            }
            (Some(Modal::Confirm(confirm)), Hit::Button(i)) => {
                confirm.sel = i.min(confirm.buttons.len() - 1);
                self.modal_key(enter);
            }
            (Some(Modal::Rename(_)), Hit::Button(0)) => self.modal_key(enter),
            (Some(Modal::Rename(_)), Hit::Button(_)) => {
                self.modals.pop();
            }
            (Some(Modal::NewSession(dialog)), Hit::Field(field)) => {
                dialog.focus = field;
                if matches!(field, NsField::Browse | NsField::Recent | NsField::Mode | NsField::Base | NsField::Agent | NsField::Model | NsField::Context | NsField::Mcp | NsField::Create | NsField::Cancel) {
                    self.modal_key(enter);
                }
            }
            (Some(Modal::Folder(browser)), Hit::ListRow(i)) => {
                let visible = Self::folder_visible(browser);
                if let Some(pos) = visible.iter().position(|&v| v == i) {
                    browser.sel = pos;
                    if double {
                        self.modal_key(enter);
                    }
                }
            }
            (Some(Modal::Folder(browser)), Hit::Button(0)) => {
                if let Some(dir) = browser.dir.clone() {
                    self.modals.pop();
                    self.choose_folder(dir.to_string_lossy().into_owned());
                }
            }
            (Some(Modal::Folder(_)), Hit::Button(_)) => {
                self.modals.pop();
            }
            (Some(Modal::Pick(pick)), Hit::ListRow(i)) => {
                if let Some(pos) = pick.visible().iter().position(|&v| v == i) {
                    pick.sel = pos;
                    self.modal_key(enter);
                }
            }
            (Some(Modal::ImportCopilot(dialog)), Hit::ListRow(i)) => {
                dialog.sel = i;
                self.modal_key(KeyEvent::new(KeyCode::Char(' '), Modifiers::NONE));
                if let Some(Modal::ImportCopilot(dialog)) = self.modals.last_mut() {
                    dialog.sel = i;
                }
            }
            (Some(Modal::ImportCopilot(dialog)), Hit::Toggle(_)) => dialog.grouping = dialog.grouping.next(),
            (Some(Modal::ImportCopilot(_)), Hit::Button(0)) => self.modal_key(enter),
            (Some(Modal::Prompt(_)), Hit::Button(0)) => self.modal_key(enter),
            (Some(Modal::Migrate(dialog)), Hit::ListRow(i)) => {
                dialog.sel = i;
                self.modal_key(KeyEvent::new(KeyCode::Char(' '), Modifiers::NONE));
                if let Some(Modal::Migrate(dialog)) = self.modals.last_mut() {
                    dialog.sel = i;
                }
            }
            (Some(Modal::Migrate(_)), Hit::Button(0)) => self.modal_key(enter),
            (Some(Modal::Mcp(picker)), Hit::ListRow(i)) => {
                picker.sel = i;
                self.modal_key(KeyEvent::new(KeyCode::Char(' '), Modifiers::NONE));
                if let Some(Modal::Mcp(picker)) = self.modals.last_mut() {
                    picker.sel = i;
                }
            }
            (Some(Modal::Mcp(picker)), Hit::Chip(i)) => {
                let agent = picker.agent;
                let known = self.mcp_available(agent);
                let off = self.doc.mcp.names().get(i).and_then(|name| self.doc.mcp.preset_off(name, &known));
                if let (Some(off), Some(Modal::Mcp(picker))) = (off, self.modals.last_mut()) {
                    picker.off = off;
                }
            }
            (Some(Modal::Mcp(_)), Hit::Button(0)) => self.modal_key(enter),
            (Some(Modal::Mcp(_)), Hit::Button(1)) => self.modal_key(KeyEvent::new(KeyCode::Char('s'), Modifiers::CONTROL)),
            (Some(Modal::Viewer(viewer)), Hit::ListRow(i)) => {
                viewer.pick = i;
                viewer.on_buttons = false;
                self.modal_key(enter);
            }
            (Some(Modal::Viewer(viewer)), Hit::Button(i)) => {
                viewer.sel = i;
                viewer.on_buttons = true;
                self.modal_key(enter);
            }
            (Some(Modal::Inspector(inspector)), Hit::Button(i)) => {
                inspector.sel = i;
                self.modal_key(enter);
            }
            (Some(Modal::Report(viewer)), Hit::Button(i)) => {
                viewer.sel = i;
                self.modal_key(enter);
            }
            // A link in a report, or one of its details (they ask or only view).
            (Some(Modal::Report(_)), Hit::Toolbar(action)) => self.run(action),
            (Some(_), Hit::Button(_)) => {
                self.modals.pop();
            }
            _ => {}
        }
    }
}

fn is_text(field: NsField) -> bool {
    matches!(field, NsField::Name | NsField::Folder | NsField::NewBranch | NsField::WtPath | NsField::Prompt)
}
