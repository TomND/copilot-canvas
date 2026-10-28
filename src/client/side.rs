//! Activity-bar views beyond Sessions: Automations, Plugins, MCP servers,
//! Skills, Agents & instructions, VS Code, and Settings — plus the features
//! they drive (automation boxes, VS Code sync, group folders, launch defaults).

use std::path::PathBuf;
use std::time::Duration;

use ratatui::style::Color;

use super::app::{App, AppEvent, Seed};
use super::dialogs::{Confirm, Modal, Pick, PickPurpose, Prompt, PromptPurpose, TextInput};
use super::doc::{CONTEXTS, EFFORTS, GRID_X, GRID_Y, Group, LaunchDefaults, MODES, Node, NodeKind, Rect, SESSION_H, SESSION_W};
use super::paint::{BLUE, DARK, GRAY, POSITIVE, MUTED, PURPLE, NEGATIVE, TEXT, ATTENTION};
use super::status::Status;
use crate::automations::{self, Automation};
use crate::copilot_cfg::{self, Kind};
use crate::paths::{format_age, now_ms, parse_rfc3339_ms};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideView {
    Sessions,
    Automations,
    Plugins,
    Mcp,
    Skills,
    Agents,
    VsCode,
    Settings,
}

pub const VIEWS: [(SideView, &str, &str); 8] = [
    (SideView::Sessions, "▤", "SESSIONS"),
    (SideView::Automations, "◷", "AUTOMATIONS"),
    (SideView::Plugins, "⧉", "PLUGINS"),
    (SideView::Mcp, "⌁", "MCP SERVERS"),
    (SideView::Skills, "✦", "SKILLS"),
    (SideView::Agents, "◈", "AGENTS & INSTRUCTIONS"),
    (SideView::VsCode, "⇄", "VS CODE"),
    (SideView::Settings, "≡", "SETTINGS"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultField {
    Model,
    Effort,
    Mode,
    Agent,
    Context,
    AddDirs,
    Permissions,
}

impl DefaultField {
    pub const ALL: [DefaultField; 7] = [
        DefaultField::Permissions,
        DefaultField::Model,
        DefaultField::Effort,
        DefaultField::Mode,
        DefaultField::Agent,
        DefaultField::Context,
        DefaultField::AddDirs,
    ];
    pub fn label(self) -> &'static str {
        match self {
            DefaultField::Model => "Model",
            DefaultField::Effort => "Reasoning effort",
            DefaultField::Mode => "Start mode",
            DefaultField::Agent => "Custom agent",
            DefaultField::Context => "Context tier",
            DefaultField::AddDirs => "Extra folders",
            DefaultField::Permissions => "Permissions",
        }
    }
    pub fn get(self, d: &LaunchDefaults) -> String {
        match self {
            DefaultField::Model => d.model.clone().unwrap_or_default(),
            DefaultField::Effort => d.reasoning_effort.clone().unwrap_or_default(),
            DefaultField::Mode => d.mode.clone().unwrap_or_default(),
            DefaultField::Agent => d.agent.clone().unwrap_or_default(),
            DefaultField::Context => d.context.clone().unwrap_or_default(),
            DefaultField::AddDirs => d.add_dirs.join("; "),
            DefaultField::Permissions => d.permissions.clone().unwrap_or_default(),
        }
    }
    pub fn set(self, d: &mut LaunchDefaults, value: &str) {
        let value = value.trim();
        let opt = (!value.is_empty() && value != "(not set)").then(|| value.to_owned());
        match self {
            DefaultField::Model => d.model = opt,
            DefaultField::Effort => d.reasoning_effort = opt,
            DefaultField::Mode => d.mode = opt,
            DefaultField::Agent => d.agent = opt,
            DefaultField::Context => d.context = opt,
            DefaultField::AddDirs => d.add_dirs = value.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
            DefaultField::Permissions => d.permissions = opt,
        }
    }
    pub fn choices(self) -> Option<&'static [&'static str]> {
        match self {
            DefaultField::Effort => Some(&EFFORTS),
            DefaultField::Mode => Some(&MODES),
            DefaultField::Context => Some(&CONTEXTS),
            DefaultField::Permissions => Some(&super::doc::PERMISSIONS),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddKind {
    Skill,
    Mcp,
    Plugin,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemRef {
    Header,
    Info,
    Automation(String),
    Plugin(String),
    Mcp(String),
    Skill(String),
    Agent(String),
    Instruction(String),
    Default(DefaultField),
    Setting(String),
    AddSetting,
    Add(AddKind),
    Refresh,
    VsSync,
    VsWindow,
    VsArchived,
    VsCliSessions,
    VsUndo,
    IdleStop,
    AutomationsDir,
    VsAuto,
    VsMigrate,
}

#[derive(Clone, Debug)]
pub struct ListRow {
    pub item: ItemRef,
    pub label: String,
    pub right: String,
    pub icon: &'static str,
    pub color: Color,
}

impl ListRow {
    fn header(label: impl Into<String>) -> Self {
        ListRow { item: ItemRef::Header, label: label.into(), right: String::new(), icon: "", color: MUTED }
    }
    fn info(label: impl Into<String>) -> Self {
        ListRow { item: ItemRef::Info, label: label.into(), right: String::new(), icon: "", color: MUTED }
    }
    fn row(item: ItemRef, icon: &'static str, color: Color, label: impl Into<String>, right: impl Into<String>) -> Self {
        ListRow { item, label: label.into(), right: right.into(), icon, color }
    }
    pub fn selectable(&self) -> bool {
        !matches!(self.item, ItemRef::Header | ItemRef::Info)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RightDetail {
    Automation(String),
    Item(ItemRef),
    /// A passive session's settings and state (node id).
    Passive(String),
}

fn enabled_icon(enabled: bool) -> (&'static str, Color) {
    if enabled { ("●", POSITIVE) } else { ("○", DARK) }
}

pub fn automation_status(automation: &Automation) -> Status {
    if let Some(status) = super::factory::task_status(automation) {
        return status;
    }
    let last = automation.last_status().to_ascii_lowercase();
    if last == "running" || last == "pending" {
        return Status::Working;
    }
    if last.starts_with("fail") || last == "error" {
        return Status::Failed;
    }
    if !automation.enabled {
        return Status::Stopped;
    }
    let recent = parse_rfc3339_ms(&automation.last_run_at).is_some_and(|t| now_ms().saturating_sub(t) < super::status::RECENT_MS);
    if recent { Status::JustFinished } else { Status::Idle }
}

pub fn when(text: &str) -> String {
    match parse_rfc3339_ms(text) {
        Some(ms) if ms <= now_ms() => format!("{} ago", format_age(ms, now_ms())),
        Some(ms) => format!("in {}", format_age(now_ms(), ms)),
        None if text.is_empty() => "—".into(),
        None => text.to_owned(),
    }
}

impl App {
    pub fn set_view(&mut self, view: SideView) {
        self.side_view = view;
        self.list_sel = 0;
        self.list_scroll = 0;
        self.doc.ui.sidebar_visible = true;
        match view {
            SideView::Plugins | SideView::Mcp | SideView::Skills | SideView::Agents | SideView::Settings => {
                if self.config.is_none() && !self.config_loading {
                    self.refresh_config();
                }
            }
            SideView::Automations => self.refresh_automations(),
            _ => {}
        }
        self.set_focus(super::app::Focus::Sidebar);
        self.mark_dirty();
    }

    pub fn refresh_config(&mut self) {
        if self.config_loading {
            return;
        }
        self.config_loading = true;
        let tx = self.app_tx.clone();
        if self.demo {
            // Demo mode never runs Copilot.
            let snapshot = copilot_cfg::Snapshot { errors: vec!["demo mode: Copilot CLI settings are not loaded".into()], ..Default::default() };
            let _ = tx.send(AppEvent::Config(Box::new(snapshot)));
            return;
        }
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::Config(Box::new(copilot_cfg::snapshot())));
        });
    }

    pub fn refresh_automations(&mut self) {
        self.load_automations();
    }

    /// Runs a CLI change in the background; refreshes the config afterwards.
    pub fn run_cli_op(&mut self, label: String, op: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        self.cli_busy = Some(label.clone());
        self.notify(format!("{label}…"), false);
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = op().map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::CliDone { label, result });
        });
    }

    pub fn automation(&self, key: &str) -> Option<&Automation> {
        self.automations.iter().find(|a| a.key == key)
    }

    /// Builds the rows of the current non-session view.
    pub fn build_list(&self) -> Vec<ListRow> {
        let mut rows = Vec::new();
        let loading = |rows: &mut Vec<ListRow>| rows.push(ListRow::info("Loading from Copilot CLI…"));
        match self.side_view {
            SideView::Sessions => {}
            SideView::Automations => {
                if self.automations.is_empty() {
                    rows.push(ListRow::info(if self.automations_loading { "Loading…" } else { "No automations yet." }));
                    rows.push(ListRow::info("Create them in VS Code (Agents ›"));
                    rows.push(ListRow::info("Automations) or the Copilot app;"));
                    rows.push(ListRow::info("they appear here automatically."));
                }
                if let Some(error) = &self.automations_error {
                    rows.push(ListRow::info(format!("⚠ {error}")));
                }
                let mut families: Vec<String> = Vec::new();
                for a in &self.automations {
                    let family = if a.factory.is_some() { a.project.clone() } else { "Copilot & VS Code".to_owned() };
                    if !families.contains(&family) {
                        families.push(family);
                    }
                }
                for family in families {
                    let members: Vec<&Automation> =
                        self.automations.iter().filter(|a| if a.factory.is_some() { a.project == family } else { family == "Copilot & VS Code" }).collect();
                    let failing = members.iter().filter(|a| automation_status(a) == Status::Failed).count();
                    rows.push(ListRow::header(if failing > 0 { format!("{family} · ✗ {failing} failing") } else { family.clone() }));
                    for a in members {
                        let status = automation_status(a);
                        let right = match (status, a.factory.as_ref()) {
                            (Status::Failed, Some(live)) => live.runs.iter().find(|r| !r.ended.is_empty()).map(|r| format!("{} {}", r.exit.map(|c| format!("exit {c}")).unwrap_or_else(|| "failed".into()), when(&r.ended))).unwrap_or_else(|| "failed".into()),
                            (Status::Working, _) => "running".into(),
                            _ if !a.enabled => "disabled".into(),
                            (_, Some(_)) if !a.next_run_at.is_empty() => format!("next {}", when(&a.next_run_at)),
                            _ => a.schedule.clone(),
                        };
                        rows.push(ListRow::row(ItemRef::Automation(a.key.clone()), status.icon(0), status.color(), a.name.clone(), right));
                    }
                }
                rows.push(ListRow::header("Manifests"));
                rows.push(ListRow::row(ItemRef::AutomationsDir, "›", TEXT, "Folder", self.automations_dir().display().to_string()));
                rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
            }
            SideView::Plugins => match &self.config {
                None => loading(&mut rows),
                Some(config) => {
                    for p in &config.plugins {
                        let (icon, color) = enabled_icon(p.enabled);
                        rows.push(ListRow::row(ItemRef::Plugin(p.name.clone()), icon, color, p.name.clone(), p.version.clone()));
                    }
                    if config.plugins.is_empty() {
                        rows.push(ListRow::info("No plugins installed."));
                    }
                    rows.push(ListRow::row(ItemRef::Add(AddKind::Plugin), "+", BLUE, "Install plugin…", ""));
                    rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
                }
            },
            SideView::Mcp => match &self.config {
                None => loading(&mut rows),
                Some(config) => {
                    let mut last = String::new();
                    for s in &config.mcp {
                        if s.source != last {
                            rows.push(ListRow::header(s.source.clone()));
                            last = s.source.clone();
                        }
                        let (icon, color) = enabled_icon(s.enabled);
                        rows.push(ListRow::row(ItemRef::Mcp(s.name.clone()), icon, color, s.name.clone(), s.transport.clone()));
                    }
                    rows.push(ListRow::row(ItemRef::Add(AddKind::Mcp), "+", BLUE, "Add MCP server…", ""));
                    rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
                }
            },
            SideView::Skills => match &self.config {
                None => loading(&mut rows),
                Some(config) => {
                    let mut last = String::new();
                    for s in &config.skills {
                        if s.source != last {
                            rows.push(ListRow::header(s.source.clone()));
                            last = s.source.clone();
                        }
                        let (icon, color) = enabled_icon(s.enabled);
                        rows.push(ListRow::row(ItemRef::Skill(s.name.clone()), icon, color, s.name.clone(), ""));
                    }
                    rows.push(ListRow::row(ItemRef::Add(AddKind::Skill), "+", BLUE, "Add skill (file, folder or URL)…", ""));
                    rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
                }
            },
            SideView::Agents => match &self.config {
                None => loading(&mut rows),
                Some(config) => {
                    rows.push(ListRow::header(format!("Custom agents ({})", config.agents.len())));
                    for a in &config.agents {
                        rows.push(ListRow::row(ItemRef::Agent(a.path.clone()), "◈", PURPLE, a.name.clone(), a.source.clone()));
                    }
                    rows.push(ListRow::header(format!("Instructions ({})", config.instructions.len())));
                    for i in &config.instructions {
                        let label = if i.label.is_empty() { i.source_path.rsplit(['\\', '/']).next().unwrap_or_default().to_owned() } else { i.label.clone() };
                        rows.push(ListRow::row(ItemRef::Instruction(i.source_path.clone()), "≡", if i.default_disabled { DARK } else { TEXT }, label, i.location.clone()));
                    }
                    rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
                }
            },
            SideView::VsCode => {
                let prefs = &self.doc.vscode;
                rows.push(ListRow::row(ItemRef::VsSync, "⟳", BLUE, if self.vs_syncing { "Syncing…" } else { "Sync sessions now" }, ""));
                if self.undo.is_some() {
                    rows.push(ListRow::row(ItemRef::VsUndo, "↶", BLUE, "Undo last sync", "Ctrl+Z"));
                }
                rows.push(ListRow::row(ItemRef::VsWindow, "◷", TEXT, "Active within", format!("{} h", prefs.window_hours)));
                rows.push(ListRow::row(ItemRef::VsArchived, if prefs.include_archived { "☑" } else { "☐" }, TEXT, "Include archived", ""));
                rows.push(ListRow::row(ItemRef::VsCliSessions, if prefs.include_cli_sessions { "☑" } else { "☐" }, TEXT, "Include CLI-started sessions", ""));
                rows.push(ListRow::row(ItemRef::VsAuto, if prefs.auto_sync { "☑" } else { "☐" }, TEXT, "Sync when canvas opens", ""));
                let last = if prefs.last_sync_ms == 0 { "never".into() } else { format!("{} ago", format_age(prefs.last_sync_ms, now_ms())) };
                rows.push(ListRow::info(format!("Last sync: {last}")));
                rows.push(ListRow::info("Agents-window groups become canvas"));
                rows.push(ListRow::info("groups; others group by workspace."));
                rows.push(ListRow::header("Settings"));
                rows.push(ListRow::row(ItemRef::VsMigrate, "⇢", BLUE, "Migrate skills & settings…", ""));
            }
            SideView::Settings => {
                let (defaults, scope) = match self.settings_group.as_deref().and_then(|g| self.doc.group(g)) {
                    Some(group) => (group.defaults.clone().unwrap_or_default(), format!("Group “{}” overrides", group.name)),
                    None => (self.doc.defaults.clone(), "New-session defaults".to_owned()),
                };
                rows.push(ListRow::header(scope));
                let global = self.settings_group.is_none();
                for field in DefaultField::ALL {
                    let value = field.get(&defaults);
                    let shown = match (value.is_empty(), field, global) {
                        (true, DefaultField::Permissions, true) => "allow-all (default)".to_owned(),
                        (true, DefaultField::Permissions, false) => "(canvas default)".to_owned(),
                        (true, _, _) => "(not set)".to_owned(),
                        (false, _, _) => value,
                    };
                    rows.push(ListRow::row(ItemRef::Default(field), "›", TEXT, field.label(), shown));
                }
                if self.settings_group.is_some() {
                    rows.push(ListRow::info("Esc: back to canvas defaults"));
                }
                if global {
                    rows.push(ListRow::header("Performance"));
                    let minutes = self.doc.idle_stop_minutes;
                    let value = if minutes == 0 { "never".to_owned() } else { format!("after {minutes} min idle") };
                    rows.push(ListRow::row(ItemRef::IdleStop, "›", TEXT, "Stop idle sessions", value));
                    rows.push(ListRow::header("Automations"));
                    rows.push(ListRow::row(ItemRef::AutomationsDir, "›", TEXT, "Manifest folder", self.automations_dir().display().to_string()));
                }
                rows.push(ListRow::header("Copilot settings (copilot config)"));
                match &self.config {
                    None => loading(&mut rows),
                    Some(config) => {
                        for (key, value) in &config.settings {
                            rows.push(ListRow::row(ItemRef::Setting(key.clone()), "·", GRAY, key.clone(), value.clone()));
                        }
                        rows.push(ListRow::row(ItemRef::AddSetting, "+", BLUE, "Set another setting…", ""));
                        rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
                    }
                }
            }
        }
        if let Some(errors) = self.config.as_ref().map(|c| &c.errors).filter(|e| !e.is_empty())
            && matches!(self.side_view, SideView::Plugins | SideView::Mcp | SideView::Skills | SideView::Agents | SideView::Settings)
        {
            for error in errors {
                rows.push(ListRow::info(format!("⚠ {error}")));
            }
        }
        rows
    }

    pub fn list_move(&mut self, delta: i64) {
        let rows = self.build_list();
        if rows.is_empty() {
            return;
        }
        let mut index = self.list_sel as i64;
        for _ in 0..rows.len() {
            index = (index + delta.signum()).clamp(0, rows.len() as i64 - 1);
            let stop = rows[index as usize].selectable() || index == 0 || index == rows.len() as i64 - 1;
            if stop && (delta.abs() <= 1 || (index - self.list_sel as i64).abs() >= delta.abs()) {
                break;
            }
        }
        if rows[index as usize].selectable() {
            self.list_sel = index as usize;
        }
        self.show_list_detail();
    }

    fn show_list_detail(&mut self) {
        let rows = self.build_list();
        if let Some(row) = rows.get(self.list_sel) {
            self.right_detail = match &row.item {
                ItemRef::Automation(key) => Some(RightDetail::Automation(key.clone())),
                ItemRef::Plugin(_) | ItemRef::Mcp(_) | ItemRef::Skill(_) | ItemRef::Agent(_) | ItemRef::Instruction(_) => Some(RightDetail::Item(row.item.clone())),
                _ => self.right_detail.clone(),
            };
        }
    }

    /// Enter / click on a list row.
    pub fn list_activate(&mut self, index: usize) {
        let rows = self.build_list();
        let Some(row) = rows.get(index).cloned() else { return };
        if !row.selectable() {
            return;
        }
        self.list_sel = index;
        match row.item {
            ItemRef::Automation(key) => {
                self.right_detail = Some(RightDetail::Automation(key.clone()));
                if let Some(node) = self.doc.nodes.iter().find(|n| n.automation.as_deref() == Some(&key)).map(|n| n.id.clone()) {
                    self.selection = vec![node.clone()];
                    self.selected_group = None;
                    self.reveal_node(&node);
                }
            }
            ItemRef::Plugin(_) | ItemRef::Mcp(_) | ItemRef::Skill(_) | ItemRef::Agent(_) | ItemRef::Instruction(_) => {
                self.right_detail = Some(RightDetail::Item(row.item));
            }
            ItemRef::Refresh => {
                if self.side_view == SideView::Automations {
                    self.refresh_automations();
                } else {
                    self.refresh_config();
                }
            }
            ItemRef::Add(kind) => {
                let (title, hint, purpose) = match kind {
                    AddKind::Skill => ("Add a skill", "path to SKILL.md, a folder of skills, or an https URL", PromptPurpose::AddSkill),
                    AddKind::Mcp => ("Add an MCP server", "name -- command [args]    or    name https://url", PromptPurpose::AddMcp),
                    AddKind::Plugin => ("Install a plugin", "plugin@marketplace, owner/repo, owner/repo:path, or git URL", PromptPurpose::InstallPlugin),
                };
                self.modals.push(Modal::Prompt(Prompt { title: title.into(), hint: hint.into(), input: TextInput::default(), purpose }));
            }
            ItemRef::Default(field) => self.edit_default(field),
            ItemRef::Setting(key) => {
                let current = self.config.as_ref().and_then(|c| c.settings.iter().find(|(k, _)| *k == key)).map(|(_, v)| v.clone()).unwrap_or_default();
                self.modals.push(Modal::Prompt(Prompt {
                    title: format!("copilot config {key}"),
                    hint: "Copilot validates the value; empty removes the setting".into(),
                    input: TextInput::new(&current),
                    purpose: PromptPurpose::CopilotSetting(key),
                }));
            }
            ItemRef::AddSetting => {
                self.modals.push(Modal::Prompt(Prompt {
                    title: "Set a Copilot setting".into(),
                    hint: "key value   (see `copilot help config`, e.g. beep on)".into(),
                    input: TextInput::default(),
                    purpose: PromptPurpose::NewCopilotSetting,
                }));
            }
            ItemRef::VsSync => self.start_vs_sync(),
            ItemRef::VsWindow => self.modals.push(Modal::Prompt(Prompt {
                title: "Sync sessions active within (hours)".into(),
                hint: "e.g. 48".into(),
                input: TextInput::new(&self.doc.vscode.window_hours.to_string()),
                purpose: PromptPurpose::SyncWindow,
            })),
            ItemRef::VsArchived => {
                self.doc.vscode.include_archived = !self.doc.vscode.include_archived;
                self.mark_dirty();
            }
            ItemRef::VsCliSessions => {
                self.doc.vscode.include_cli_sessions = !self.doc.vscode.include_cli_sessions;
                self.mark_dirty();
            }
            ItemRef::VsAuto => {
                self.doc.vscode.auto_sync = !self.doc.vscode.auto_sync;
                self.mark_dirty();
            }
            ItemRef::VsMigrate => self.start_migration(),
            ItemRef::VsUndo => self.undo_last(),
            ItemRef::IdleStop => {
                let choices = super::doc::IDLE_STOP_CHOICES;
                let next = choices.iter().position(|m| *m == self.doc.idle_stop_minutes).map(|i| (i + 1) % choices.len()).unwrap_or(2);
                self.doc.idle_stop_minutes = choices[next];
                self.mark_dirty();
            }
            ItemRef::AutomationsDir => self.modals.push(Modal::Prompt(Prompt {
                title: "Automation manifest folder".into(),
                hint: "one YAML manifest per automation (see copilot-canvas automations import); empty: default".into(),
                input: TextInput::new(&self.doc.automations_dir.clone().unwrap_or_else(|| self.automations_dir().display().to_string())),
                purpose: PromptPurpose::AutomationsDir,
            })),
            ItemRef::Header | ItemRef::Info => {}
        }
        self.needs_draw = true;
    }

    /// Space: enable/disable.
    pub fn list_toggle(&mut self, index: usize) {
        let rows = self.build_list();
        let Some(row) = rows.get(index).cloned() else { return };
        let config = self.config.clone().unwrap_or_default();
        match row.item {
            ItemRef::Automation(key) => self.toggle_automation(&key),
            ItemRef::Plugin(name) => {
                let enabled = !config.plugins.iter().any(|p| p.name == name && p.enabled);
                self.run_cli_op(format!("{} plugin {name}", if enabled { "Enabling" } else { "Disabling" }), move || copilot_cfg::set_enabled(Kind::Plugin, &name, enabled));
            }
            ItemRef::Mcp(name) => {
                let enabled = !config.mcp.iter().any(|s| s.name == name && s.enabled);
                self.run_cli_op(format!("{} MCP server {name}", if enabled { "Enabling" } else { "Disabling" }), move || copilot_cfg::set_enabled(Kind::Mcp, &name, enabled));
            }
            ItemRef::Skill(name) => {
                let enabled = !config.skills.iter().any(|s| s.name == name && s.enabled);
                self.run_cli_op(format!("{} skill {name}", if enabled { "Enabling" } else { "Disabling" }), move || copilot_cfg::set_enabled(Kind::Skill, &name, enabled));
            }
            ItemRef::VsArchived | ItemRef::VsCliSessions | ItemRef::VsAuto | ItemRef::IdleStop => self.list_activate(index),
            _ => {}
        }
    }

    /// Del: remove (always confirmed).
    pub fn list_delete(&mut self, index: usize) {
        let rows = self.build_list();
        let Some(row) = rows.get(index).cloned() else { return };
        let (what, action) = match &row.item {
            ItemRef::Plugin(name) => (format!("Uninstall plugin “{name}”?"), super::dialogs::Action::RemoveConfig(Kind::Plugin, name.clone())),
            ItemRef::Mcp(name) => {
                let source = self.config.as_ref().and_then(|c| c.mcp.iter().find(|s| s.name == *name)).map(|s| s.source.clone()).unwrap_or_default();
                if !source.eq_ignore_ascii_case("user") {
                    return self.notify(format!("“{name}” comes from {source}; disable it instead (Space)."), true);
                }
                (format!("Remove MCP server “{name}” from your user config?"), super::dialogs::Action::RemoveConfig(Kind::Mcp, name.clone()))
            }
            ItemRef::Skill(name) => {
                let source = self.config.as_ref().and_then(|c| c.skills.iter().find(|s| s.name == *name)).map(|s| s.source.to_lowercase()).unwrap_or_default();
                if source.contains("plugin") || source.contains("project") || source.contains("builtin") {
                    return self.notify(format!("“{name}” comes from {source}; disable it instead (Space)."), true);
                }
                (format!("Remove skill “{name}”?"), super::dialogs::Action::RemoveConfig(Kind::Skill, name.clone()))
            }
            _ => return,
        };
        self.modals.push(Modal::Confirm(Confirm {
            title: what,
            body: vec!["Copilot CLI performs the removal; this cannot be undone here.".into()],
            buttons: vec![("Remove".into(), action), ("Cancel".into(), super::dialogs::Action::Nothing)],
            sel: 1,
        }));
    }

    pub fn toggle_automation(&mut self, key: &str) {
        let Some(automation) = self.automation(key).cloned() else { return };
        if automation.factory.is_some() {
            if !automation.toggleable {
                return self.notify("This task is not registered yet: run `copilot-canvas automations apply`.", false);
            }
            return self.toggle_task(&automation);
        }
        if !automation.toggleable {
            return self.notify("Session schedules are managed with /every inside that session.", false);
        }
        let enabled = !automation.enabled;
        let key = key.to_owned();
        let tx = self.app_tx.clone();
        let dir = (!self.demo).then(|| self.automations_dir());
        self.notify(format!("{} “{}”…", if enabled { "Enabling" } else { "Disabling" }, automation.name), false);
        std::thread::spawn(move || {
            let result = automations::set_enabled(&automations::db_path(), &key, enabled).map(|_| String::new()).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::CliDone { label: format!("{} automation", if enabled { "Enabled" } else { "Disabled" }), result });
            let factory_loaded = dir.as_ref().is_some_and(|d| d.is_dir());
            let (list, errors) = automations::load_all(&automations::db_path(), dir.as_deref());
            let _ = tx.send(AppEvent::Automations(super::factory::AutoLoad { list, errors, factory_loaded }));
        });
    }

    fn edit_default(&mut self, field: DefaultField) {
        let group = self.settings_group.clone();
        let defaults = match group.as_deref().and_then(|g| self.doc.group(g)) {
            Some(g) => g.defaults.clone().unwrap_or_default(),
            None => self.doc.defaults.clone(),
        };
        match field.choices() {
            Some(choices) => {
                let mut items: Vec<(String, String)> = match (field, group.is_some()) {
                    (DefaultField::Permissions, false) => Vec::new(),
                    (DefaultField::Permissions, true) => vec![("(not set)".into(), "use the canvas default".into())],
                    _ => vec![("(not set)".into(), "use Copilot's default".into())],
                };
                let note = |c: &str| match c {
                    "allow-all" => "--allow-all: tools, paths and URLs without asking",
                    "ask" => "Copilot asks before tools, paths and URLs",
                    _ => "",
                };
                items.extend(choices.iter().map(|c| ((*c).to_owned(), note(c).to_owned())));
                self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::Default(field, group), title: field.label().into(), items, filter: String::new(), sel: 0, scroll: 0 }));
            }
            None => {
                let hint = match field {
                    DefaultField::Model => "model id, e.g. claude-opus-5.5 or auto; empty = Copilot's default",
                    DefaultField::Agent => "custom agent name; empty = none",
                    _ => "folders separated by ;   (passed as --add-dir)",
                };
                self.modals.push(Modal::Prompt(Prompt {
                    title: field.label().into(),
                    hint: hint.into(),
                    input: TextInput::new(&field.get(&defaults)),
                    purpose: PromptPurpose::Default(field, group),
                }));
            }
        }
    }

    pub fn apply_default(&mut self, field: DefaultField, group: Option<String>, value: &str) {
        match group.as_deref().and_then(|g| self.doc.group_mut(g)) {
            Some(group) => {
                let mut defaults = group.defaults.clone().unwrap_or_default();
                field.set(&mut defaults, value);
                group.defaults = (!defaults.is_empty()).then_some(defaults);
            }
            None => field.set(&mut self.doc.defaults, value),
        }
        self.mark_dirty();
    }

    pub fn apply_prompt(&mut self, purpose: PromptPurpose, value: String) -> Result<(), String> {
        let value = value.trim().to_owned();
        match purpose {
            PromptPurpose::AddSkill => {
                if value.is_empty() {
                    return Err("enter a path or URL".into());
                }
                self.run_cli_op(format!("Adding skill {value}"), move || copilot_cfg::add_skill(&value));
            }
            PromptPurpose::AddMcp => {
                copilot_cfg::parse_mcp_add(&value).map_err(|e| e.to_string())?;
                self.run_cli_op("Adding MCP server".into(), move || copilot_cfg::add_mcp(&value));
            }
            PromptPurpose::InstallPlugin => {
                if value.is_empty() {
                    return Err("enter a plugin source".into());
                }
                // Installs can ask for confirmation, so they run in a visible terminal.
                self.open_cli_terminal("Install plugin", vec!["plugin".into(), "install".into(), value]);
            }
            PromptPurpose::CopilotSetting(key) => {
                if value.is_empty() {
                    self.run_cli_op(format!("Removing setting {key}"), move || copilot_cfg::config_remove(&key));
                } else {
                    self.run_cli_op(format!("Setting {key}"), move || copilot_cfg::config_set(&key, &value));
                }
            }
            PromptPurpose::NewCopilotSetting => {
                let Some((key, rest)) = value.split_once(char::is_whitespace) else { return Err("enter: key value".into()) };
                let (key, rest) = (key.to_owned(), rest.trim().to_owned());
                self.run_cli_op(format!("Setting {key}"), move || copilot_cfg::config_set(&key, &rest));
            }
            PromptPurpose::Default(field, group) => self.apply_default(field, group, &value),
            PromptPurpose::AutomationsDir => {
                let value = value.trim().trim_matches('"').to_owned();
                if !value.is_empty() && !std::path::Path::new(&value).is_dir() {
                    return Err(format!("{value} is not a folder"));
                }
                self.doc.automations_dir = (!value.is_empty()).then_some(value);
                self.mark_dirty();
                self.load_automations();
            }
            PromptPurpose::SyncWindow => {
                let hours: u32 = value.parse().map_err(|_| "enter a number of hours".to_owned())?;
                if hours == 0 || hours > 24 * 365 {
                    return Err("between 1 and 8760 hours".into());
                }
                self.doc.vscode.window_hours = hours;
                self.mark_dirty();
            }
            PromptPurpose::PassiveName(group, template) => {
                self.create_passive(&group, template, &value)?;
            }
            PromptPurpose::Passive(id, field) => self.apply_passive_setting(&id, field, &value)?,
            PromptPurpose::SendEvent(group) => self.send_event(&group, &value)?,
            PromptPurpose::MessagePassive(id) => self.message_passive(&id, &value)?,
            PromptPurpose::McpPreset(off) => {
                if value.is_empty() {
                    return Err("enter a preset name".into());
                }
                if [super::doc::MCP_ALL, super::doc::MCP_NONE].iter().any(|b| b.eq_ignore_ascii_case(&value)) {
                    return Err(format!("“{value}” is a built-in preset; choose another name"));
                }
                let name: String = value.chars().take(40).collect();
                self.doc.mcp.save_preset(&name, &off);
                self.mark_dirty();
                self.notify(format!("Saved MCP preset “{name}”."), false);
            }
            PromptPurpose::GroupFolder(group_id) => {
                let path = if value.is_empty() { self.suggested_group_folder(&group_id) } else { value };
                self.set_group_folder(&group_id, &path)?;
            }
        }
        Ok(())
    }

    /// MCP servers from Copilot's configuration: (name, source, enabled in the
    /// config). `None` while the configuration is loading.
    pub fn mcp_choices(&self) -> Option<Vec<(String, String, bool)>> {
        self.config.as_ref().map(|c| c.mcp.iter().map(|s| (s.name.clone(), s.source.clone(), s.enabled)).collect())
    }

    /// Names of the servers you can turn on or off (enabled in Copilot's config).
    pub fn mcp_available(&self) -> Vec<String> {
        self.mcp_choices().unwrap_or_default().into_iter().filter(|(_, _, on)| *on).map(|(name, _, _)| name).collect()
    }

    /// "12 of 19 on · preset “Investigation”" for a choice.
    pub fn mcp_summary(&self, off: &[String]) -> String {
        let Some(choices) = self.mcp_choices() else { return "loading your MCP servers…".into() };
        let known = self.mcp_available();
        if choices.is_empty() {
            return "no MCP servers configured".into();
        }
        let on = known.iter().filter(|k| !off.contains(k)).count();
        let preset = self.doc.mcp.matching(off, &known).map(|p| format!(" · preset “{p}”")).unwrap_or_default();
        format!("{on} of {} on{preset}", known.len())
    }

    /// Runs an interactive Copilot CLI command in a canvas terminal tile.
    pub fn open_cli_terminal(&mut self, title: &str, args: Vec<String>) {
        let Some(exe) = copilot_cfg::copilot_exe() else { return self.notify("copilot.exe was not found on PATH", true) };
        let id = format!("cli-{}", uuid::Uuid::new_v4());
        let (x, y) = self.doc.free_slot(self.doc.view.x + 2, self.doc.view.y + 1, 3, NodeKind::Session);
        self.doc.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Session,
            name: format!("{title}: {}", args.last().cloned().unwrap_or_default()),
            x,
            y,
            cwd: Some(self.default_cwd.clone()),
            parent: None,
            agent_key: None,
            source: Some("cli".into()),
            command: Some(std::iter::once(exe.to_string_lossy().into_owned()).chain(args).collect()),
            fresh: true,
            automation: None,
            synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
        });
        self.select_node(&id, false);
        self.launch(&id);
    }

    // ---------- group folders ----------

    pub fn suggested_group_folder(&self, group_id: &str) -> String {
        let name = self.doc.group(group_id).map(|g| g.name.clone()).unwrap_or_else(|| "group".into());
        let slug = crate::git::slug(&name).replace('/', "-");
        let slug = if slug.is_empty() { "group".to_owned() } else { slug };
        self.instance_home.join("groups").join(slug).display().to_string()
    }

    pub fn set_group_folder(&mut self, group_id: &str, path: &str) -> Result<(), String> {
        let path = crate::paths::absolute(std::path::Path::new(path.trim().trim_matches('"')));
        let Some(group) = self.doc.group_mut(group_id) else { return Err("group not found".into()) };
        group.folder = Some(path.display().to_string());
        let group = group.clone();
        super::doc::prepare_group_folders(&group).map_err(|e| format!("{e:#}"))?;
        self.mark_dirty();
        self.notify(format!("“{}” uses {} for ai-docs and human-outputs.", group.name, path.display()), false);
        Ok(())
    }

    pub fn ask_group_folder(&mut self, group_id: &str) {
        let suggestion = self.suggested_group_folder(group_id);
        let current = self.doc.group(group_id).and_then(|g| g.folder.clone());
        self.modals.push(Modal::Prompt(Prompt {
            title: format!("Folder for “{}” (ai-docs + human-outputs)", self.doc.group(group_id).map(|g| g.name.as_str()).unwrap_or("group")),
            hint: format!("Enter to use: {}   ·   Tab: browse   ·   Esc: later", current.clone().unwrap_or(suggestion)),
            input: TextInput::new(current.as_deref().unwrap_or("")),
            purpose: PromptPurpose::GroupFolder(group_id.to_owned()),
        }));
    }

    pub fn open_group_folder(&mut self, group_id: &str, outputs: bool) {
        let Some(group) = self.doc.group(group_id).cloned() else { return };
        if group.folder.is_none() {
            return self.ask_group_folder(group_id);
        }
        if let Err(error) = super::doc::prepare_group_folders(&group) {
            return self.notify(format!("{error:#}"), true);
        }
        let path = if outputs { group.outputs() } else { group.ai_docs() }.expect("folder set");
        if let Err(error) = copilot_cfg::open_in_explorer(&path) {
            self.notify(format!("{error:#}"), true);
        }
    }

    /// Group the session belongs to (geometric membership).
    pub fn group_for_session(&self, id: &str) -> Option<Group> {
        let node = self.doc.node(id)?;
        self.doc.group_of(node).cloned()
    }
}

