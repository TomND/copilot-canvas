//! The persisted canvas: session boxes, subagent boxes, group rectangles, view
//! and UI preferences. Group membership is geometric (a node belongs to the
//! smallest group containing its center), like Obsidian canvas groups.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;

pub const SESSION_W: i32 = 30;
pub const SESSION_H: i32 = 4;
pub const AGENT_W: i32 = 24;
pub const AGENT_H: i32 = 3;
pub const GROUP_MIN_W: i32 = 20;
pub const GROUP_MIN_H: i32 = 6;
pub const GRID_X: i32 = 34;
pub const GRID_Y: i32 = 7;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Session,
    Agent,
    Automation,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Parent session for subagents and subsessions.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub agent_key: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Program for demo/benchmark nodes instead of Copilot.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// True until the first launch of a newly created session.
    #[serde(default)]
    pub fresh: bool,
    /// Automation key (`wf:<id>` or `session:<id>`) for automation boxes.
    #[serde(default)]
    pub automation: Option<String>,
    /// Name last given by VS Code sync; while the box still has it, the next
    /// sync may update it (a name you type on the canvas is never replaced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_name: Option<String>,
    /// When you last looked at this session (opened, clicked or typed in it).
    /// A turn that finished after this shows as "done · review".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_ms: Option<u64>,
    /// MCP servers this session starts without (`--disable-mcp-server`, every
    /// launch including resumes); `None` = all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_off: Option<Vec<String>>,
    /// A passive session: runs on events and schedules for its group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passive: Option<Passive>,
    /// Do not type messages from passive sessions into this session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_messages: bool,
    /// Agent CLI of a session: "copilot", "opencode" or "claude" (`None`: Copilot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The agent's own session id when it differs from `id` (OpenCode names
    /// its sessions itself, on the first prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
    /// When the session was last used: started, typed into, or agent activity
    /// (looking at it does not count). Unused sessions are drawn dusty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ms: Option<u64>,
    /// Its subsessions (recursively) are drawn as a deck beneath it. They keep
    /// their own positions (which move with it), so unstacking restores them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stacked: bool,
    /// The model its conversation started with, as its agent names it (empty:
    /// the agent's own default; `None`: not recorded). Resumes keep it; a new
    /// default never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Passive session settings (the service keeps its run state).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Passive {
    pub key: String,
    pub group: String,
    pub slug: String,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub subscribe: Vec<String>,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub every_minutes: u32,
    #[serde(default)]
    pub daily_at: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default = "default_rollover")]
    pub rollover_runs: u32,
    #[serde(default = "default_runs_per_day")]
    pub max_runs_per_day: u32,
    /// Earlier conversations (before each rollover), oldest first.
    #[serde(default)]
    pub episodes: Vec<String>,
}

fn default_rollover() -> u32 {
    20
}

