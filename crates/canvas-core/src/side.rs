//! Activity-bar views beyond Sessions: Automations, Plugins, MCP servers,
//! Skills, Agents & instructions, VS Code, and Settings — plus the features
//! they drive (automation boxes, VS Code sync, group folders, launch defaults).

use std::path::PathBuf;
use std::time::Duration;

use ratatui::style::Color;

use crate::agents::{AgentKind, agent_of};
use crate::app::{App, AppEvent, Seed};
use crate::automation::Automation;
use crate::config::{self, Kind};
use crate::dialogs::{Confirm, Modal, Pick, PickPurpose, Prompt, PromptPurpose, TextInput};
use crate::doc::{CONTEXTS, EFFORTS, GRID_X, GRID_Y, Group, LaunchDefaults, MODES, Node, NodeKind, Rect, SESSION_H, SESSION_W};
use crate::paint::{BLUE, DARK, GRAY, POSITIVE, MUTED, PURPLE, NEGATIVE, TEXT, ATTENTION};
use crate::paths::{format_age, now_ms, parse_rfc3339_ms};
use crate::rpc::Backend;
use crate::status::Status;

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
    /// The model of new sessions of one agent (ids differ per agent).
    Model(AgentKind),
    Effort,
    Mode,
    Agent,
    Context,
    AddDirs,
    Permissions,
}