/// Copilot arguments and environment variables added to a launch.
pub type LaunchExtras = (Vec<String>, Vec<(String, String)>);

impl App {
    /// Extra Copilot arguments and environment for launching `id`.
    pub fn launch_extras(&self, id: &str, fresh: bool) -> Result<LaunchExtras, String> {
        let group = self.group_for_session(id);
        let defaults = self.doc.defaults.merged(group.as_ref().and_then(|g| g.defaults.as_ref()));
        // Permissions apply to every run (they are not saved with the session).
        let mut args = defaults.permission_args();
        if fresh {
            args.extend(defaults.args());
        }
        // The session's MCP server choice applies to every run (resumes too).
        if let Some(off) = self.doc.node(id).and_then(|n| n.mcp_off.as_ref()) {
            args.extend(super::doc::mcp_off_args(off));
        }
        let mut env = Vec::new();
        // Events: the canvas skills, this session's identity and the copilot-canvas command.
        if let Some(instance) = self.instance.as_ref() {
            let plugin = instance.home.join("plugin");
            if plugin.is_dir() {
                args.push("--plugin-dir".into());
                args.push(plugin.display().to_string());
            }
            env.push(("COPILOT_CANVAS_HOME".into(), instance.home.display().to_string()));
            env.push(("COPILOT_CANVAS_SESSION".into(), id.to_owned()));
            env.push(("PATH".into(), format!("{};{}", instance.home.join("cli").display(), std::env::var("PATH").unwrap_or_default())));
        }
        if let Some(group) = group.filter(|g| g.folder.is_some()) {
            let instructions = super::doc::prepare_group_folders(&group).map_err(|e| format!("{e:#}"))?.expect("folder set");
            for dir in [group.ai_docs(), group.outputs()].into_iter().flatten() {
                args.push("--add-dir".into());
                args.push(dir.display().to_string());
            }
            let mut dirs = std::env::var("COPILOT_CUSTOM_INSTRUCTIONS_DIRS").unwrap_or_default();
            if !dirs.is_empty() {
                dirs.push(',');
            }
            dirs.push_str(&instructions.display().to_string());
            env.push(("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(), dirs));
        }
        Ok((args, env))
    }

    // ---------- automations on the canvas ----------

    /// Keeps one box per automation inside a dedicated "Automations" group.
    pub fn sync_automation_nodes(&mut self) {
        let keys: Vec<String> = self.automations.iter().map(|a| a.key.clone()).collect();
        let before = self.doc.nodes.len();
        let factory_ok = self.automations_factory_ok;
        self.doc.nodes.retain(|n| {
            n.kind != NodeKind::Automation || n.automation.as_ref().is_some_and(|k| keys.contains(k) || (!factory_ok && k.starts_with("task:")))
        });
        let mut changed = before != self.doc.nodes.len();
        let missing: Vec<Automation> =
            self.automations.iter().filter(|a| !self.doc.nodes.iter().any(|n| n.automation.as_deref() == Some(&a.key))).cloned().collect();
        if !missing.is_empty() {
            let group_id = match self.doc.groups.iter().find(|g| g.is_automations()).map(|g| g.id.clone()) {
                Some(id) => id,
                None => {
                    let (x, y) = match self.doc.bounds() {
                        Some(b) => (b.x, b.y + b.h + 4),
                        None => (self.doc.view.x + 2, self.doc.view.y + 1),
                    };
                    let id = format!("g-automations-{}", uuid::Uuid::new_v4());
                    self.doc.groups.push(Group {
                        id: id.clone(),
                        name: "Automations".into(),
                        x,
                        y,
                        w: 3 * GRID_X + 4,
                        h: GRID_Y + 3,
                        color: 3,
                        folder: None,
                        source: Some("automations".into()),
                        defaults: None,
                    });
                    id
                }
            };
            let (tasks, others): (Vec<Automation>, Vec<Automation>) = missing.into_iter().partition(|a| a.factory.is_some());
            if !tasks.is_empty() {
                self.place_task_boxes(&group_id, tasks.into_iter().map(|a| (a.key.clone(), a.name.clone(), a.project.clone())).collect());
            }
            self.add_automation_boxes(&group_id, others.into_iter().map(|a| (a.key.clone(), a.name.clone())).collect());
            changed = true;
        }
        if self.automations.iter().any(|a| a.factory.is_some())
            && let Some(group_id) = self.doc.groups.iter().find(|g| g.is_automations()).map(|g| g.id.clone())
        {
            self.ensure_doctor(&group_id);
        }
        if changed {
            self.mark_dirty();
        }
    }

    fn add_automation_boxes(&mut self, group_id: &str, seeds: Vec<(String, String)>) {
        for (key, name) in seeds {
            let (_, (x, y)) = self.slot_or_overflow(group_id);
            self.doc.nodes.push(Node {
                id: format!("auto-{}", uuid::Uuid::new_v4()),
                kind: NodeKind::Automation,
                name,
                x,
                y,
                cwd: None,
                parent: None,
                agent_key: None,
                source: Some("automation".into()),
                command: None,
                fresh: false,
                automation: Some(key),
                synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
            });
        }
    }

    /// Free slot inside a group, growing the group when it is full. `None`
    /// when growing would cover another group or a box outside it (membership
    /// is geometric, so that would silently regroup things).
    pub fn slot_in_group(&mut self, group_id: &str) -> Option<(i32, i32)> {
        let group = self.doc.group(group_id).cloned()?;
        let rect = group.rect();
        let columns = ((group.w - 4) / GRID_X).max(1);
        for columns in [columns, columns + 1] {
            let (x, y) = self.doc.free_slot(group.x + 2, group.y + 2, columns, NodeKind::Session);
            let needed = Rect { x: x - 2, y: y - 2, w: SESSION_W + 4, h: SESSION_H + 3 };
            let grown = rect.union(&needed);
            let covers_group = self.doc.groups.iter().any(|g| g.id != group.id && !rect.contains_rect(&g.rect()) && g.rect().intersects(&grown));
            let covers_box = self.doc.nodes.iter().any(|n| {
                let (cx, cy) = n.rect().center();
                grown.contains(cx, cy) && !rect.contains(cx, cy)
            });
            if !covers_group && !covers_box {
                if let Some(group) = self.doc.group_mut(group_id) {
                    group.w = grown.w;
                    group.h = grown.h;
                }
                return Some((x, y));
            }
        }
        None
    }

    /// A slot in the group, or in its continuation group ("Name (2)") when the
    /// group cannot grow without covering its neighbours.
    pub fn slot_or_overflow(&mut self, group_id: &str) -> (String, (i32, i32)) {
        if let Some(slot) = self.slot_in_group(group_id) {
            return (group_id.to_owned(), slot);
        }
        let Some(group) = self.doc.group(group_id).cloned() else { return (group_id.to_owned(), (0, 0)) };
        let base = group.source.clone().unwrap_or_else(|| format!("group:{}", group.id));
        let base = base.split('#').next().unwrap_or_default().to_owned();
        let siblings: Vec<String> =
            self.doc.groups.iter().filter(|g| g.source.as_deref().is_some_and(|s| s.starts_with(&format!("{base}#")))).map(|g| g.id.clone()).collect();
        for sibling in &siblings {
            if let Some(slot) = self.slot_in_group(sibling) {
                return (sibling.clone(), slot);
            }
        }
        let primary = self.doc.groups.iter().find(|g| g.source.as_deref() == Some(base.as_str())).map(|g| g.name.clone()).unwrap_or(group.name.clone());
        let name = format!("{primary} ({})", siblings.len() + 2);
        let id = self.new_group_in_free_space(&name, &format!("{base}#{}", siblings.len() + 2), GRID_X * 2 + 4, GRID_Y + 3);
        if let Some(g) = self.doc.group_mut(&id) {
            g.color = group.color;
        }
        let slot = self.slot_in_group(&id).unwrap_or((0, 0));
        (id, slot)
    }

    // ---------- VS Code sync ----------

    pub fn start_vs_sync(&mut self) {
        if self.demo {
            return self.notify("VS Code sync is unavailable in the demo.", false);
        }
        if self.vs_syncing {
            return;
        }
        self.vs_syncing = true;
        self.send(crate::protocol::ClientMsg::RefreshSessions);
        self.notify("Syncing VS Code agent sessions…", false);
        let created: Vec<crate::vscode::VsSession> = self
            .sessions
            .values()
            .filter(|s| s.client.starts_with("vscode"))
            .map(|s| crate::vscode::VsSession {
                session_id: s.id.clone(),
                label: String::new(),
                archived: false,
                cwd: (!s.cwd.is_empty()).then(|| s.cwd.clone()),
                created_ms: Some(s.created_ms),
                active_ms: Some(s.updated_ms.max(s.last_event_ms)),
            })
            .collect();
        let since = now_ms().saturating_sub(self.doc.vscode.window_hours as u64 * 3_600_000);
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let (workspaces, mut warnings) = crate::vscode::scan(&crate::vscode::default_roots());
            let (groups, group_warnings) = crate::vscode::agent_groups(&crate::vscode::default_user_dirs());
            warnings.extend(group_warnings);
            let grouped: std::collections::HashSet<String> = groups.iter().flat_map(|g| g.sessions.iter().cloned()).collect();
            let (containers, container_warnings) = crate::vscode::read_containers(&crate::vscode::default_container_roots(), since, &grouped);
            warnings.extend(container_warnings);
            let candidates = crate::vscode::sync_candidates(&workspaces, &groups, &containers, &created);
            let _ = tx.send(AppEvent::VsSync(candidates, warnings));
        });
    }