fn default_runs_per_day() -> u32 {
    24
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub color: u8,
    /// Root folder holding `ai-docs` and `human-outputs`.
    #[serde(default)]
    pub folder: Option<String>,
    /// Origin of a synced group (`vscode-group:…`, `vscode-ws:…`) or `automations`.
    #[serde(default)]
    pub source: Option<String>,
    /// Session defaults overriding the canvas defaults for sessions in this group.
    #[serde(default)]
    pub defaults: Option<LaunchDefaults>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct View {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub zoom: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UiPrefs {
    pub sidebar_width: u16,
    pub sidebar_visible: bool,
    /// Percent of the non-sidebar width given to terminals.
    pub terminal_percent: u16,
    pub tiled: bool,
    #[serde(default)]
    pub collapsed: Vec<String>,
    /// Subsession boxes are hidden on this client's canvas (shown again in place).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_subsessions: bool,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self { sidebar_width: 30, sidebar_visible: true, terminal_percent: 55, tiled: false, collapsed: Vec::new(), hide_subsessions: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RecentFolder {
    pub path: String,
    pub used_ms: u64,
}

/// Settings for new sessions: the canvas's (`Doc::defaults`) or a group's
/// overrides (`Group::defaults`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(from = "RawLaunchDefaults")]
pub struct LaunchDefaults {
    /// Model per agent id ("copilot", "opencode", "claude"); an empty id is
    /// the agent's own default (no model flag). On the canvas: the remembered
    /// choice (unset: [`crate::models::initial`]); in a group: its override.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
    pub reasoning_effort: Option<String>,
    /// interactive | plan | autopilot
    pub mode: Option<String>,
    pub agent: Option<String>,
    /// default | long_context
    pub context: Option<String>,
    pub add_dirs: Vec<String>,
    /// allow-all | ask. Unset means allow-all (applies to every launch, resumed too).
    pub permissions: Option<String>,
}

/// [`LaunchDefaults`] as stored, including the single `model` of canvases
/// from before models were kept per agent.
#[derive(Deserialize, Default)]
struct RawLaunchDefaults {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    add_dirs: Vec<String>,
    #[serde(default)]
    permissions: Option<String>,
}

impl From<RawLaunchDefaults> for LaunchDefaults {
    /// The old single model moves to the agents whose ids it fits
    /// ([`crate::models::legacy_agents`]); per-agent entries win.
    fn from(raw: RawLaunchDefaults) -> Self {
        let mut models = raw.models;
        if let Some(model) = raw.model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()) {
            for kind in crate::models::legacy_agents(&model) {
                models.entry(kind.id().to_owned()).or_insert_with(|| model.clone());
            }
        }
        LaunchDefaults {
            models,
            reasoning_effort: raw.reasoning_effort,
            mode: raw.mode,
            agent: raw.agent,
            context: raw.context,
            add_dirs: raw.add_dirs,
            permissions: raw.permissions,
        }
    }
}

pub const PERMISSIONS: [&str; 2] = ["allow-all", "ask"];
pub const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const MODES: [&str; 3] = ["interactive", "plan", "autopilot"];
pub const CONTEXTS: [&str; 2] = ["default", "long_context"];

impl LaunchDefaults {
    /// `self` overridden field-by-field by `over` (models per agent); folders are combined.
    pub fn merged(&self, over: Option<&LaunchDefaults>) -> LaunchDefaults {
        let Some(over) = over else { return self.clone() };
        let pick = |a: &Option<String>, b: &Option<String>| b.clone().filter(|v| !v.is_empty()).or_else(|| a.clone());
        let mut add_dirs = self.add_dirs.clone();
        for dir in &over.add_dirs {
            if !add_dirs.iter().any(|d| d.eq_ignore_ascii_case(dir)) {
                add_dirs.push(dir.clone());
            }
        }
        let mut models = self.models.clone();
        models.extend(over.models.iter().map(|(k, v)| (k.clone(), v.clone())));
        LaunchDefaults {
            models,
            reasoning_effort: pick(&self.reasoning_effort, &over.reasoning_effort),
            mode: pick(&self.mode, &over.mode),
            agent: pick(&self.agent, &over.agent),
            context: pick(&self.context, &over.context),
            add_dirs,
            permissions: pick(&self.permissions, &over.permissions),
        }
    }

    /// The model set for `kind` (`Some("")`: the agent's own default; `None`: not set).
    pub fn model(&self, kind: AgentKind) -> Option<&str> {
        self.models.get(kind.id()).map(String::as_str)
    }

    /// Sets the model for `kind` (`None` unsets it).
    pub fn set_model(&mut self, kind: AgentKind, model: Option<&str>) {
        match model {
            Some(model) => self.models.insert(kind.id().to_owned(), model.trim().to_owned()),
            None => self.models.remove(kind.id()),
        };
    }

    /// Copilot's permission flag for every launch: `--allow-all` unless set to ask.
    pub fn permission_args(&self) -> Vec<String> {
        if self.permissions.as_deref() == Some("ask") { Vec::new() } else { vec!["--allow-all".to_owned()] }
    }

    /// Copilot CLI arguments for these defaults (models are per agent: see [`Self::summary`]).
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut flag = |name: &str, value: &Option<String>| {
            if let Some(value) = value.as_ref().filter(|v| !v.trim().is_empty()) {
                args.push(name.to_owned());
                args.push(value.trim().to_owned());
            }
        };
        flag("--reasoning-effort", &self.reasoning_effort);
        flag("--mode", &self.mode);
        flag("--agent", &self.agent);
        flag("--context", &self.context);
        for dir in self.add_dirs.iter().filter(|d| !d.trim().is_empty()) {
            args.push("--add-dir".into());
            args.push(dir.trim().to_owned());
        }
        args
    }

    /// One line for panels: the models per agent, then the other flags.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = crate::agent::ALL
            .iter()
            .filter_map(|kind| self.model(*kind).map(|m| format!("{} {}", kind.label(), if m.is_empty() { "(its default)" } else { m })))
            .collect();
        let args = self.args().join(" ");
        if !args.is_empty() {
            parts.push(args);
        }
        parts.join(" · ")
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.args().is_empty() && self.permissions.is_none()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VsPrefs {
    /// Only sessions active within this many hours are synced.
    pub window_hours: u32,
    pub include_archived: bool,
    /// Also sync sessions started in Copilot CLI that VS Code merely lists
    /// (sessions in an Agents-window group are always synced).
    #[serde(default)]
    pub include_cli_sessions: bool,
    pub auto_sync: bool,
    #[serde(default)]
    pub last_sync_ms: u64,
}

impl Default for VsPrefs {
    fn default() -> Self {
        Self { window_hours: 48, include_archived: false, include_cli_sessions: false, auto_sync: false, last_sync_ms: 0 }
    }
}

pub const AI_DOCS: &str = "ai-docs";
pub const HUMAN_OUTPUTS: &str = "human-outputs";

impl Group {
    pub fn ai_docs(&self) -> Option<PathBuf> {
        self.folder.as_ref().map(|f| PathBuf::from(f).join(AI_DOCS))
    }
    pub fn outputs(&self) -> Option<PathBuf> {
        self.folder.as_ref().map(|f| PathBuf::from(f).join(HUMAN_OUTPUTS))
    }
    pub fn is_automations(&self) -> bool {
        self.source.as_deref().is_some_and(|s| s == "automations" || s.starts_with("automations#"))
    }
}

/// Creates the group's folders and the instructions file Copilot loads through
/// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`. Returns the instructions directory.
pub fn prepare_group_folders(group: &Group) -> Result<Option<PathBuf>> {
    let (Some(folder), Some(docs), Some(outputs)) = (group.folder.as_ref(), group.ai_docs(), group.outputs()) else { return Ok(None) };
    std::fs::create_dir_all(&docs).with_context(|| format!("cannot create {}", docs.display()))?;
    std::fs::create_dir_all(&outputs).with_context(|| format!("cannot create {}", outputs.display()))?;
    let instructions = PathBuf::from(folder).join(".canvas");
    std::fs::create_dir_all(&instructions)?;
    let text = format!(
        "---\napplyTo: '**'\n---\n# Canvas group: {name}\n\nThis Copilot session belongs to the \"{name}\" group in Copilot Canvas.\n\n\
         - Save AI-oriented material (research notes, plans, investigation logs, context for other agents) in:\n  `{docs}`\n\
         - Save deliverables meant for people (reports, summaries, final documents) in:\n  `{outputs}`\n\
         - Before starting substantial work, check `{docs}` for context left by other sessions in this group.\n",
        name = group.name.replace('"', "'"),
        docs = docs.display(),
        outputs = outputs.display()
    );
    let file = instructions.join("canvas-group.instructions.md");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&file, text)?;
    }
    Ok(Some(instructions))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Doc {
    pub version: u32,
    pub nodes: Vec<Node>,
    pub groups: Vec<Group>,
    pub view: View,
    #[serde(default)]
    pub ui: UiPrefs,
    #[serde(default)]
    pub recent_folders: Vec<RecentFolder>,
    /// Most recently shown terminals, newest first (tile order).
    #[serde(default)]
    pub tiles: Vec<String>,
    #[serde(default)]
    pub selected: Option<String>,
    /// Defaults for new Copilot sessions started from the canvas.
    #[serde(default)]
    pub defaults: LaunchDefaults,
    #[serde(default)]
    pub vscode: VsPrefs,
    /// Stop canvas-started sessions idle (and off screen) for this many minutes
    /// to free memory; 0 = never. They stay resumable.
    #[serde(default = "default_idle_stop")]
    pub idle_stop_minutes: u32,
    /// Set once the idle-stop default moved from 60 to 240 minutes.
    #[serde(default)]
    pub idle_stop_v2: bool,
    #[serde(default)]
    pub mcp: McpPrefs,
    /// Folder of automation manifests (empty: COPILOT_CANVAS_AUTOMATIONS or <home>/automations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automations_dir: Option<String>,
    /// The default automation doctor was added once (removing it is respected).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub doctor_created: bool,
    /// Agent CLI for new sessions on this canvas (`None`: the machine's
    /// setting, else the first installed agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// Sessions unused for this many days are drawn dusty (0: never).
    #[serde(default = "default_dust_days")]
    pub dust_after_days: u32,
}