impl DefaultField {
    pub const ALL: [DefaultField; 9] = [
        DefaultField::Permissions,
        DefaultField::Model(AgentKind::Copilot),
        DefaultField::Model(AgentKind::OpenCode),
        DefaultField::Model(AgentKind::Claude),
        DefaultField::Effort,
        DefaultField::Mode,
        DefaultField::Agent,
        DefaultField::Context,
        DefaultField::AddDirs,
    ];
    pub fn label(self) -> &'static str {
        match self {
            DefaultField::Model(AgentKind::Copilot) => "Copilot model",
            DefaultField::Model(AgentKind::OpenCode) => "OpenCode model",
            DefaultField::Model(AgentKind::Claude) => "Claude Code model",
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
            DefaultField::Model(kind) => d.model(kind).unwrap_or_default().to_owned(),
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
            DefaultField::Model(kind) => d.set_model(kind, opt.as_deref()),
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
            DefaultField::Permissions => Some(&crate::doc::PERMISSIONS),
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
    /// Which agent's configuration the views show (Enter or A switches).
    ConfigAgent,
    /// The canvas's agent for new sessions.
    DefaultAgent,
    /// Dust and effects settings.
    Visual(crate::visual::VisualSetting),
    /// The value of the row above on a line of its own (shown by its end when long).
    Detail,
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
    fn detail(label: impl Into<String>) -> Self {
        ListRow { item: ItemRef::Detail, label: label.into(), right: String::new(), icon: "", color: TEXT }
    }
    fn row(item: ItemRef, icon: &'static str, color: Color, label: impl Into<String>, right: impl Into<String>) -> Self {
        ListRow { item, label: label.into(), right: right.into(), icon, color }
    }
    pub fn selectable(&self) -> bool {
        !matches!(self.item, ItemRef::Header | ItemRef::Info | ItemRef::Detail)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RightDetail {
    Automation(String),
    Item(ItemRef),
    /// A passive session's settings and state (node id).
    Passive(String),
    /// A session or subagent box that is not running here: what it is and
    /// what starting it would do (node id).
    Session(String),
}

fn enabled_icon(enabled: bool) -> (&'static str, Color) {
    if enabled { ("●", POSITIVE) } else { ("○", DARK) }
}

pub fn automation_status(automation: &Automation) -> Status {
    if let Some(status) = crate::factory_client::task_status(automation) {
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
    let recent = parse_rfc3339_ms(&automation.last_run_at).is_some_and(|t| now_ms().saturating_sub(t) < crate::status::RECENT_MS);
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
                if self.config.is_none() {
                    self.refresh_config();
                }
            }
            SideView::Automations => self.refresh_automations(),
            _ => {}
        }
        self.set_focus(crate::app::Focus::Sidebar);
        self.mark_dirty();
    }

    /// Reloads the configuration the views show (`config_agent`'s).
    pub fn refresh_config(&mut self) {
        self.refresh_config_for(self.config_agent);
    }

    /// Loads an agent's configuration in the background.
    pub fn refresh_config_for(&mut self, kind: AgentKind) {
        if !self.configs_loading.insert(kind) {
            return;
        }
        if self.demo {
            // Demo mode never runs an agent CLI.
            let snapshot = config::Snapshot { errors: vec![format!("demo mode: {} settings are not loaded", kind.label())], ..Default::default() };
            let _ = self.app_tx.send(AppEvent::Config(kind, Box::new(snapshot)));
            return;
        }
        self.in_background(
            move |backend| backend.config(kind).unwrap_or_else(|e| config::Snapshot { errors: vec![format!("{e:#}")], ..Default::default() }),
            move |snapshot| AppEvent::Config(kind, Box::new(snapshot)),
        );
    }

    /// A configuration arrived.
    pub fn on_config(&mut self, kind: AgentKind, snapshot: config::Snapshot) {
        self.configs_loading.remove(&kind);
        if kind == self.config_agent {
            self.config = Some(snapshot.clone());
        }
        self.agent_configs.insert(kind, snapshot);
    }

    /// Shows the next agent's configuration in the views.
    pub fn cycle_config_agent(&mut self) {
        let index = crate::agents::ALL.iter().position(|k| *k == self.config_agent).unwrap_or(0);
        self.config_agent = crate::agents::ALL[(index + 1) % crate::agents::ALL.len()];
        self.config = self.agent_configs.get(&self.config_agent).cloned();
        self.right_detail = None;
        if self.config.is_none() {
            self.refresh_config();
        }
        self.needs_draw = true;
    }

    /// Whether `config_agent` supports a change; tells you when it does not.
    fn supported(&mut self, ok: bool) -> bool {
        if !ok {
            self.notify(format!("Not supported for {}.", self.config_agent.label()), true);
        }
        ok
    }

    pub fn refresh_automations(&mut self) {
        self.load_automations();
    }

    /// Runs a CLI change in the background; refreshes the config afterwards.
    pub fn run_cli_op(&mut self, label: String, op: impl FnOnce(&dyn Backend) -> anyhow::Result<String> + Send + 'static) {
        self.cli_busy = Some(label.clone());
        self.notify(format!("{label}…"), false);
        self.in_background(move |backend| op(backend).map_err(|e| format!("{e:#}")), move |result| AppEvent::CliDone { label, result });
    }

    pub fn automation(&self, key: &str) -> Option<&Automation> {
        self.automations.iter().find(|a| a.key == key)
    }

    /// Builds the rows of the current non-session view.
    pub fn build_list(&self) -> Vec<ListRow> {
        let mut rows = Vec::new();
        let label = self.config_agent.label();
        let loading = |rows: &mut Vec<ListRow>| rows.push(ListRow::info(format!("Loading from {label}…")));
        if matches!(self.side_view, SideView::Plugins | SideView::Mcp | SideView::Skills | SideView::Agents) {
            // Which agent's configuration this is; Enter or A shows the next one.
            rows.push(ListRow::row(ItemRef::ConfigAgent, "⇄", BLUE, format!("Agent: {label}"), "A: switch"));
        }
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
                rows.push(ListRow::row(ItemRef::AutomationsDir, "›", TEXT, "Folder", self.automations_folder.clone()));
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
                if matches!(self.undo, Some(crate::app::Undo::Doc(..))) {
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
                if global {
                    let agent = match self.doc.default_agent.as_deref().and_then(AgentKind::parse) {
                        Some(kind) => kind.label().to_owned(),
                        None => format!("Auto (detected: {})", self.detected_agent.label()),
                    };
                    rows.push(ListRow::row(ItemRef::DefaultAgent, "›", TEXT, "Agent for new sessions", agent));
                }
                for field in DefaultField::ALL {
                    if let DefaultField::Model(kind) = field {
                        // The id on a line of its own: ids are long.
                        let (source, id) = crate::models::settings_value(defaults.model(kind), self.doc.defaults.model(kind), kind, global);
                        rows.push(ListRow::row(ItemRef::Default(field), "›", TEXT, field.label(), source));
                        rows.push(ListRow::detail(id));
                        continue;
                    }
                    let value = field.get(&defaults);
                    let shown = match (value.is_empty(), field, global) {
                        (true, DefaultField::Permissions, true) => "allow-all (default)".to_owned(),
                        (true, DefaultField::Permissions, false) => "(canvas default)".to_owned(),
                        (true, _, _) => "(not set)".to_owned(),
                        (false, _, _) => value,
                    };
                    rows.push(ListRow::row(ItemRef::Default(field), "›", TEXT, field.label(), shown));
                }
                rows.push(ListRow::info("Effort: Copilot, Claude Code"));
                rows.push(ListRow::info("Mode, context tier: Copilot"));
                if global {
                    rows.push(ListRow::info("A model picked in the new-session"));
                    rows.push(ListRow::info("dialog becomes its agent's default"));
                }
                if self.settings_group.is_some() {
                    rows.push(ListRow::info("Esc: back to canvas defaults"));
                }
                if global {
                    rows.push(ListRow::header("Performance"));
                    let minutes = self.doc.idle_stop_minutes;
                    let value = if minutes == 0 { "never".to_owned() } else { format!("after {minutes} min idle") };
                    rows.push(ListRow::row(ItemRef::IdleStop, "›", TEXT, "Stop idle sessions", value));
                    rows.push(ListRow::header("Canvas"));
                    rows.extend(self.visual_setting_rows());
                    rows.push(ListRow::header("Automations"));
                    rows.push(ListRow::row(ItemRef::AutomationsDir, "›", TEXT, "Manifest folder", self.automations_folder.clone()));
                }
                rows.push(ListRow::header(if self.config_agent == AgentKind::Copilot { "Copilot settings (copilot config)".to_owned() } else { format!("{label} settings (read-only)") }));
                rows.push(ListRow::row(ItemRef::ConfigAgent, "⇄", BLUE, format!("Agent: {label}"), "A: switch"));
                match &self.config {
                    None => loading(&mut rows),
                    Some(config) => {
                        for (key, value) in &config.settings {
                            rows.push(ListRow::row(ItemRef::Setting(key.clone()), "·", GRAY, key.clone(), value.clone()));
                        }
                        if self.config_agent == AgentKind::Copilot {
                            rows.push(ListRow::row(ItemRef::AddSetting, "+", BLUE, "Set another setting…", ""));
                        }
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
                // Adding and installing go through Copilot's CLI only.
                if !self.supported(self.config_agent == AgentKind::Copilot) {
                    return;
                }
                let (title, hint, purpose) = match kind {
                    AddKind::Skill => ("Add a skill", "path to SKILL.md, a folder of skills, or an https URL", PromptPurpose::AddSkill),
                    AddKind::Mcp => ("Add an MCP server", "name -- command [args]    or    name https://url", PromptPurpose::AddMcp),
                    AddKind::Plugin => ("Install a plugin", "plugin@marketplace, owner/repo, owner/repo:path, or git URL", PromptPurpose::InstallPlugin),
                };
                self.modals.push(Modal::Prompt(Prompt { title: title.into(), hint: hint.into(), input: TextInput::default(), purpose }));
            }
            ItemRef::Default(field) => self.edit_default(field),
            ItemRef::Setting(key) => {
                if !self.supported(self.config_agent == AgentKind::Copilot) {
                    return;
                }
                let current = self.config.as_ref().and_then(|c| c.settings.iter().find(|(k, _)| *k == key)).map(|(_, v)| v.clone()).unwrap_or_default();
                self.modals.push(Modal::Prompt(Prompt {
                    title: format!("copilot config {key}"),
                    hint: "Copilot validates the value; empty removes the setting".into(),
                    input: TextInput::new(&current),
                    purpose: PromptPurpose::CopilotSetting(key),
                }));
            }
            ItemRef::AddSetting => {
                if !self.supported(self.config_agent == AgentKind::Copilot) {
                    return;
                }
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
                let choices = crate::doc::IDLE_STOP_CHOICES;
                let next = choices.iter().position(|m| *m == self.doc.idle_stop_minutes).map(|i| (i + 1) % choices.len()).unwrap_or(2);
                self.doc.idle_stop_minutes = choices[next];
                self.mark_dirty();
            }
            ItemRef::AutomationsDir => self.modals.push(Modal::Prompt(Prompt {
                title: "Automation manifest folder".into(),
                hint: "one YAML manifest per automation (see copilot-canvas automations import); empty: default".into(),
                input: TextInput::new(&self.doc.automations_dir.clone().unwrap_or_else(|| self.automations_folder.clone())),
                purpose: PromptPurpose::AutomationsDir,
            })),
            ItemRef::ConfigAgent => self.cycle_config_agent(),
            ItemRef::DefaultAgent => {
                // Auto → Copilot → OpenCode → Claude Code → Auto.
                let current = self.doc.default_agent.as_deref().and_then(AgentKind::parse);
                let next = match current {
                    None => Some(crate::agents::ALL[0]),
                    Some(kind) => crate::agents::ALL.iter().position(|k| *k == kind).and_then(|i| crate::agents::ALL.get(i + 1)).copied(),
                };
                self.doc.default_agent = next.map(|k| k.id().to_owned());
                let agent = self.default_agent();
                if !self.installed_agents.contains(&agent) {
                    self.notify(format!("{} was not found on this machine; new sessions with it cannot start.", agent.label()), true);
                }
                // The views follow the default agent.
                self.config_agent = agent;
                self.config = self.agent_configs.get(&agent).cloned();
                if self.config.is_none() {
                    self.refresh_config();
                }
                self.mark_dirty();
            }
            ItemRef::Visual(setting) => self.visual_setting(setting),
            ItemRef::Header | ItemRef::Info | ItemRef::Detail => {}
        }
        self.needs_draw = true;
    }

    /// Space: enable/disable.
    pub fn list_toggle(&mut self, index: usize) {
        let rows = self.build_list();
        let Some(row) = rows.get(index).cloned() else { return };
        let config = self.config.clone().unwrap_or_default();
        let enabled_now = |kind: Kind, name: &str| match kind {
            Kind::Plugin => config.plugins.iter().any(|p| p.name == name && p.enabled),
            Kind::Mcp => config.mcp.iter().any(|s| s.name == name && s.enabled),
            Kind::Skill => config.skills.iter().any(|s| s.name == name && s.enabled),
        };
        match row.item {
            ItemRef::Automation(key) => self.toggle_automation(&key),
            ItemRef::Plugin(name) => self.set_config_enabled(Kind::Plugin, name.clone(), !enabled_now(Kind::Plugin, &name)),
            ItemRef::Mcp(name) => self.set_config_enabled(Kind::Mcp, name.clone(), !enabled_now(Kind::Mcp, &name)),
            ItemRef::Skill(name) => self.set_config_enabled(Kind::Skill, name.clone(), !enabled_now(Kind::Skill, &name)),
            ItemRef::VsArchived | ItemRef::VsCliSessions | ItemRef::VsAuto | ItemRef::IdleStop | ItemRef::ConfigAgent | ItemRef::DefaultAgent | ItemRef::Visual(_) => self.list_activate(index),
            _ => {}
        }
    }

    /// Turns a plugin, MCP server or skill of `config_agent` on or off.
    pub fn set_config_enabled(&mut self, what: Kind, name: String, enabled: bool) {
        let agent = self.config_agent;
        if !self.supported(config::caps_for_changes(agent, what).0) {
            return;
        }
        let noun = match what {
            Kind::Plugin => "plugin",
            Kind::Mcp => "MCP server",
            Kind::Skill => "skill",
        };
        self.run_cli_op(format!("{} {noun} {name}", if enabled { "Enabling" } else { "Disabling" }), move |backend| backend.config_set_enabled(agent, what, &name, enabled));
    }

    /// Removes a plugin, MCP server or skill of `config_agent`.
    pub fn remove_config(&mut self, what: Kind, name: String) {
        let agent = self.config_agent;
        if !self.supported(config::caps_for_changes(agent, what).1) {
            return;
        }
        self.run_cli_op(format!("Removing {name}"), move |backend| backend.config_remove(agent, what, &name));
    }

    /// Del: remove (always confirmed).
    pub fn list_delete(&mut self, index: usize) {
        let rows = self.build_list();
        let Some(row) = rows.get(index).cloned() else { return };
        let what = match &row.item {
            ItemRef::Plugin(_) => Kind::Plugin,
            ItemRef::Mcp(_) => Kind::Mcp,
            ItemRef::Skill(_) => Kind::Skill,
            _ => return,
        };
        if !self.supported(config::caps_for_changes(self.config_agent, what).1) {
            return;
        }
        let (what, action) = match &row.item {
            ItemRef::Plugin(name) => (format!("Uninstall plugin “{name}”?"), crate::dialogs::Action::RemoveConfig(Kind::Plugin, name.clone())),
            ItemRef::Mcp(name) => {
                let source = self.config.as_ref().and_then(|c| c.mcp.iter().find(|s| s.name == *name)).map(|s| s.source.clone()).unwrap_or_default();
                if !source.eq_ignore_ascii_case("user") {
                    return self.notify(format!("“{name}” comes from {source}; disable it instead (Space)."), true);
                }
                (format!("Remove MCP server “{name}” from your user config?"), crate::dialogs::Action::RemoveConfig(Kind::Mcp, name.clone()))
            }
            ItemRef::Skill(name) => {
                let source = self.config.as_ref().and_then(|c| c.skills.iter().find(|s| s.name == *name)).map(|s| s.source.to_lowercase()).unwrap_or_default();
                if source.contains("plugin") || source.contains("project") || source.contains("builtin") {
                    return self.notify(format!("“{name}” comes from {source}; disable it instead (Space)."), true);
                }
                (format!("Remove skill “{name}”?"), crate::dialogs::Action::RemoveConfig(Kind::Skill, name.clone()))
            }
            _ => return,
        };
        self.modals.push(Modal::Confirm(Confirm {
            title: what,
            body: vec![format!("{} performs the removal; this cannot be undone here.", self.config_agent.label())],
            buttons: vec![("Remove".into(), action), ("Cancel".into(), crate::dialogs::Action::Nothing)],
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
        let (backend, tx) = (self.backend.clone(), self.app_tx.clone());
        let (dir, tasks) = (self.doc.automations_dir.clone(), !self.demo);
        self.notify(format!("{} “{}”…", if enabled { "Enabling" } else { "Disabling" }, automation.name), false);
        std::thread::spawn(move || {
            let result = backend.automation_set_enabled(&key, enabled).map(|_| String::new()).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::CliDone { label: format!("{} automation", if enabled { "Enabled" } else { "Disabled" }), result });
            let _ = tx.send(crate::factory_client::loaded(backend.automations(dir, tasks)));
        });
    }

    fn edit_default(&mut self, field: DefaultField) {
        let group = self.settings_group.clone();
        if let DefaultField::Model(kind) = field {
            return self.open_model_picker(crate::dialogs::ModelTarget::Default(kind, group));
        }
        let defaults = match group.as_deref().and_then(|g| self.doc.group(g)) {
            Some(g) => g.defaults.clone().unwrap_or_default(),
            None => self.doc.defaults.clone(),
        };
        match field.choices() {
            Some(choices) => {
                let mut items: Vec<(String, String)> = match (field, group.is_some()) {
                    (DefaultField::Permissions, false) => Vec::new(),
                    (DefaultField::Permissions, true) => vec![("(not set)".into(), "use the canvas default".into())],
                    _ => vec![("(not set)".into(), "use the agent's default".into())],
                };
                let note = |c: &str| match c {
                    "allow-all" => "tools, paths and URLs without asking (every agent)",
                    "ask" => "the agent asks before tools, paths and URLs",
                    _ => "",
                };
                items.extend(choices.iter().map(|c| ((*c).to_owned(), note(c).to_owned())));
                self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::Default(field, group), title: field.label().into(), items, filter: String::new(), sel: 0, scroll: 0 }));
            }
            None => {
                let hint = match field {
                    DefaultField::Agent => "custom agent name; empty = none",
                    _ => "folders separated by ;   (the session may use them)",
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
                self.run_cli_op(format!("Adding skill {value}"), move |backend| backend.skill_add(&value));
            }
            PromptPurpose::AddMcp => {
                config::parse_mcp_add(&value).map_err(|e| e.to_string())?;
                self.run_cli_op("Adding MCP server".into(), move |backend| backend.mcp_add(&value));
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
                    self.run_cli_op(format!("Removing setting {key}"), move |backend| backend.config_unset(&key));
                } else {
                    self.run_cli_op(format!("Setting {key}"), move |backend| backend.config_set(&key, &value));
                }
            }
            PromptPurpose::NewCopilotSetting => {
                let Some((key, rest)) = value.split_once(char::is_whitespace) else { return Err("enter: key value".into()) };
                let (key, rest) = (key.to_owned(), rest.trim().to_owned());
                self.run_cli_op(format!("Setting {key}"), move |backend| backend.config_set(&key, &rest));
            }
            PromptPurpose::Default(field, group) => self.apply_default(field, group, &value),
            PromptPurpose::Model(target) => self.apply_model_choice(target, &value)?,
            PromptPurpose::AutomationsDir => {
                let value = value.trim().trim_matches('"').to_owned();
                if !value.is_empty() && !self.backend.path_info(&value).map_err(|e| format!("{e:#}"))?.dir {
                    return Err(format!("{value} is not a folder"));
                }
                self.doc.automations_dir = (!value.is_empty()).then_some(value);
                if let Ok(folder) = self.backend.automations_dir(self.doc.automations_dir.clone()) {
                    self.automations_folder = folder;
                }
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
                if [crate::doc::MCP_ALL, crate::doc::MCP_NONE].iter().any(|b| b.eq_ignore_ascii_case(&value)) {
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

    /// MCP servers from an agent's configuration: (name, source, enabled in
    /// the config). `None` while the configuration is loading.
    pub fn mcp_choices(&self, agent: AgentKind) -> Option<Vec<(String, String, bool)>> {
        self.agent_configs.get(&agent).map(|c| c.mcp.iter().map(|s| (s.name.clone(), s.source.clone(), s.enabled)).collect())
    }

    /// Names of the servers you can turn on or off (enabled in the agent's config).
    pub fn mcp_available(&self, agent: AgentKind) -> Vec<String> {
        self.mcp_choices(agent).unwrap_or_default().into_iter().filter(|(_, _, on)| *on).map(|(name, _, _)| name).collect()
    }

    /// "12 of 19 on · preset “Investigation”" for a choice.
    pub fn mcp_summary(&self, agent: AgentKind, off: &[String]) -> String {
        let Some(choices) = self.mcp_choices(agent) else { return format!("loading {}'s MCP servers…", agent.label()) };
        let known = self.mcp_available(agent);
        if choices.is_empty() {
            return "no MCP servers configured".into();
        }
        let on = known.iter().filter(|k| !off.contains(k)).count();
        let preset = self.doc.mcp.matching(off, &known).map(|p| format!(" · preset “{p}”")).unwrap_or_default();
        format!("{on} of {} on{preset}", known.len())
    }

    /// Runs an interactive Copilot CLI command in a canvas terminal tile.
    pub fn open_cli_terminal(&mut self, title: &str, args: Vec<String>) {
        let name = format!("{title}: {}", args.last().cloned().unwrap_or_default());
        let command = match self.backend.cli_command(args) {
            Ok(command) => command,
            Err(error) => return self.notify(format!("{error:#}"), true),
        };
        let id = format!("cli-{}", uuid::Uuid::new_v4());
        let (x, y) = self.doc.free_slot(self.doc.view.x + 2, self.doc.view.y + 1, 3, NodeKind::Session);
        self.doc.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Session,
            name,
            x,
            y,
            cwd: Some(self.default_cwd.clone()),
            parent: None,
            agent_key: None,
            source: Some("cli".into()),
            command: Some(command),
            fresh: true,
            automation: None,
            synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, stacked: false, model: None,
        });
        self.select_node(&id, false);
        self.launch(&id);
    }

    // ---------- group folders ----------

    /// `<backend home>\groups\<slug of the group name>`.
    pub fn suggested_group_folder(&self, group_id: &str) -> String {
        let name = self.doc.group(group_id).map(|g| g.name.clone()).unwrap_or_else(|| "group".into());
        let slug = crate::git::slug(&name).replace('/', "-");
        let slug = if slug.is_empty() { "group".to_owned() } else { slug };
        PathBuf::from(&self.info.home).join("groups").join(slug).display().to_string()
    }

    pub fn set_group_folder(&mut self, group_id: &str, path: &str) -> Result<(), String> {
        let path = self.backend.path_info(path.trim().trim_matches('"')).map_err(|e| format!("{e:#}"))?.absolute;
        let Some(group) = self.doc.group_mut(group_id) else { return Err("group not found".into()) };
        group.folder = Some(path.clone());
        let group = group.clone();
        self.backend.prepare_group(&group).map_err(|e| format!("{e:#}"))?;
        self.mark_dirty();
        self.notify(format!("“{}” uses {path} for ai-docs and human-outputs.", group.name), false);
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
        if let Err(error) = self.backend.prepare_group(&group) {
            return self.notify(format!("{error:#}"), true);
        }
        let path = if outputs { group.outputs() } else { group.ai_docs() }.expect("folder set");
        self.open_folder(&path.display().to_string());
    }

    /// Group the session belongs to (geometric membership).
    pub fn group_for_session(&self, id: &str) -> Option<Group> {
        let node = self.doc.node(id)?;
        self.doc.group_of(node).cloned()
    }
}

/// What to launch for an agent session box. The service adds the canvas
/// integration (skills plugin, identity, `copilot-canvas` on PATH) and turns
/// it into the agent's command line.
///
/// Launch defaults (canvas, overridden by the box's group) apply to new
/// conversations only, and only those the agent supports; permissions and the
/// box's MCP choice apply to every run. The model: a new conversation gets the
/// box's own (chosen in the new-session dialog), else the effective default
/// ([`canvas_model::models::effective`]); a resume or fork carries the model
/// recorded on the box (never the current default), which the provider passes
/// only where its agent does not keep it itself.
pub fn agent_launch(
    node: &Node,
    canvas: &LaunchDefaults,
    group: Option<&Group>,
    prompt: Option<String>,
    fork_from: Option<String>,
    resume: bool,
) -> crate::protocol::AgentLaunch {
    let kind = agent_of(node);
    let caps = kind.caps();
    let defaults = canvas.merged(group.and_then(|g| g.defaults.as_ref()));
    let fresh = |value: &Option<String>, supported: bool| value.clone().filter(|v| !resume && supported && !v.trim().is_empty());
    let model = if resume || fork_from.is_some() {
        node.model.clone()
    } else {
        node.model.clone().or_else(|| Some(canvas_model::models::effective(kind, canvas, group).model))
    };
    let group = group.filter(|g| g.folder.is_some());
    crate::protocol::AgentLaunch {
        agent: kind.id().to_owned(),
        native_id: node.agent_session.clone().filter(|s| !s.is_empty()),
        resume,
        fork_from,
        name: Some(node.name.clone()).filter(|n| !n.trim().is_empty()),
        prompt: prompt.filter(|p| !p.trim().is_empty()),
        model: model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()),
        effort: fresh(&defaults.reasoning_effort, caps.effort),
        mode: fresh(&defaults.mode, caps.modes),
        custom_agent: fresh(&defaults.agent, true),
        context: fresh(&defaults.context, kind == AgentKind::Copilot),
        allow_all: defaults.permissions.as_deref() != Some("ask"),
        add_dirs: if resume { Vec::new() } else { defaults.add_dirs.iter().filter(|d| !d.trim().is_empty()).cloned().collect() },
        mcp_off: node.mcp_off.clone().unwrap_or_default(),
        group_folder: group.and_then(|g| g.folder.clone()),
        group_name: group.map(|g| g.name.clone()),
        headless_prompt: None,
        env: Vec::new(),
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;

    fn node(agent: Option<&str>) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": "12345678-1234-1234-1234-123456789abc", "kind": "session", "name": "Fix it", "x": 0, "y": 0 })).unwrap();
        node.agent = agent.map(str::to_owned);
        node
    }

    fn defaults() -> LaunchDefaults {
        let mut defaults = LaunchDefaults {
            reasoning_effort: Some("high".into()),
            mode: Some("plan".into()),
            agent: Some("reviewer".into()),
            context: Some("long_context".into()),
            add_dirs: vec![r"C:\shared".into()],
            ..Default::default()
        };
        for kind in crate::agents::ALL {
            defaults.set_model(kind, Some("m1"));
        }
        defaults
    }

    #[test]
    fn new_sessions_get_the_defaults_their_agent_supports() {
        let copilot = agent_launch(&node(None), &defaults(), None, Some("hello".into()), None, false);
        assert_eq!(copilot.agent, "copilot");
        assert_eq!((copilot.name.as_deref(), copilot.prompt.as_deref(), copilot.resume, copilot.native_id.as_deref()), (Some("Fix it"), Some("hello"), false, None));
        assert_eq!((copilot.model.as_deref(), copilot.effort.as_deref(), copilot.mode.as_deref(), copilot.context.as_deref()), (Some("m1"), Some("high"), Some("plan"), Some("long_context")));
        assert_eq!((copilot.custom_agent.as_deref(), copilot.add_dirs.as_slice()), (Some("reviewer"), [r"C:\shared".to_owned()].as_slice()));
        assert!(copilot.allow_all, "unset permissions mean allow-all");
        assert!(copilot.headless_prompt.is_none() && copilot.env.is_empty(), "the service adds the integration");

        let claude = agent_launch(&node(Some("claude")), &defaults(), None, None, None, false);
        assert_eq!(claude.agent, "claude");
        assert_eq!((claude.model.as_deref(), claude.effort.as_deref(), claude.mode, claude.context), (Some("m1"), Some("high"), None, None), "no start mode or context tier");

        let opencode = agent_launch(&node(Some("opencode")), &defaults(), None, None, None, false);
        assert_eq!(opencode.agent, "opencode");
        assert_eq!((opencode.model.as_deref(), opencode.effort, opencode.mode, opencode.context), (Some("m1"), None, None, None));
        assert_eq!(opencode.custom_agent.as_deref(), Some("reviewer"));
    }

    #[test]
    fn new_sessions_start_with_opus_5_5_until_a_model_is_chosen() {
        for kind in crate::agents::ALL {
            let new = agent_launch(&node(Some(kind.id())), &LaunchDefaults::default(), None, None, None, false);
            assert_eq!(new.model.as_deref(), Some(canvas_model::models::initial(kind)), "{kind:?}");
        }
        assert_eq!(agent_launch(&node(Some("opencode")), &LaunchDefaults::default(), None, None, None, false).model.as_deref(), Some("github-copilot/claude-opus-5.5"));
        // The box's own choice (the new-session dialog) wins over every default; empty is the agent's own default.
        let mut chosen = node(Some("claude"));
        chosen.model = Some("sonnet".into());
        assert_eq!(agent_launch(&chosen, &defaults(), None, None, None, false).model.as_deref(), Some("sonnet"));
        chosen.model = Some(String::new());
        assert_eq!(agent_launch(&chosen, &defaults(), None, None, None, false).model, None);
    }

    #[test]
    fn resumes_never_get_the_current_default() {
        let mut later = LaunchDefaults::default();
        for kind in crate::agents::ALL {
            later.set_model(kind, Some("new-default"));
        }
        for kind in crate::agents::ALL {
            let mut box_ = node(Some(kind.id()));
            box_.agent_session = (kind == AgentKind::OpenCode).then(|| "ses_abc".into());
            assert_eq!(agent_launch(&box_, &later, None, None, None, true).model, None, "{kind:?}: not recorded, so nothing");
            box_.model = Some("old-model".into());
            assert_eq!(agent_launch(&box_, &later, None, None, None, true).model.as_deref(), Some("old-model"), "{kind:?}: its recorded model");
        }
        // A fork continues its parent's conversation: the parent's model (recorded on the fork's box at creation).
        let mut fork = node(Some("claude"));
        fork.model = Some("claude-opus-5-5".into());
        assert_eq!(agent_launch(&fork, &later, None, None, Some("87654321-4321-4321-4321-cba987654321".into()), false).model.as_deref(), Some("claude-opus-5-5"));
        fork.model = None;
        assert_eq!(agent_launch(&fork, &later, None, None, Some("87654321-4321-4321-4321-cba987654321".into()), false).model, None);
    }

    #[test]
    fn resumes_keep_permissions_and_mcp_but_not_new_session_defaults() {
        let mut box_ = node(Some("opencode"));
        box_.agent_session = Some("ses_abc".into());
        box_.mcp_off = Some(vec!["github".into()]);
        let asking = LaunchDefaults { permissions: Some("ask".into()), ..defaults() };
        let resumed = agent_launch(&box_, &asking, None, None, None, true);
        assert_eq!((resumed.resume, resumed.native_id.as_deref()), (true, Some("ses_abc")));
        assert!(resumed.model.is_none() && resumed.effort.is_none() && resumed.custom_agent.is_none() && resumed.add_dirs.is_empty());
        assert!(!resumed.allow_all, "ask applies to every run");
        assert_eq!(resumed.mcp_off, ["github"], "the box's MCP choice applies to resumes too");
        let mut empty = LaunchDefaults::default();
        empty.set_model(AgentKind::Copilot, Some("  "));
        assert!(agent_launch(&node(None), &empty, None, Some(" ".into()), None, false).model.is_none() && agent_launch(&node(None), &empty, None, Some(" ".into()), None, false).prompt.is_none());
    }

    #[test]
    fn groups_override_defaults_and_pass_their_folder() {
        let mut overrides = LaunchDefaults { permissions: Some("ask".into()), add_dirs: vec![r"D:\docs".into()], ..Default::default() };
        overrides.set_model(AgentKind::Claude, Some("group-model"));
        let group = Group { id: "g".into(), name: "Team".into(), folder: Some(r"C:\team".into()), defaults: Some(overrides), ..Default::default() };
        let launch = agent_launch(&node(Some("claude")), &defaults(), Some(&group), None, None, false);
        assert_eq!((launch.model.as_deref(), launch.allow_all), (Some("group-model"), false));
        assert_eq!(launch.add_dirs, [r"C:\shared", r"D:\docs"]);
        assert_eq!((launch.group_folder.as_deref(), launch.group_name.as_deref()), (Some(r"C:\team"), Some("Team")));
        assert_eq!(agent_launch(&node(Some("opencode")), &defaults(), Some(&group), None, None, false).model.as_deref(), Some("m1"), "no override for OpenCode");
        let mut chosen = node(Some("claude"));
        chosen.model = Some("haiku".into());
        assert_eq!(agent_launch(&chosen, &defaults(), Some(&group), None, None, false).model.as_deref(), Some("haiku"), "a per-session choice wins over the group");
        let no_folder = Group { folder: None, ..group };
        let launch = agent_launch(&node(None), &defaults(), Some(&no_folder), None, None, false);
        assert_eq!((launch.group_folder, launch.group_name), (None, None), "a group without a folder has nothing to share");
    }

    #[test]
    fn native_forks_name_their_parent() {
        let fork = agent_launch(&node(Some("claude")), &defaults(), None, Some("go on".into()), Some("87654321-4321-4321-4321-cba987654321".into()), false);
        assert_eq!((fork.fork_from.as_deref(), fork.resume, fork.prompt.as_deref()), (Some("87654321-4321-4321-4321-cba987654321"), false, Some("go on")));
        let fork = agent_launch(&node(Some("opencode")), &defaults(), None, None, Some("ses_parent".into()), false);
        assert_eq!((fork.agent.as_str(), fork.fork_from.as_deref()), ("opencode", Some("ses_parent")));
    }
}

impl App {
    // ---------- automations on the canvas ----------

    /// An id derived from `key` (so windows creating the same box at the same
    /// time create one), or a random one when the canvas already uses it.
    pub fn shared_id(&self, prefix: &str, key: &str) -> String {
        let id = crate::sync::stable_id(prefix, key);
        if self.doc.node(&id).is_some() || self.doc.group(&id).is_some() { format!("{prefix}-{}", uuid::Uuid::new_v4()) } else { id }
    }

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
                    // Windows adding it at the same time add the same group.
                    let id = self.shared_id("g-automations", "automations");
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
            let id = self.shared_id("auto", &key);
            self.doc.nodes.push(Node {
                id,
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
                synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, stacked: false, model: None,
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
        self.in_background(
            move |backend| backend.vscode_scan(created, since),
            |scan| match scan {
                Ok((candidates, warnings)) => AppEvent::VsSync(candidates, warnings),
                Err(error) => AppEvent::VsSync(Vec::new(), vec![format!("{error:#}")]),
            },
        );
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
                agent: None,
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
            self.backup_doc(before.clone(), "before-vscode-sync", true);
            self.undo = Some(crate::app::Undo::Doc("VS Code sync".into(), Box::new(before)));
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

    /// Restores the canvas from before the last bulk change (VS Code sync) or
    /// the boxes of the last removal.
    pub fn undo_last(&mut self) {
        let (label, doc) = match self.undo.take() {
            None => return self.notify("Nothing to undo.", false),
            Some(crate::app::Undo::Removed { nodes, links }) => return self.restore_removed(nodes, links),
            Some(crate::app::Undo::Doc(label, doc)) => (label, doc),
        };
        self.backup_doc(self.doc.clone(), "before-undo", false);
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
            reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: seed.agent, agent_session: None, last_used_ms: None, stacked: false, model: None,
        });
    }

    // ---------- migration ----------

    pub fn start_migration(&mut self) {
        self.modals.push(Modal::Migrate(crate::dialogs::Migrate { items: None, chosen: Vec::new(), sel: 0, applying: false, result: None }));
        self.in_background(
            |backend| backend.migrate_plan(),
            |plan| match plan {
                Ok(items) => AppEvent::MigratePlan(items),
                Err(error) => AppEvent::MigrateDone(Err(format!("{error:#}"))),
            },
        );
    }

    pub fn apply_migration(&mut self, items: Vec<crate::migrate::Item>) {
        self.in_background(move |backend| backend.migrate_apply(items).map_err(|e| format!("{e:#}")), AppEvent::MigrateDone);
    }

    /// Copilot's automation database on the backend's machine.
    pub fn automation_db(&self) -> PathBuf {
        PathBuf::from(&self.info.automation_db)
    }

    pub fn tick_side(&mut self) {
        if self.automations_at.elapsed() >= Duration::from_secs(15) && !self.demo {
            self.refresh_automations();
        }
        if !self.auto_synced && self.doc.vscode.auto_sync && self.sessions_loaded && !self.demo {
            self.auto_synced = true;
            self.start_vs_sync();
        }
        self.refresh_tails();
    }

    /// Keeps the log tails of the automation info page fresh (read on the
    /// backend's machine, every 2 s while the page shows).
    fn refresh_tails(&mut self) {
        let Some(RightDetail::Automation(key)) = &self.right_detail else { return };
        let Some(live) = self.automation(key).and_then(|a| a.factory.as_ref()) else { return };
        let mut files = vec![(live.log_path.display().to_string(), 4096)];
        files.extend(live.logs.first().map(|log| (log.display().to_string(), 2048)));
        let missing = files.iter().any(|(path, _)| !self.tails.contains_key(path));
        if self.tails_loading || (!missing && self.tails_at.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.tails_loading = true;
        self.tails_at = std::time::Instant::now();
        let paths: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
        self.in_background(
            move |backend| backend.tails(files).unwrap_or_default(),
            move |tails| AppEvent::Tails(paths.into_iter().zip(tails.into_iter().chain(std::iter::repeat(String::new()))).collect()),
        );
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