    pub fn apply_vs_sync(&mut self, candidates: Vec<crate::vscode::SyncCandidate>, warnings: Vec<String>) {
        self.vs_syncing = false;
        // Sync only ever adds boxes, sub-session links and names; it never moves
        // existing boxes or removes groups. A snapshot makes it undoable.
        let before = self.doc.clone();
        let window_ms = self.doc.vscode.window_hours as u64 * 3_600_000;
        let now = now_ms();
        let (mut added, mut inactive, mut archived, mut missing, mut present, mut from_cli) = (0, 0, 0, 0, 0, 0);
        let (mut linked, mut renamed) = (0, 0);
        let mut renames: Vec<(String, String)> = Vec::new();
        // (group key, group name, seeds) in discovery order; sub-sessions follow their parent.
        let mut buckets: Vec<(String, String, Vec<Seed>)> = Vec::new();
        let mut links: Vec<(String, String)> = Vec::new();
        let mut accepted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for candidate in &candidates {
            let session = &candidate.session;
            let in_agents_group = candidate.group_key.starts_with("vscode-group:");
            if let Some(parent) = &candidate.parent {
                links.push((session.session_id.clone(), parent.clone()));
            }
            if let Some(node) = self.doc.node(&session.session_id) {
                present += 1;
                // Take VS Code's title unless the box was renamed on the canvas.
                let title = session.label.trim();
                if !title.is_empty() && node.name != title {
                    let copilot_name = self.sessions.get(&node.id).map(|s| s.name.trim());
                    let untouched = node.synced_name.as_deref() == Some(node.name.as_str())
                        || copilot_name == Some(node.name.trim())
                        || node.name.trim().is_empty();
                    if untouched {
                        renames.push((node.id.clone(), title.to_owned()));
                    }
                }
                continue;
            }
            let Some(summary) = self.sessions.get(&session.session_id) else {
                missing += 1;
                continue;
            };
            // Sub-sessions follow their parent's decision.
            let parent_ok = candidate.parent.as_ref().is_some_and(|p| accepted.contains(p) || self.doc.node(p).is_some());
            if !parent_ok {
                // VS Code lists every local Copilot CLI session; sync the ones created in VS Code
                // and anything the user filed into an Agents-window group.
                let created_in_vscode = candidate.from_vscode || summary.client.starts_with("vscode");
                if !in_agents_group && !created_in_vscode && !self.doc.vscode.include_cli_sessions {
                    from_cli += 1;
                    continue;
                }
                if session.archived && !self.doc.vscode.include_archived {
                    archived += 1;
                    continue;
                }
                let active = session.active_ms.unwrap_or(0).max(summary.updated_ms);
                if now.saturating_sub(active) > window_ms {
                    inactive += 1;
                    continue;
                }
            }
            accepted.insert(session.session_id.clone());
            let seed = Seed {
                id: session.session_id.clone(),
                name: if session.label.is_empty() { summary.name.clone() } else { session.label.clone() },
                cwd: session.cwd.clone().unwrap_or_else(|| summary.cwd.clone()),
                source: Some("vscode".into()),
            };
            added += 1;
            match buckets.iter_mut().find(|(k, _, _)| *k == candidate.group_key) {
                Some((_, _, seeds)) => seeds.push(seed),
                None => buckets.push((candidate.group_key.clone(), candidate.group_name.clone(), vec![seed])),
            }
        }
        for (_, _, seeds) in &mut buckets {
            // Parents first so a sub-session lands right after its parent.
            let parent_of: std::collections::HashMap<&str, &str> = links.iter().map(|(c, p)| (c.as_str(), p.as_str())).collect();
            let order: Vec<String> = seeds.iter().map(|s| s.id.clone()).collect();
            let rank = |id: &str| -> (usize, usize) {
                let root = parent_of.get(id).copied().filter(|p| order.iter().any(|o| o == p)).unwrap_or(id);
                (order.iter().position(|o| o == root).unwrap_or(usize::MAX), usize::from(root != id))
            };
            seeds.sort_by_key(|s| rank(&s.id));
        }
        let mut first = None;
        for (key, name, seeds) in buckets {
            let group_id = self.ensure_vs_group(&key, &name, seeds.len());
            for seed in seeds {
                first.get_or_insert_with(|| seed.id.clone());
                let (_, (x, y)) = self.slot_or_overflow(&group_id);
                self.push_seed(seed, x, y);
            }
        }
        for (id, title) in renames {
            if let Some(node) = self.doc.nodes.iter_mut().find(|n| n.id == id) {
                node.name = title.clone();
                node.synced_name = Some(title);
                renamed += 1;
            }
        }
        // Sub-sessions: link to the parent box (new or existing) when both are on the canvas.
        for (child, parent) in links {
            if self.doc.node(&parent).is_none() {
                continue;
            }
            if let Some(node) = self.doc.nodes.iter_mut().find(|n| n.id == child && n.kind == NodeKind::Session)
                && node.parent.is_none()
            {
                node.parent = Some(parent);
                linked += 1;
            }
        }
        self.doc.vscode.last_sync_ms = now;
        self.sync_agents();
        let changed = added + renamed + linked > 0;
        if changed {
            if let Err(error) = self.store.backup(&before, "before-vscode-sync") {
                self.notify(format!("Could not back up the canvas: {error:#}"), true);
            }
            self.undo = Some(("VS Code sync".into(), Box::new(before)));
        }
        if let Some(first) = first {
            self.select_node(&first, false);
            self.fit_all();
        }
        self.mark_dirty();
        let mut message = format!("VS Code sync: added {added} session(s)");
        let inactive_text = format!("inactive > {} h", self.doc.vscode.window_hours);
        for (count, text) in [
            (renamed, "renamed to VS Code titles"),
            (linked, "sub-session links"),
            (present, "already on the canvas"),
            (inactive, inactive_text.as_str()),
            (archived, "archived"),
            (missing, "not on this machine"),
            (from_cli, "started in Copilot CLI"),
        ] {
            if count > 0 {
                message.push_str(&format!("; {count} {text}"));
            }
        }
        if !warnings.is_empty() {
            message.push_str(&format!("; {} warning(s)", warnings.len()));
        }
        if changed {
            message.push_str(" · Ctrl+Z undoes it");
        }
        self.notify(message, false);
    }