fn default_dust_days() -> u32 {
    crate::visual::DEFAULT_DUST_DAYS
}

/// MCP server choice for new sessions: the last one used and named presets.
/// Choices are stored as the servers turned *off*, so servers you add to
/// Copilot later start on (Copilot's default) until you turn them off.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct McpPrefs {
    #[serde(default)]
    pub last_off: Vec<String>,
    #[serde(default)]
    pub presets: Vec<McpPreset>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct McpPreset {
    pub name: String,
    pub off: Vec<String>,
}

/// Built-in presets shown before saved ones.
pub const MCP_ALL: &str = "All";
pub const MCP_NONE: &str = "None";

impl McpPrefs {
    /// Saves (or replaces) a preset.
    pub fn save_preset(&mut self, name: &str, off: &[String]) {
        let off = normalize_off(off);
        match self.presets.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name)) {
            Some(preset) => preset.off = off,
            None => self.presets.push(McpPreset { name: name.to_owned(), off }),
        }
    }

    /// Preset names: built-ins first, then saved ones.
    pub fn names(&self) -> Vec<String> {
        [MCP_ALL.to_owned(), MCP_NONE.to_owned()].into_iter().chain(self.presets.iter().map(|p| p.name.clone())).collect()
    }

    /// Servers a preset turns off, among the `known` (available) servers.
    pub fn preset_off(&self, name: &str, known: &[String]) -> Option<Vec<String>> {
        match name {
            MCP_ALL => Some(Vec::new()),
            MCP_NONE => Some(normalize_off(known)),
            _ => self.presets.iter().find(|p| p.name == name).map(|p| normalize_off(&p.off)),
        }
    }

    /// The preset that gives exactly this choice for the `known` servers.
    pub fn matching(&self, off: &[String], known: &[String]) -> Option<String> {
        let current: Vec<&String> = known.iter().filter(|k| off.contains(k)).collect();
        self.names().into_iter().find(|name| {
            let preset = self.preset_off(name, known).unwrap_or_default();
            known.iter().filter(|k| preset.contains(k)).collect::<Vec<_>>() == current
        })
    }
}

pub fn normalize_off(off: &[String]) -> Vec<String> {
    let mut off: Vec<String> = off.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
    off.sort_by_key(|s| s.to_lowercase());
    off.dedup();
    off
}

/// Copilot arguments that keep these MCP servers from starting.
pub fn mcp_off_args(off: &[String]) -> Vec<String> {
    off.iter().flat_map(|name| ["--disable-mcp-server".to_owned(), name.clone()]).collect()
}

fn default_idle_stop() -> u32 {
    240
}

pub const IDLE_STOP_CHOICES: [u32; 5] = [0, 30, 60, 120, 240];

impl Default for Doc {
    fn default() -> Self {
        Self {
            version: 2,
            nodes: Vec::new(),
            groups: Vec::new(),
            view: View { x: -2, y: -1, zoom: 0 },
            ui: UiPrefs::default(),
            recent_folders: Vec::new(),
            tiles: Vec::new(),
            selected: None,
            defaults: LaunchDefaults::default(),
            vscode: VsPrefs::default(),
            idle_stop_minutes: default_idle_stop(),
            idle_stop_v2: true,
            mcp: McpPrefs::default(),
            automations_dir: None,
            doctor_created: false,
            default_agent: None,
            dust_after_days: default_dust_days(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Coarse grid of box rectangles for fast overlap checks.
pub struct SpatialIndex {
    cells: HashMap<(i32, i32), Vec<Rect>>,
}

impl SpatialIndex {
    const CELL_W: i32 = 16;
    const CELL_H: i32 = 8;

    pub fn new(doc: &Doc) -> Self {
        let mut index = Self { cells: HashMap::new() };
        for node in &doc.nodes {
            index.insert(node.rect());
        }
        index
    }

    fn cells(rect: &Rect) -> impl Iterator<Item = (i32, i32)> {
        let (x0, x1) = (rect.x.div_euclid(Self::CELL_W), (rect.x + rect.w - 1).div_euclid(Self::CELL_W));
        let (y0, y1) = (rect.y.div_euclid(Self::CELL_H), (rect.y + rect.h - 1).div_euclid(Self::CELL_H));
        (y0..=y1).flat_map(move |y| (x0..=x1).map(move |x| (x, y)))
    }

    pub fn insert(&mut self, rect: Rect) {
        for cell in Self::cells(&rect) {
            self.cells.entry(cell).or_default().push(rect);
        }
    }

    /// Same answer as `Doc::is_free` (no box intersects `rect`).
    pub fn is_free(&self, rect: &Rect) -> bool {
        !Self::cells(rect).any(|cell| self.cells.get(&cell).is_some_and(|rects| rects.iter().any(|r| r.intersects(rect))))
    }
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
    pub fn contains_rect(&self, other: &Rect) -> bool {
        other.x >= self.x && other.y >= self.y && other.x + other.w <= self.x + self.w && other.y + other.h <= self.y + self.h
    }
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect { x, y, w: (self.x + self.w).max(other.x + other.w) - x, h: (self.y + self.h).max(other.y + other.h) - y }
    }
}

impl Node {
    pub fn rect(&self) -> Rect {
        match self.kind {
            NodeKind::Session | NodeKind::Automation => Rect { x: self.x, y: self.y, w: SESSION_W, h: SESSION_H },
            NodeKind::Agent => Rect { x: self.x, y: self.y, w: AGENT_W, h: AGENT_H },
        }
    }
}

impl Group {
    pub fn rect(&self) -> Rect {
        Rect { x: self.x, y: self.y, w: self.w, h: self.h }
    }
}

impl Doc {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
    pub fn node_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
    pub fn group_mut(&mut self, id: &str) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// Smallest group containing the node's center.
    pub fn group_of(&self, node: &Node) -> Option<&Group> {
        let (cx, cy) = node.rect().center();
        self.group_at(cx, cy)
    }

    /// Smallest group containing the point (where a box centred there belongs).
    pub fn group_at(&self, x: i32, y: i32) -> Option<&Group> {
        self.groups.iter().filter(|g| g.rect().contains(x, y)).min_by_key(|g| (g.w as i64) * (g.h as i64))
    }

    /// Smallest group that strictly contains `group` (for nesting).
    pub fn parent_group(&self, group: &Group) -> Option<&Group> {
        let rect = group.rect();
        self.groups
            .iter()
            .filter(|g| g.id != group.id && g.rect().contains_rect(&rect) && g.rect() != rect)
            .min_by_key(|g| (g.w as i64) * (g.h as i64))
    }

    pub fn members(&self, group_id: &str) -> Vec<String> {
        self.nodes.iter().filter(|n| self.group_of(n).is_some_and(|g| g.id == group_id)).map(|n| n.id.clone()).collect()
    }

    /// Everything that moves with a group: nodes whose center is inside, and nested groups.
    fn group_contents(&self, group_id: &str) -> (Vec<String>, Vec<String>) {
        let Some(group) = self.group(group_id) else { return (Vec::new(), Vec::new()) };
        let rect = group.rect();
        let nodes = self
            .nodes
            .iter()
            .filter(|n| {
                let (cx, cy) = n.rect().center();
                rect.contains(cx, cy)
            })
            .map(|n| n.id.clone())
            .collect();
        let groups = self.groups.iter().filter(|g| g.id != group_id && rect.contains_rect(&g.rect())).map(|g| g.id.clone()).collect();
        (nodes, groups)
    }

    pub fn move_group(&mut self, group_id: &str, dx: i32, dy: i32) {
        let (mut nodes, groups) = self.group_contents(group_id);
        // A stack moves with its parent, wherever its boxes' own positions are.
        let follow: Vec<String> = nodes.iter().filter(|id| self.node(id).is_some_and(|n| n.stacked)).flat_map(|id| crate::visual::stack_members(self, id)).collect();
        nodes.extend(follow);
        for node in self.nodes.iter_mut().filter(|n| nodes.contains(&n.id)) {
            node.x += dx;
            node.y += dy;
        }
        for group in self.groups.iter_mut().filter(|g| g.id == group_id || groups.contains(&g.id)) {
            group.x += dx;
            group.y += dy;
        }
    }

    /// Moves nodes together with their subagent boxes and stacked
    /// subsessions (each at most once).
    pub fn move_nodes(&mut self, ids: &[String], dx: i32, dy: i32) {
        let mut moving: HashSet<String> = ids.iter().cloned().collect();
        for id in ids {
            if self.node(id).is_some_and(|n| n.stacked) {
                moving.extend(crate::visual::stack_members(self, id));
            }
        }
        for node in &self.nodes {
            if node.kind == NodeKind::Agent && node.parent.as_ref().is_some_and(|p| ids.contains(p)) {
                moving.insert(node.id.clone());
            }
        }
        for node in self.nodes.iter_mut().filter(|n| moving.contains(&n.id)) {
            node.x += dx;
            node.y += dy;
        }
    }

    pub fn bounds(&self) -> Option<Rect> {
        let mut rects = self.nodes.iter().map(Node::rect).chain(self.groups.iter().map(Group::rect));
        let first = rects.next()?;
        Some(rects.fold(first, |acc, r| acc.union(&r)))
    }

    pub fn is_free(&self, rect: &Rect, ignore: &str) -> bool {
        !self.nodes.iter().any(|n| n.id != ignore && n.rect().intersects(rect))
    }

    /// First free grid slot scanning right then down from `(x, y)` within `columns`.
    pub fn free_slot(&self, x: i32, y: i32, columns: i32, kind: NodeKind) -> (i32, i32) {
        let (w, h) = match kind {
            NodeKind::Session | NodeKind::Automation => (SESSION_W, SESSION_H),
            NodeKind::Agent => (AGENT_W, AGENT_H),
        };
        for index in 0..10_000 {
            let cx = x + (index % columns) * GRID_X;
            let cy = y + (index / columns) * GRID_Y;
            let rect = Rect { x: cx - 1, y: cy - 1, w: w + 2, h: h + 2 };
            if self.is_free(&rect, "") {
                return (cx, cy);
            }
        }
        (x, y)
    }

    /// Like `agent_slot`, but answers "is this spot free" from a spatial index
    /// (O(nearby boxes) instead of O(all boxes)).
    pub fn agent_slot_in(&self, parent: &Node, index: &SpatialIndex) -> (i32, i32) {
        let x = parent.x + 4;
        let mut y = parent.y + SESSION_H + 1;
        loop {
            let rect = Rect { x, y, w: AGENT_W, h: AGENT_H };
            if index.is_free(&rect) {
                return (x, y);
            }
            y += AGENT_H + 1;
        }
    }

    /// Next placement for a subagent box under its parent, avoiding overlaps.
    #[cfg(test)]
    pub fn agent_slot(&self, parent: &Node) -> (i32, i32) {
        let x = parent.x + 4;
        let mut y = parent.y + SESSION_H + 1;
        loop {
            let rect = Rect { x, y, w: AGENT_W, h: AGENT_H };
            if self.is_free(&rect, "") {
                return (x, y);
            }
            y += AGENT_H + 1;
        }
    }

    /// Creates a group snugly around the given nodes (and their subagents).
    pub fn group_around(&mut self, ids: &[String], name: &str) -> Option<String> {
        let mut rect: Option<Rect> = None;
        for node in &self.nodes {
            let include = ids.contains(&node.id)
                || (node.kind == NodeKind::Agent && node.parent.as_ref().is_some_and(|p| ids.contains(p)));
            if include {
                rect = Some(match rect {
                    Some(r) => r.union(&node.rect()),
                    None => node.rect(),
                });
            }
        }
        let rect = rect?;
        let id = format!("g-{}", uuid::Uuid::new_v4());
        let color = (self.groups.len() % 6) as u8;
        self.groups.push(Group {
            id: id.clone(),
            name: name.to_owned(),
            x: rect.x - 2,
            y: rect.y - 2,
            w: (rect.w + 4).max(GROUP_MIN_W),
            h: (rect.h + 3).max(GROUP_MIN_H),
            color,
            folder: None,
            source: None,
            defaults: None,
        });
        Some(id)
    }

    pub fn fit_group(&mut self, group_id: &str) {
        let members = self.members(group_id);
        let mut rect: Option<Rect> = None;
        for node in self.nodes.iter().filter(|n| members.contains(&n.id)) {
            rect = Some(match rect {
                Some(r) => r.union(&node.rect()),
                None => node.rect(),
            });
        }
        if let (Some(rect), Some(group)) = (rect, self.group_mut(group_id)) {
            group.x = rect.x - 2;
            group.y = rect.y - 2;
            group.w = (rect.w + 4).max(GROUP_MIN_W);
            group.h = (rect.h + 3).max(GROUP_MIN_H);
        }
    }

    pub fn remove_node(&mut self, id: &str) {
        self.nodes.retain(|n| n.id != id && !(n.kind == NodeKind::Agent && n.parent.as_deref() == Some(id)));
        for node in self.nodes.iter_mut().filter(|n| n.parent.as_deref() == Some(id)) {
            // Subsessions outlive their parent's box.
            node.parent = None;
        }
        self.tiles.retain(|t| t != id);
    }

    pub fn note_folder(&mut self, path: &str, now: u64) {
        let key = path.trim_end_matches(['\\', '/']).to_lowercase();
        self.recent_folders.retain(|f| f.path.trim_end_matches(['\\', '/']).to_lowercase() != key);
        self.recent_folders.insert(0, RecentFolder { path: path.to_owned(), used_ms: now });
        self.recent_folders.truncate(30);
    }

    pub fn touch_tile(&mut self, id: &str) {
        self.tiles.retain(|t| t != id);
        self.tiles.insert(0, id.to_owned());
        self.tiles.truncate(16);
    }

    /// Canvases still on the old 60-minute idle-stop default move to 4 hours
    /// (once; any other choice is kept).
    pub fn migrate_idle_stop(&mut self) {
        if !self.idle_stop_v2 {
            if self.idle_stop_minutes == 60 {
                self.idle_stop_minutes = default_idle_stop();
            }
            self.idle_stop_v2 = true;
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == 2, "unsupported canvas version {}", self.version);
        let mut ids = HashSet::new();
        for node in &self.nodes {
            anyhow::ensure!(ids.insert(node.id.as_str()), "duplicate node id {}", node.id);
            anyhow::ensure!(node.x.abs() < 10_000_000 && node.y.abs() < 10_000_000, "node {} is out of range", node.id);
        }
        for group in &self.groups {
            anyhow::ensure!(ids.insert(group.id.as_str()), "duplicate group id {}", group.id);
            anyhow::ensure!(group.w > 0 && group.h > 0, "group {} has no area", group.id);
        }
        Ok(())
    }
}

/// Exclusive, crash-safe ownership of a canvas file.
pub struct Store {
    pub path: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        // Share mode 0: released automatically if this process dies.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .share_mode(0)
            .open(&lock_path)
            .map_err(|e| {
                if e.raw_os_error() == Some(32) {
                    anyhow::anyhow!("this canvas is already open in another Copilot Canvas window")
                } else {
                    anyhow::Error::new(e).context(format!("cannot lock {}", lock_path.display()))
                }
            })?;
        Ok(Self { path: path.to_owned(), _lock: lock })
    }

    pub fn load(&self) -> Result<Option<Doc>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let mut doc: Doc = serde_json::from_str(&text)
                    .with_context(|| format!("{} is not a valid canvas; back it up and repair or remove it", self.path.display()))?;
                doc.validate()?;
                doc.migrate_idle_stop();
                Ok(Some(doc))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", self.path.display())),
        }
    }