    /// Restores the canvas from before the last bulk change (VS Code sync).
    pub fn undo_last(&mut self) {
        let Some((label, doc)) = self.undo.take() else {
            return self.notify("Nothing to undo.", false);
        };
        let _ = self.store.backup(&self.doc, "before-undo");
        // Keep view and preferences that changed meanwhile; restore the layout.
        let (view, ui, vscode) = (self.doc.view.clone(), self.doc.ui.clone(), self.doc.vscode.clone());
        self.doc = *doc;
        self.doc.view = view;
        self.doc.ui = ui;
        self.doc.vscode = vscode;
        self.selection.retain(|id| self.doc.node(id).is_some());
        self.selected_group = self.selected_group.take().filter(|g| self.doc.group(g).is_some());
        self.mark_dirty();
        self.notify(format!("Undid the last {label}."), false);
    }

    /// The canvas group synced from VS Code under `key`; created (sized for
    /// `count` boxes, in free space right of everything) when missing.
    fn ensure_vs_group(&mut self, key: &str, name: &str, count: usize) -> String {
        if let Some(group) = self.doc.groups.iter().find(|g| g.source.as_deref() == Some(key)) {
            return group.id.clone();
        }
        let cols = ((count as f64).sqrt().ceil() as i32).clamp(1, 6);
        let rows = (count as i32 + cols - 1) / cols;
        self.new_group_in_free_space(name, key, cols * GRID_X + 4, rows * GRID_Y + 3)
    }