    /// Writes `backups/<canvas>-<ms>-<label>.json` next to the canvas, keeping
    /// the newest 30.
    pub fn backup(&self, doc: &Doc, label: &str) -> Result<PathBuf> {
        write_backup(&self.path, doc, label)
    }

    pub fn save(&self, doc: &Doc) -> Result<()> {
        save_doc(&self.path, doc)
    }
}

/// Writes a canvas file atomically (a temporary file renamed over it).
pub fn save_doc(path: &Path, doc: &Doc) -> Result<()> {
    doc.validate()?;
    let text = serde_json::to_string_pretty(doc)?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, text)?;
    let mut last = None;
    // Antivirus/indexers can briefly hold the target open on Windows.
    for attempt in 0..5 {
        match std::fs::rename(&temp, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20 << attempt));
            }
        }
    }
    let _ = std::fs::remove_file(&temp);
    Err(last.expect("at least one attempt")).context("cannot replace the canvas file")
}

fn backup_dir(canvas: &Path) -> (PathBuf, String) {
    let dir = canvas.parent().unwrap_or(Path::new(".")).join("backups");
    let stem = canvas.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "canvas".into());
    (dir, stem)
}

/// Writes `backups/<canvas>-<ms>-<label>.json` next to the canvas file,
/// keeping the newest 30.
pub fn write_backup(canvas: &Path, doc: &Doc, label: &str) -> Result<PathBuf> {
    anyhow::ensure!(!label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "invalid backup label");
    let (dir, stem) = backup_dir(canvas);
    std::fs::create_dir_all(&dir)?;
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let file = dir.join(format!("{stem}-{ms:013}-{label}.json"));
    std::fs::write(&file, serde_json::to_string_pretty(doc)?)?;
    let mut existing = list_backups(canvas)?;
    while existing.len() > 30 {
        let _ = std::fs::remove_file(existing.remove(0));
    }
    Ok(file)
}

/// Backups of a canvas file, oldest first.
pub fn list_backups(canvas: &Path) -> Result<Vec<PathBuf>> {
    let (dir, stem) = backup_dir(canvas);
    let prefix = format!("{stem}-");
    let mut existing: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix) && n.to_string_lossy().ends_with(".json")))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    existing.sort();
    Ok(existing)
}

/// The `--demo` canvas: PowerShell sessions in `cwd`, a subagent and a group.
pub fn demo_doc(cwd: &str) -> Doc {
    let shell = Some(vec!["powershell.exe".to_owned(), "-NoLogo".to_owned(), "-NoProfile".to_owned()]);
    let session = |id: &str, name: &str, x: i32, y: i32| Node {
        id: id.into(),
        kind: NodeKind::Session,
        name: name.into(),
        x,
        y,
        cwd: Some(cwd.to_owned()),
        parent: None,
        agent_key: None,
        source: Some("demo".into()),
        command: shell.clone(),
        fresh: true,
        automation: None,
        synced_name: None,
        reviewed_ms: None,
        mcp_off: None,
        passive: None,
        no_messages: false,
        agent: None,
        agent_session: None, last_used_ms: None, stacked: false, model: None,
    };
    let mut doc = Doc::default();
    doc.nodes.push(session("demo-main", "Build session canvas", 2, 2));
    doc.nodes.push(Node {
        kind: NodeKind::Agent,
        parent: Some("demo-main".into()),
        agent_key: Some("explore".into()),
        command: None,
        ..session("demo-main:explore", "Explore terminal APIs", 6, 7)
    });
    doc.nodes.push(session("demo-review", "Review implementation", 36, 2));
    let mut sub = session("demo-sub", "Write tests", 36, 9);
    sub.parent = Some("demo-review".into());
    doc.nodes.push(sub);
    doc.groups.push(Group { id: "g-demo".into(), name: "Canvas project".into(), x: 0, y: 0, w: 70, h: 15, color: 0, folder: None, source: None, defaults: None });
    doc.view.x = -2;
    doc.view.y = -1;
    doc.selected = Some("demo-main".into());
    doc
}