    fn new_group_in_free_space(&mut self, name: &str, source: &str, w: i32, h: i32) -> String {
        let (x, y) = self.doc.bounds().map(|b| (b.x + b.w + 8, b.y)).unwrap_or((self.doc.view.x + 3, self.doc.view.y + 2));
        let id = format!("g-{}", uuid::Uuid::new_v4());
        let color = (self.doc.groups.len() % 6) as u8;
        self.doc.groups.push(Group { id: id.clone(), name: name.to_owned(), x, y, w, h, color, folder: None, source: Some(source.to_owned()), defaults: None });
        id
    }

    fn push_seed(&mut self, seed: Seed, x: i32, y: i32) {
        let synced_name = (seed.source.as_deref() == Some("vscode")).then(|| seed.name.clone());
        self.doc.nodes.push(Node {
            id: seed.id,
            kind: NodeKind::Session,
            name: seed.name,
            x,
            y,
            cwd: (!seed.cwd.is_empty()).then_some(seed.cwd),
            parent: None,
            agent_key: None,
            source: seed.source,
            command: None,
            fresh: false,
            automation: None,
            synced_name,
            reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
        });
    }

    // ---------- migration ----------

    pub fn start_migration(&mut self) {
        self.modals.push(Modal::Migrate(super::dialogs::Migrate { items: None, chosen: Vec::new(), sel: 0, applying: false, result: None }));
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let snapshot = copilot_cfg::snapshot();
            let items = crate::migrate::plan(&crate::migrate::vscode_user_dirs(), &copilot_cfg::copilot_home(), &snapshot.mcp, &snapshot.skills);
            let _ = tx.send(AppEvent::MigratePlan(items));
        });
    }

    pub fn apply_migration(&mut self, items: Vec<crate::migrate::Item>) {
        let tx = self.app_tx.clone();
        let backups = self.instance_home.join("backups");
        std::thread::spawn(move || {
            let refs: Vec<&crate::migrate::Item> = items.iter().collect();
            let runner = |args: &[String]| -> anyhow::Result<String> {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                copilot_cfg::run_cli(&refs, Duration::from_secs(120))
            };
            let result = crate::migrate::apply(&refs, &copilot_cfg::copilot_home(), &backups, &runner)
                .map(|(backup, outcomes)| {
                    let mut lines = vec![format!("Backup: {}", backup.display())];
                    for outcome in outcomes {
                        lines.push(match outcome.result {
                            Ok(_) => format!("✓ {}", outcome.name),
                            Err(error) => format!("✗ {} — {error}", outcome.name),
                        });
                    }
                    lines
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::MigrateDone(result));
        });
    }

    pub fn automation_db(&self) -> PathBuf {
        automations::db_path()
    }

    pub fn tick_side(&mut self) {
        if self.automations_at.elapsed() >= Duration::from_secs(15) && !self.demo {
            self.refresh_automations();
        }
        if !self.auto_synced && self.doc.vscode.auto_sync && self.sessions_loaded && !self.demo {
            self.auto_synced = true;
            self.start_vs_sync();
        }
    }
}

pub fn status_color(status: &str) -> Color {
    let s = status.to_ascii_lowercase();
    if s.starts_with("fail") || s == "error" {
        NEGATIVE
    } else if s == "running" || s == "pending" {
        POSITIVE
    } else if s == "completed" || s == "succeeded" {
        BLUE
    } else {
        ATTENTION
    }
}

#[cfg(test)]
mod glyph_tests {
    /// Glyphs must be one cell wide in every width table in the chain (the
    /// canvas, Windows' console layer, the terminal). Unicode 16 widened the
    /// trigram symbols (U+2630..) — a mismatch shifts whole rows on screen.
    #[test]
    fn activity_bar_icons_are_single_width_everywhere() {
        for (_, icon, _) in super::VIEWS {
            let c = icon.chars().next().unwrap();
            assert_eq!(unicode_width::UnicodeWidthStr::width(icon), 1, "{icon}");
            let cp = c as u32;
            assert!(!(0x2630..=0x2637).contains(&cp) && !(0x268A..=0x268F).contains(&cp) && !(0x4DC0..=0x4DFF).contains(&cp), "{icon} changed width in Unicode 16");
        }
    }
}