/// Imports the Node prototype's `layout.json` (version 1) if present.
pub fn migrate_v1(path: &Path) -> Option<Doc> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("version")?.as_u64()? != 1 {
        return None;
    }
    let mut doc = Doc::default();
    let mut agent_parents = HashMap::new();
    for node in value.get("nodes")?.as_array()? {
        let id = node.get("id")?.as_str()?.to_owned();
        let kind = match node.get("kind")?.as_str()? {
            "session" => NodeKind::Session,
            "agent" => NodeKind::Agent,
            _ => continue,
        };
        let parent = node.get("parentId").and_then(|p| p.as_str()).map(str::to_owned);
        if kind == NodeKind::Agent {
            agent_parents.insert(id.clone(), parent.clone());
        }
        doc.nodes.push(Node {
            id,
            kind,
            name: node.get("name").and_then(|n| n.as_str()).unwrap_or("Session").to_owned(),
            x: node.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            y: node.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            cwd: node.get("cwd").and_then(|v| v.as_str()).map(str::to_owned),
            parent,
            agent_key: node.get("agentKey").and_then(|v| v.as_str()).map(str::to_owned),
            source: Some("prototype".into()),
            command: None,
            fresh: false,
            automation: None,
            synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, stacked: false, model: None,
        });
    }
    // The prototype stacked boxes 7 rows apart; v2 boxes are 4 tall, so keep positions.
    doc.selected = value.get("selectedId").and_then(|v| v.as_str()).map(str::to_owned);
    doc.validate().ok()?;
    Some(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, x: i32, y: i32) -> Node {
        Node { id: id.into(), kind: NodeKind::Session, name: id.into(), x, y, cwd: None, parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false, agent: None, agent_session: None, last_used_ms: None, stacked: false, model: None }
    }

    fn agent(id: &str, parent: &str, x: i32, y: i32) -> Node {
        Node { kind: NodeKind::Agent, parent: Some(parent.into()), ..session(id, x, y) }
    }

    #[test]
    fn agent_fields_default_for_old_canvases_and_are_omitted_when_unset() {
        let old: Doc = serde_json::from_str(r#"{"version":2,"nodes":[{"id":"a","kind":"session","name":"A","x":0,"y":0}],"groups":[],"view":{"x":0,"y":0}}"#).unwrap();
        assert_eq!((old.nodes[0].agent.as_deref(), old.nodes[0].agent_session.as_deref(), old.default_agent.as_deref()), (None, None, None));
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("agent_session") && !text.contains("default_agent"), "{text}");
        let node = serde_json::to_value(&old.nodes[0]).unwrap();
        assert!(node.get("agent").is_none(), "{node}");
        let mut doc = old;
        doc.nodes[0].agent = Some("opencode".into());
        doc.nodes[0].agent_session = Some("ses_x".into());
        doc.default_agent = Some("claude".into());
        let back: Doc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn idle_stop_moves_from_the_old_default_to_four_hours_once() {
        let old: Doc = serde_json::from_str(r#"{"version":2,"nodes":[],"groups":[],"view":{"x":0,"y":0,"zoom":0},"idle_stop_minutes":60}"#).unwrap();
        let mut doc = old.clone();
        doc.migrate_idle_stop();
        assert_eq!(doc.idle_stop_minutes, 240);
        doc.idle_stop_minutes = 60;
        doc.migrate_idle_stop();
        assert_eq!(doc.idle_stop_minutes, 60, "a later choice of 60 is kept");
        let mut never = Doc { idle_stop_minutes: 0, idle_stop_v2: false, ..old };
        never.migrate_idle_stop();
        assert_eq!(never.idle_stop_minutes, 0, "other choices are kept");
    }

    #[test]
    fn mcp_presets_match_and_new_servers_start_on() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut prefs = McpPrefs::default();
        let known = s(&["azure", "icm", "kusto"]);
        assert_eq!(prefs.matching(&[], &known).as_deref(), Some(MCP_ALL));
        assert_eq!(prefs.matching(&known, &known).as_deref(), Some(MCP_NONE));
        prefs.save_preset("Investigation", &s(&["azure"]));
        assert_eq!(prefs.matching(&s(&["azure"]), &known).as_deref(), Some("Investigation"));
        assert_eq!(prefs.matching(&s(&["icm"]), &known), None);
        // A server added later is on in the saved preset; one removed from Copilot is ignored.
        let later = s(&["azure", "icm", "kusto", "newsrv"]);
        assert_eq!(prefs.preset_off("Investigation", &later).unwrap(), s(&["azure"]));
        assert_eq!(prefs.matching(&s(&["azure", "gone"]), &later).as_deref(), Some("Investigation"));
        prefs.save_preset("investigation", &s(&["icm", "icm"]));
        assert_eq!(prefs.presets.len(), 1, "same name (any case) replaces");
        assert_eq!(prefs.presets[0].off, s(&["icm"]));
        assert_eq!(mcp_off_args(&s(&["a", "b"])), s(&["--disable-mcp-server", "a", "--disable-mcp-server", "b"]));
    }

    #[test]
    fn spatial_index_agrees_with_the_full_scan() {
        let mut doc = Doc::default();
        for i in 0..60 {
            let mut n = session(&format!("n{i}"), (i * 37 % 400) - 100, (i * 23 % 120) - 30);
            if i % 3 == 0 {
                n.kind = NodeKind::Agent;
            }
            doc.nodes.push(n);
        }
        let index = SpatialIndex::new(&doc);
        for x in (-120..420).step_by(7) {
            for y in (-40..130).step_by(3) {
                let rect = Rect { x, y, w: AGENT_W, h: AGENT_H };
                assert_eq!(index.is_free(&rect), doc.is_free(&rect, ""), "{rect:?}");
            }
        }
        let parent = doc.nodes[1].clone();
        assert_eq!(doc.agent_slot_in(&parent, &index), doc.agent_slot(&parent));
    }

    #[test]
    fn launch_defaults_merge_and_render_cli_flags() {
        let models = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let base = LaunchDefaults { models: models(&[("copilot", "gpt-x"), ("claude", "opus")]), reasoning_effort: Some("high".into()), add_dirs: vec![r"C:\shared".into()], ..Default::default() };
        let group = LaunchDefaults { models: models(&[("copilot", "claude-y"), ("opencode", "")]), mode: Some("autopilot".into()), add_dirs: vec![r"c:\SHARED".into(), r"D:\docs".into()], ..Default::default() };
        let merged = base.merged(Some(&group));
        assert_eq!(merged.args(), ["--reasoning-effort", "high", "--mode", "autopilot", "--add-dir", r"C:\shared", "--add-dir", r"D:\docs"]);
        assert_eq!((merged.model(AgentKind::Copilot), merged.model(AgentKind::OpenCode), merged.model(AgentKind::Claude)), (Some("claude-y"), Some(""), Some("opus")), "the group wins per agent");
        assert_eq!(merged.summary(), "GitHub Copilot claude-y · OpenCode (its default) · Claude Code opus · --reasoning-effort high --mode autopilot --add-dir C:\\shared --add-dir D:\\docs");
        assert!(LaunchDefaults::default().is_empty());
        assert!(!LaunchDefaults { models: models(&[("claude", "opus")]), ..Default::default() }.is_empty(), "a model alone is an override");
        assert_eq!(base.merged(None), base);
    }

    #[test]
    fn the_old_single_model_moves_to_the_agents_its_id_fits() {
        let old: LaunchDefaults = serde_json::from_str(r#"{"model":"github-copilot/claude-opus-5.5","reasoning_effort":"high"}"#).unwrap();
        assert_eq!(old.models, BTreeMap::from([("opencode".to_owned(), "github-copilot/claude-opus-5.5".to_owned())]));
        assert_eq!(old.reasoning_effort.as_deref(), Some("high"));
        let bare: LaunchDefaults = serde_json::from_str(r#"{"model":" opus ","models":{"claude":"sonnet"}}"#).unwrap();
        assert_eq!((bare.model(AgentKind::Copilot), bare.model(AgentKind::Claude), bare.model(AgentKind::OpenCode)), (Some("opus"), Some("sonnet"), None), "per-agent entries win");
        let blank: LaunchDefaults = serde_json::from_str(r#"{"model":null,"reasoning_effort":null,"mode":null,"agent":null,"context":null,"add_dirs":[],"permissions":null}"#).unwrap();
        assert!(blank.is_empty(), "canvases saved before per-agent models load unchanged");
        // Written back without the old field; read again unchanged.
        let text = serde_json::to_string(&bare).unwrap();
        assert!(!text.contains("\"model\""), "{text}");
        assert_eq!(serde_json::from_str::<LaunchDefaults>(&text).unwrap(), bare);
        let mut set = LaunchDefaults::default();
        set.set_model(AgentKind::OpenCode, Some(" x/y "));
        set.set_model(AgentKind::Claude, Some(""));
        assert_eq!((set.model(AgentKind::OpenCode), set.model(AgentKind::Claude)), (Some("x/y"), Some("")));
        set.set_model(AgentKind::OpenCode, None);
        assert_eq!(set.model(AgentKind::OpenCode), None);
    }

    #[test]
    fn a_whole_canvas_round_trips_models_per_agent_and_per_session() {
        let dir = std::env::temp_dir().join(format!("cc-models-{}", uuid::Uuid::new_v4()));
        let path = dir.join("canvas.json");
        std::fs::create_dir_all(&dir).unwrap();
        // A canvas from before: one model for every agent, in the canvas and a group.
        std::fs::write(
            &path,
            r#"{"version":2,"nodes":[{"id":"a","kind":"session","name":"A","x":0,"y":0,"agent":"claude"}],"groups":[{"id":"g","name":"G","x":0,"y":0,"w":40,"h":10,"defaults":{"model":"opencode/big-pickle"}}],"view":{"x":0,"y":0},"defaults":{"model":"claude-opus-4.5"}}"#,
        )
        .unwrap();
        let store = Store::open(&path).unwrap();
        let mut doc = store.load().unwrap().unwrap();
        assert_eq!((doc.defaults.model(AgentKind::Copilot), doc.defaults.model(AgentKind::Claude), doc.defaults.model(AgentKind::OpenCode)), (Some("claude-opus-4.5"), Some("claude-opus-4.5"), None));
        assert_eq!(doc.groups[0].defaults.as_ref().unwrap().model(AgentKind::OpenCode), Some("opencode/big-pickle"));
        assert_eq!(doc.nodes[0].model, None, "not recorded for older boxes");
        doc.defaults.set_model(AgentKind::OpenCode, Some("github-copilot/gpt-6-sol"));
        doc.nodes[0].model = Some("claude-opus-5-5".into());
        store.save(&doc).unwrap();
        assert_eq!(store.load().unwrap(), Some(doc));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permissions_default_to_allow_all_and_groups_can_ask() {
        let canvas = LaunchDefaults::default();
        assert_eq!(canvas.permission_args(), ["--allow-all"], "unset means allow-all");
        assert!(canvas.args().is_empty(), "not part of the fresh-session flags");
        let asking = LaunchDefaults { permissions: Some("ask".into()), ..Default::default() };
        assert!(!asking.is_empty(), "a group override is kept");
        assert!(canvas.merged(Some(&asking)).permission_args().is_empty());
        let unset_group = LaunchDefaults { models: BTreeMap::from([("copilot".to_owned(), "m".to_owned())]), ..Default::default() };
        assert_eq!(asking.merged(Some(&unset_group)).permission_args(), Vec::<String>::new(), "groups inherit the canvas setting");
        // Canvases saved before this setting existed load as allow-all.
        let old: LaunchDefaults = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert_eq!(old.permission_args(), ["--allow-all"]);
    }

    #[test]
    fn group_folders_and_instructions_are_created() {
        let dir = std::env::temp_dir().join(format!("cc-group-{}", uuid::Uuid::new_v4()));
        let mut group = Group { id: "g".into(), name: "Release \"x\"".into(), x: 0, y: 0, w: 10, h: 10, color: 0, folder: None, source: None, defaults: None };
        assert_eq!(prepare_group_folders(&group).unwrap(), None, "no folder chosen yet");
        group.folder = Some(dir.display().to_string());
        let instructions = prepare_group_folders(&group).unwrap().unwrap();
        assert!(dir.join(AI_DOCS).is_dir() && dir.join(HUMAN_OUTPUTS).is_dir());
        let text = std::fs::read_to_string(instructions.join("canvas-group.instructions.md")).unwrap();
        assert!(text.starts_with("---\napplyTo: '**'"));
        assert!(text.contains(&dir.join(AI_DOCS).display().to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn group_membership_is_geometric_and_nested_groups_move_together() {
        let mut doc = Doc { nodes: vec![session("a", 0, 0), session("b", 40, 0), agent("a1", "a", 4, 5)], ..Doc::default() };
        let outer = doc.group_around(&["a".into()], "Outer").unwrap();
        doc.group_mut(&outer).unwrap().w = 100;
        doc.group_mut(&outer).unwrap().h = 30;
        let inner = doc.group_around(&["b".into()], "Inner").unwrap();
        assert_eq!(doc.group_of(doc.node("b").unwrap()).unwrap().id, inner, "smallest container wins");
        assert_eq!(doc.group_of(doc.node("a1").unwrap()).unwrap().id, outer);
        assert_eq!(doc.parent_group(doc.group(&inner).unwrap()).unwrap().id, outer);
        doc.move_group(&outer, 10, 5);
        assert_eq!((doc.node("a").unwrap().x, doc.node("b").unwrap().y), (10, 5));
        assert_eq!(doc.node("a1").unwrap().x, 14);
        assert_eq!(doc.group(&inner).unwrap().x, 40 - 2 + 10);
    }

    #[test]
    fn moving_a_session_moves_its_agents_but_not_subsessions() {
        let mut doc = Doc::default();
        let mut sub = session("s", 0, 20);
        sub.parent = Some("a".into());
        doc.nodes = vec![session("a", 0, 0), agent("a1", "a", 4, 5), sub];
        doc.move_nodes(&["a".into()], 3, 1);
        assert_eq!((doc.node("a1").unwrap().x, doc.node("a1").unwrap().y), (7, 6));
        assert_eq!(doc.node("s").unwrap().x, 0);
    }

    #[test]
    fn free_slots_and_agent_slots_avoid_overlap() {
        let mut doc = Doc { nodes: vec![session("a", 0, 0)], ..Doc::default() };
        let (x, y) = doc.free_slot(0, 0, 3, NodeKind::Session);
        assert_eq!((x, y), (GRID_X, 0));
        let parent = doc.node("a").unwrap().clone();
        let first = doc.agent_slot(&parent);
        doc.nodes.push(agent("x", "a", first.0, first.1));
        let second = doc.agent_slot(&parent);
        assert!(second.1 > first.1);
    }

    #[test]
    fn removing_a_session_drops_its_agents_and_orphans_subsessions() {
        let mut doc = Doc::default();
        let mut sub = session("s", 0, 20);
        sub.parent = Some("a".into());
        doc.nodes = vec![session("a", 0, 0), agent("a1", "a", 4, 5), sub];
        doc.tiles = vec!["a".into()];
        doc.remove_node("a");
        assert_eq!(doc.nodes.len(), 1);
        assert_eq!(doc.nodes[0].parent, None);
        assert!(doc.tiles.is_empty());
    }

    #[test]
    fn recent_folders_are_deduplicated_case_insensitively() {
        let mut doc = Doc::default();
        doc.note_folder(r"C:\Repo", 1);
        doc.note_folder(r"c:\repo\", 2);
        doc.note_folder(r"D:\Other", 3);
        assert_eq!(doc.recent_folders.len(), 2);
        assert_eq!(doc.recent_folders[1].path, r"c:\repo\");
    }

    #[test]
    fn store_round_trips_locks_and_rejects_corruption() {
        let dir = std::env::temp_dir().join(format!("cc-doc-{}", uuid::Uuid::new_v4()));
        let path = dir.join("canvas.json");
        {
            let store = Store::open(&path).unwrap();
            assert!(Store::open(&path).is_err(), "second writer refused");
            assert_eq!(store.load().unwrap(), None);
            let mut doc = Doc::default();
            doc.nodes.push(session("a", -5, 7));
            store.save(&doc).unwrap();
            assert_eq!(store.load().unwrap(), Some(doc));
            std::fs::write(&path, "{broken").unwrap();
            assert!(store.load().is_err());
        }
        assert!(Store::open(&path).is_ok(), "lock released on drop");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backups_are_listed_oldest_first_and_capped() {
        let dir = std::env::temp_dir().join(format!("cc-backup-{}", uuid::Uuid::new_v4()));
        let path = dir.join("canvas.json");
        assert!(list_backups(&path).unwrap().is_empty());
        let doc = demo_doc(r"C:\work");
        let first = write_backup(&path, &doc, "before-vscode-sync").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = write_backup(&path, &Doc::default(), "before-undo").unwrap();
        assert_eq!(list_backups(&path).unwrap(), vec![first.clone(), second]);
        let back: Doc = serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(back, doc);
        assert!(write_backup(&path, &doc, "../escape").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_prototype_layouts() {
        let dir = std::env::temp_dir().join(format!("cc-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layout.json");
        std::fs::write(
            &path,
            r#"{"version":1,"nodes":[{"id":"s1","kind":"session","name":"One","x":2,"y":3,"cwd":"C:\\r"},
               {"id":"s1:a","kind":"agent","name":"Explorer","parentId":"s1","agentKey":"a","x":7,"y":10}],
               "viewport":{"x":0,"y":0},"selectedId":"s1","tiled":false}"#,
        )
        .unwrap();
        let doc = migrate_v1(&path).unwrap();
        assert_eq!(doc.nodes.len(), 2);
        assert_eq!(doc.nodes[1].parent.as_deref(), Some("s1"));
        assert_eq!(doc.nodes[1].agent_key.as_deref(), Some("a"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
